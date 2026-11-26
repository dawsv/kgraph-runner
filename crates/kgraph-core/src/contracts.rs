//! JSON Schema assertions for workflow outputs and ordered reductions for splitter results.

use crate::VerifyOutcome;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaViolation {
    pub pointer: String,
    pub message: String,
}

fn child_pointer(parent: &str, key: &str) -> String {
    format!("{parent}/{}", key.replace('~', "~0").replace('/', "~1"))
}

fn integer(n: &Number) -> Option<i128> {
    n.as_i64()
        .map(i128::from)
        .or_else(|| n.as_u64().map(i128::from))
}

fn compare_numbers(a: &Number, b: &Number) -> Ordering {
    match (integer(a), integer(b)) {
        (Some(i), Some(j)) => i.cmp(&j),
        (Some(i), None) => {
            let f = b.as_f64().unwrap();
            i.cmp(&(f as i128))
                .then_with(|| 0.0_f64.partial_cmp(&f.fract()).unwrap())
        }
        (None, Some(_)) => compare_numbers(b, a).reverse(),
        (None, None) => a.as_f64().partial_cmp(&b.as_f64()).unwrap(),
    }
}

// JSON Schema considers 1 and 1.0 equal, including inside arrays and objects.
fn equality_key(value: &Value) -> Value {
    match value {
        Value::Number(n) if n.is_f64() => {
            let f = n.as_f64().unwrap();
            if f.fract() == 0.0 {
                if let Some(n) = Number::from_i128(f as i128) {
                    return Value::Number(n);
                }
            }
            value.clone()
        }
        Value::Array(a) => Value::Array(a.iter().map(equality_key).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| (k.clone(), equality_key(v)))
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn matches_type(v: &Value, name: &str) -> bool {
    match name {
        "null" => v.is_null(),
        "boolean" => v.is_boolean(),
        "number" => v.is_number(),
        "integer" => v
            .as_number()
            .is_some_and(|n| integer(n).is_some() || n.as_f64().unwrap().fract() == 0.0),
        "string" => v.is_string(),
        "array" => v.is_array(),
        "object" => v.is_object(),
        _ => false,
    }
}

fn violation(out: &mut Vec<SchemaViolation>, ptr: &str, msg: impl Into<String>) {
    out.push(SchemaViolation {
        pointer: ptr.into(),
        message: msg.into(),
    });
}

fn check_size(
    size: usize,
    schema: &Value,
    min: &str,
    max: &str,
    ptr: &str,
    out: &mut Vec<SchemaViolation>,
) {
    for (key, lower) in [(min, true), (max, false)] {
        if let Some(bound) = schema.get(key) {
            match bound.as_number() {
                Some(b) if matches_type(bound, "integer") && b.as_f64().unwrap() >= 0.0 => {
                    let ordering = compare_numbers(&Number::from(size), b);
                    if (lower && ordering.is_lt()) || (!lower && ordering.is_gt()) {
                        violation(out, ptr, format!("{key} is {b}, found {size}"));
                    }
                }
                _ => violation(out, ptr, format!("invalid {key}")),
            }
        }
    }
}

#[derive(Default)]
struct Validator<'a> {
    patterns: HashMap<&'a str, Result<Regex, regex::Error>>,
}

impl<'a> Validator<'a> {
    fn check(&mut self, v: &Value, s: &'a Value, ptr: &str, out: &mut Vec<SchemaViolation>) {
        if !s.is_object() {
            if s != &Value::Bool(true) {
                violation(
                    out,
                    ptr,
                    if s == &Value::Bool(false) {
                        "false schema"
                    } else {
                        "expected an object or boolean schema"
                    },
                );
            }
            return;
        }
        if let Some(t) = s.get("type") {
            let matches = t.as_str().is_some_and(|n| matches_type(v, n))
                || t.as_array().is_some_and(|ts| {
                    ts.iter()
                        .any(|n| n.as_str().is_some_and(|n| matches_type(v, n)))
                });
            if !matches {
                violation(out, ptr, format!("expected type {t}"));
            }
        }
        if let Some(expected) = s.get("const") {
            if equality_key(v) != equality_key(expected) {
                violation(out, ptr, "value differs from const");
            }
        }
        if let Some(choices) = s.get("enum") {
            let key = equality_key(v);
            if !choices
                .as_array()
                .is_some_and(|a| a.iter().any(|c| equality_key(c) == key))
            {
                violation(out, ptr, "value is outside enum");
            }
        }
        match v {
            Value::String(text) => {
                check_size(text.chars().count(), s, "minLength", "maxLength", ptr, out);
                if let Some(pattern) = s.get("pattern") {
                    match pattern
                        .as_str()
                        .map(|p| self.patterns.entry(p).or_insert_with(|| Regex::new(p)))
                    {
                        Some(Ok(re)) if !re.is_match(text) => {
                            violation(out, ptr, "string does not match pattern")
                        }
                        Some(Ok(_)) => (),
                        _ => violation(out, ptr, "invalid pattern"),
                    }
                }
            }
            Value::Number(n) => {
                for (key, allowed) in [
                    ("minimum", &[Ordering::Equal, Ordering::Greater][..]),
                    ("maximum", &[Ordering::Equal, Ordering::Less][..]),
                    ("exclusiveMinimum", &[Ordering::Greater][..]),
                    ("exclusiveMaximum", &[Ordering::Less][..]),
                ] {
                    if let Some(b) = s.get(key) {
                        if !b
                            .as_number()
                            .is_some_and(|b| allowed.contains(&compare_numbers(n, b)))
                        {
                            violation(out, ptr, format!("{key} constraint {b} failed"));
                        }
                    }
                }
            }
            Value::Array(items) => {
                check_size(items.len(), s, "minItems", "maxItems", ptr, out);
                let unique = s.get("uniqueItems") == Some(&Value::Bool(true));
                let mut seen = HashSet::new();
                for (i, item) in items.iter().enumerate() {
                    let child = format!("{ptr}/{i}");
                    if unique && !seen.insert(equality_key(item)) {
                        violation(out, &child, "duplicate array item");
                    }
                    if let Some(sub) = s.get("items") {
                        self.check(item, sub, &child, out);
                    }
                }
            }
            Value::Object(obj) => {
                check_size(obj.len(), s, "minProperties", "maxProperties", ptr, out);
                if let Some(keys) = s.get("required").and_then(Value::as_array) {
                    for key in keys.iter().filter_map(Value::as_str) {
                        if !obj.contains_key(key) {
                            violation(
                                out,
                                &child_pointer(ptr, key),
                                "required property is missing",
                            );
                        }
                    }
                }
                // Choose the same first failing property regardless of JSON insertion order.
                let mut keys: Vec<_> = obj.keys().collect();
                keys.sort_unstable();
                for key in keys {
                    let child = child_pointer(ptr, key);
                    if let Some(sub) = s.get("properties").and_then(|p| p.get(key)) {
                        self.check(&obj[key], sub, &child, out);
                    } else if let Some(sub) = s.get("additionalProperties") {
                        if sub == &Value::Bool(false) {
                            violation(out, &child, "unexpected additional property");
                        } else {
                            self.check(&obj[key], sub, &child, out);
                        }
                    }
                }
            }
            _ => (),
        }
        for name in ["allOf", "anyOf", "oneOf"] {
            let Some(branches) = s.get(name) else {
                continue;
            };
            let Some(branches) = branches.as_array() else {
                violation(out, ptr, format!("invalid {name}"));
                continue;
            };
            let mut matches = 0;
            for branch in branches {
                if name == "allOf" {
                    self.check(v, branch, ptr, out);
                } else {
                    let mut errors = Vec::new();
                    self.check(v, branch, ptr, &mut errors);
                    if errors.is_empty() {
                        matches += 1;
                        if name == "anyOf" {
                            break;
                        }
                    }
                }
            }
            if (name == "anyOf" && matches == 0) || (name == "oneOf" && matches != 1) {
                violation(out, ptr, format!("{name} matched {matches} schemas"));
            }
        }
    }
}

/// Validate the supported assertion keywords. Unknown keywords are ignored.
pub fn validate_json_schema(instance: &Value, schema: &Value) -> Result<(), Vec<SchemaViolation>> {
    let mut errors = Vec::new();
    Validator::default().check(instance, schema, "", &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

pub fn verify_json_value_schema(value: &Value, schema: &Value) -> VerifyOutcome {
    match validate_json_schema(value, schema) {
        Ok(()) => VerifyOutcome::pass("Schema valid"),
        Err(errors) => {
            let first = &errors[0];
            VerifyOutcome::fail(
                format!("schema_violation:{}", first.pointer),
                format!(
                    "{} at {:?} ({} violations)",
                    first.message,
                    first.pointer,
                    errors.len()
                ),
            )
        }
    }
}

pub(crate) fn open_regular(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected regular file",
        ));
    }
    Ok(file)
}

pub fn verify_json_file_schema(path: &Path, schema: &Value) -> VerifyOutcome {
    const LIMIT: u64 = 16 << 20;
    let file = match open_regular(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return VerifyOutcome::fail("missing_output", path.display().to_string())
        }
        Err(e) => return VerifyOutcome::fail("invalid_json", e.to_string()),
    };
    let mut bytes = Vec::new();
    if let Err(e) = file.take(LIMIT + 1).read_to_end(&mut bytes) {
        return VerifyOutcome::fail("invalid_json", e.to_string());
    }
    if bytes.len() > LIMIT as usize {
        return VerifyOutcome::fail("invalid_json", "JSON output exceeds 16 MiB");
    }
    match serde_json::from_slice(&bytes) {
        Ok(v) => verify_json_value_schema(&v, schema),
        Err(e) => VerifyOutcome::fail("invalid_json", e.to_string()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReducerKind {
    CollectList,
    MergeObject,
    Sum,
    Concat,
    Last,
    First,
    UniqueList,
}

impl ReducerKind {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "collect_list" => Some(Self::CollectList),
            "merge_object" => Some(Self::MergeObject),
            "sum" => Some(Self::Sum),
            "concat" => Some(Self::Concat),
            "last" => Some(Self::Last),
            "first" => Some(Self::First),
            "unique_list" => Some(Self::UniqueList),
            _ => None,
        }
    }
}

/// Inputs must be in splitter order. MergeObject gives later keys precedence,
/// Concat joins homogeneous strings or flattens arrays by one level, and
/// UniqueList retains the first occurrence of each input. Empty First/Last return null.
pub fn apply_reducer(kind: ReducerKind, values: &[Value]) -> Result<Value, String> {
    match kind {
        ReducerKind::First => Ok(values.first().cloned().unwrap_or(Value::Null)),
        ReducerKind::Last => Ok(values.last().cloned().unwrap_or(Value::Null)),
        ReducerKind::CollectList => Ok(Value::Array(values.to_vec())),
        ReducerKind::UniqueList => {
            let mut seen = HashSet::new();
            Ok(Value::Array(
                values
                    .iter()
                    .filter(|v| seen.insert(equality_key(v)))
                    .cloned()
                    .collect(),
            ))
        }
        ReducerKind::MergeObject => {
            let mut merged = Map::new();
            for v in values {
                merged.extend(
                    v.as_object()
                        .ok_or("merge_object requires objects")?
                        .clone(),
                );
            }
            Ok(Value::Object(merged))
        }
        ReducerKind::Concat if values.first().is_some_and(Value::is_array) => {
            let mut result = Vec::new();
            for v in values {
                result.extend_from_slice(v.as_array().ok_or("concat requires homogeneous arrays")?);
            }
            Ok(Value::Array(result))
        }
        ReducerKind::Concat => {
            let mut result = String::new();
            for v in values {
                result.push_str(v.as_str().ok_or("concat requires homogeneous strings")?);
            }
            Ok(result.into())
        }
        ReducerKind::Sum => {
            let mut exact = 0i128;
            let mut float = None;
            for v in values {
                let n = v.as_number().ok_or("sum requires numbers")?;
                if let Some(acc) = &mut float {
                    *acc += n.as_f64().unwrap();
                } else if let Some(i) = integer(n) {
                    exact = exact.checked_add(i).ok_or("sum overflow")?;
                } else {
                    float = Some(exact as f64 + n.as_f64().unwrap());
                }
            }
            float
                .map_or_else(|| Number::from_i128(exact), Number::from_f64)
                .map(Value::Number)
                .ok_or_else(|| "sum cannot be represented as a JSON number".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn rejects(v: Value, schema: Value, pointer: &str) {
        let e = validate_json_schema(&v, &schema).unwrap_err();
        assert!(
            e.iter().any(|e| e.pointer == pointer),
            "{v} against {schema}: {e:?}"
        );
    }

    #[test]
    fn scalar_and_union_types() {
        for (v, t) in [
            (Value::Null, "null"),
            (json!(false), "boolean"),
            (json!(2), "number"),
            (json!(3.0), "integer"),
            (json!("s"), "string"),
            (json!([]), "array"),
            (json!({}), "object"),
        ] {
            assert!(validate_json_schema(&v, &json!({"type":t})).is_ok());
            rejects(v, json!({"type":"unknown"}), "");
        }
        let union = json!({"type":["null","integer"]});
        assert!(validate_json_schema(&json!(null), &union).is_ok());
        assert!(validate_json_schema(&json!(42), &union).is_ok());
        rejects(json!(1.5), union, "");
    }

    #[test]
    fn const_enum_and_structural_numeric_equality() {
        assert!(validate_json_schema(
            &json!({"a":[1],"b":2}),
            &json!({"const":{"b":2.0,"a":[1.0]}})
        )
        .is_ok());
        rejects(json!(1), json!({"const":2}), "");
        assert!(validate_json_schema(&json!(2.0), &json!({"enum":[1,2]})).is_ok());
        rejects(Value::Null, json!({"enum":[false,0]}), "");
    }

    #[test]
    fn unicode_string_bounds_and_patterns() {
        let s = json!({"minLength":2.0,"maxLength":2,"pattern":"é.$"});
        assert!(validate_json_schema(&json!("é🎯"), &s).is_ok());
        for v in ["é", "éab", "ab"] {
            rejects(json!(v), s.clone(), "");
        }
        rejects(json!("x"), json!({"pattern":"["}), "");
        assert!(validate_json_schema(&json!(1), &s).is_ok());
    }

    #[test]
    fn number_boundaries_preserve_large_integer_precision() {
        assert!(validate_json_schema(&json!(2), &json!({"minimum":2,"maximum":2})).is_ok());
        for (v, s) in [
            (json!(1), json!({"minimum":2})),
            (json!(3), json!({"maximum":2})),
            (json!(2), json!({"exclusiveMinimum":2})),
            (json!(2), json!({"exclusiveMaximum":2})),
            (json!(-1), json!({"maximum":-1.5})),
            (
                json!(9007199254740993_u64),
                json!({"maximum":9007199254740992.0}),
            ),
            (json!(u64::MAX), json!({"exclusiveMaximum":u64::MAX})),
        ] {
            rejects(v, s, "");
        }
        assert!(validate_json_schema(
            &json!(u64::MAX),
            &json!({"exclusiveMaximum":18446744073709551616.0_f64})
        )
        .is_ok());
    }

    #[test]
    fn arrays_check_bounds_items_and_uniqueness() {
        let s = json!({"minItems":1,"maxItems":2,"items":{"type":"number"},"uniqueItems":true});
        assert!(validate_json_schema(&json!([1, 2]), &s).is_ok());
        rejects(json!([]), s.clone(), "");
        rejects(json!([1, 2, 3]), s.clone(), "");
        rejects(json!([1, "x"]), s.clone(), "/1");
        rejects(json!([1, 1.0]), s, "/1");
        rejects(
            json!([{"b":1,"a":2},{"a":2,"b":1}]),
            json!({"uniqueItems":true}),
            "/1",
        );
    }

    #[test]
    fn object_bounds_and_additional_properties() {
        let s = json!({"required":["name"],"minProperties":1,"maxProperties":2,
            "properties":{"name":{"type":"string"}},"additionalProperties":{"type":"boolean"}});
        assert!(validate_json_schema(&json!({"name":"A","ok":true}), &s).is_ok());
        rejects(json!({}), s.clone(), "");
        rejects(json!({"ok":true}), s.clone(), "/name");
        rejects(json!({"name":1}), s.clone(), "/name");
        rejects(json!({"name":"A","ok":2}), s.clone(), "/ok");
        rejects(json!({"name":"A","x":true,"y":true}), s, "");
        rejects(json!({"x":2}), json!({"additionalProperties":false}), "/x");
        assert!(
            validate_json_schema(&json!({"x":2}), &json!({"additionalProperties":true})).is_ok()
        );
    }

    #[test]
    fn nested_and_escaped_pointers_are_deterministic() {
        let item = json!({"properties":{"score":{"minimum":1},"a~/b":{"type":"string"}}});
        let schema = json!({"properties":{"items":{"items":item}}});
        let errors =
            validate_json_schema(&json!({"items":[{"score":0,"a~/b":3}]}), &schema).unwrap_err();
        assert_eq!(
            errors
                .iter()
                .map(|e| e.pointer.as_str())
                .collect::<Vec<_>>(),
            ["/items/0/a~0~1b", "/items/0/score"]
        );
        rejects(json!({}), json!({"required":[""]}), "/");
    }

    #[test]
    fn combinators_apply_alongside_sibling_assertions() {
        let branches = json!([{"type":"integer"},{"minimum":0,"type":"number"}]);
        assert!(validate_json_schema(&json!(2), &json!({"anyOf":branches})).is_ok());
        rejects(json!("x"), json!({"anyOf":branches}), "");
        rejects(json!(2), json!({"oneOf":branches}), "");
        rejects(json!("x"), json!({"oneOf":branches}), "");
        assert!(validate_json_schema(&json!(-2), &json!({"oneOf":branches})).is_ok());
        rejects(json!(1), json!({"allOf":branches,"const":2}), "");
        rejects(
            json!({"x":0}),
            json!({"allOf":[true,{"properties":{"x":false}}]}),
            "/x",
        );
    }

    #[test]
    fn verifies_missing_malformed_oversized_and_valid_files() {
        let d = tempdir().unwrap();
        let path = d.path().join("output.json");
        let schema = json!({"required":["ok"]});
        assert_eq!(
            verify_json_file_schema(&path, &schema).error_signature,
            "missing_output"
        );
        for (contents, signature) in [
            ("{", "invalid_json"),
            ("{}", "schema_violation:/ok"),
            ("{\"ok\":true}", ""),
        ] {
            std::fs::write(&path, contents).unwrap();
            assert_eq!(
                verify_json_file_schema(&path, &schema).error_signature,
                signature
            );
        }
        assert_eq!(
            verify_json_file_schema(&path, &schema),
            VerifyOutcome::pass("Schema valid")
        );
        File::create(&path)
            .unwrap()
            .set_len((16 << 20) + 1)
            .unwrap();
        assert_eq!(
            verify_json_file_schema(&path, &schema).error_signature,
            "invalid_json"
        );
        assert_eq!(
            verify_json_file_schema(d.path(), &schema).error_signature,
            "invalid_json"
        );
    }

    #[test]
    fn reducers_preserve_splitter_order() {
        for (name, inputs, expected) in [
            ("collect_list", json!([2, [3]]), json!([2, [3]])),
            (
                "merge_object",
                json!([{"x":1,"y":0},{"x":2}]),
                json!({"x":2,"y":0}),
            ),
            ("first", json!([2, 1]), json!(2)),
            ("last", json!([2, 1]), json!(1)),
            ("sum", json!([1, -2, 3.5]), json!(2.5)),
            (
                "sum",
                json!([9007199254740992_u64, 1]),
                json!(9007199254740993_u64),
            ),
            ("concat", json!(["ab", "c"]), json!("abc")),
            ("concat", json!([[1, 2], [], [3]]), json!([1, 2, 3])),
            ("unique_list", json!([3, 1, 3.0, 2, 1]), json!([3, 1, 2])),
        ] {
            assert_eq!(
                apply_reducer(
                    ReducerKind::parse(name).unwrap(),
                    inputs.as_array().unwrap()
                )
                .unwrap(),
                expected
            );
        }
        assert_eq!(ReducerKind::parse("typo"), None);
    }

    #[test]
    fn empty_reductions_type_errors_and_overflow() {
        for (kind, v) in [
            (ReducerKind::First, Value::Null),
            (ReducerKind::Last, Value::Null),
            (ReducerKind::CollectList, json!([])),
            (ReducerKind::UniqueList, json!([])),
            (ReducerKind::Concat, json!("")),
            (ReducerKind::MergeObject, json!({})),
            (ReducerKind::Sum, json!(0)),
        ] {
            assert_eq!(apply_reducer(kind, &[]).unwrap(), v);
        }
        for (kind, values) in [
            (ReducerKind::Sum, json!([1, "2"])),
            (ReducerKind::Sum, json!([u64::MAX, 1])),
            (ReducerKind::Sum, json!([1.0e308, 1.0e308])),
            (ReducerKind::MergeObject, json!([{}, null])),
            (ReducerKind::Concat, json!([[], 1])),
            (ReducerKind::Concat, json!(["", []])),
        ] {
            assert!(apply_reducer(kind, values.as_array().unwrap()).is_err());
        }
    }
}
