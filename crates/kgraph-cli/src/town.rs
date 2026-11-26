use crate::{
    args::*, default_file, default_url, is_python, load, print_json, project_dir, report_for_state,
    resolve, Result,
};
use kgraph_core::files::{read_object, write_text};
use kgraph_town::{interactions as live, network, registry, server, snapshot, ui};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

fn provider(mut town: Town, command: &str) -> Result<snapshot::SnapshotProvider> {
    if let Some(w) = &town.workflow {
        town.state
            .get_or_insert_with(|| default_file(w, "_state.json"));
        town.report
            .get_or_insert_with(|| default_file(w, "_report.json"));
        town.workflow = Some(resolve(w)?);
    } else if town.state.is_none() {
        return Err(format!(
            "kgraph-town {command} requires a workflow or --state"
        ));
    }
    if town.workflow.as_deref().is_some_and(is_python) {
        return Ok(Arc::new(move || {
            Ok(snapshot::build_town_snapshot_from_graph(
                town.workflow.as_deref(),
                town.state.as_deref(),
                town.report.as_deref(),
                None,
                town.title.as_deref(),
                None,
                None,
            ))
        }));
    }
    let loader: Option<snapshot::DefinitionLoader> = if town.definition.ctx.is_some() {
        town.workflow.clone().map(|path| {
            let args = town.definition.clone();
            Arc::new(move || {
                let flow = load(&path, &args)?;
                Ok((flow.nodes, flow.context))
            }) as snapshot::DefinitionLoader
        })
    } else {
        None
    };
    let explicit_title = town.title.is_some();
    let inline_title = if !explicit_title && town.definition.ctx.is_some() {
        town.workflow
            .as_deref()
            .and_then(|p| load(p, &town.definition).ok())
            .and_then(|f| f.title)
    } else {
        None
    };
    Ok(snapshot::cached_town_snapshot(
        town.workflow,
        town.state,
        town.report,
        town.definition.ctx_file,
        town.title.or(inline_title),
        loader,
    ))
}

fn config(s: &Serve, layout: &str, read_only: bool) -> server::ServerConfig {
    server::ServerConfig {
        host: if s.lan {
            "0.0.0.0".into()
        } else {
            s.host.clone()
        },
        port: s.port,
        poll_ms: s.poll_ms,
        token: if s.auth {
            Some(network::generate_token())
        } else {
            s.token.clone()
        },
        layout_mode: layout.into(),
        read_only,
    }
}

pub fn ui(town: Town, s: Serve, layout: &str, read_only: bool) -> Result<i32> {
    let provider = provider(town, "ui")?;
    provider()?;
    server::serve_town_blocking(
        provider,
        config(&s, layout, read_only),
        s.open,
        s.tunnel,
        s.no_auth,
    )?;
    Ok(0)
}

pub fn hub(s: Serve, dirs: Vec<PathBuf>, action: Option<HubAction>) -> Result<i32> {
    if let Some(HubAction::List { json }) = action {
        let runs = registry::list_runs(&[]);
        if json {
            return print_json(&json!({"runs":runs}));
        }
        if runs.is_empty() {
            println!("No registered workflow runs.");
        }
        for run in runs {
            println!(
                "{} / {} | {} | {} | {}",
                sval(&run, "project_name"),
                sval(&run, "name"),
                sval(&run, "status"),
                run["updated_at"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("—"),
                sval(&run, "id")
            );
        }
    } else {
        server::serve_hub_blocking(dirs, config(&s, "grid", false), s.open, s.tunnel, s.no_auth)?;
    }
    Ok(0)
}

pub fn render(town: Town, layout: &str, output: Option<PathBuf>, embed: bool) -> Result<i32> {
    let output = output.unwrap_or_else(|| {
        town.workflow
            .as_ref()
            .map(|p| default_file(p, "_town.html"))
            .unwrap_or_else(|| ".kgraph/town.html".into())
    });
    let ctx = town.definition.ctx_file.clone();
    let data = provider(town, "render")?()?;
    let target = resolve(&output)?;
    for path in ["workflow_ref", "state_file", "report_file"]
        .iter()
        .filter_map(|k| data[k].as_str())
        .map(Path::new)
        .chain(ctx.as_deref())
    {
        if target == resolve(path)? {
            return Err(
                "--output must be separate from the workflow, state, report, and context files"
                    .into(),
            );
        }
    }
    write_text(
        &output,
        &ui::render_town_html(
            &data,
            &ui::RenderTownOptions {
                layout_mode: layout,
                embed,
                ..Default::default()
            },
        )?,
    )?;
    println!("{}", output.display());
    Ok(0)
}

pub fn publish(mut data: Value, url: &str, opts: &Publish) -> Result<Value> {
    for field in ["state_file", "report_file"] {
        if let Some(path) = data[field].as_str().filter(|s| !s.is_empty()) {
            data[field] = json!(resolve(Path::new(path))?);
        }
    }
    let cwd = project_dir()?;
    let mut meta = json!({"name":data["title"],"status":data["status"],
        "project_name":opts.project_name.as_deref().unwrap_or_else(||cwd.file_name().and_then(|n|n.to_str()).unwrap_or_else(||cwd.to_str().unwrap_or("/"))),
        "source_host":opts.host_label.clone().unwrap_or_else(registry::hostname)});
    if let Some(id) = &opts.run_id {
        meta["run_id"] = json!(id);
    }
    Ok(Value::Object(network::push_snapshot(
        url,
        opts.token.as_deref(),
        &meta,
        &data,
    )?))
}
fn sval<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or_default()
}
fn pushed(r: &Value) {
    println!("Pushed {} | {}", sval(r, "id"), sval(r, "status"));
}
pub fn push(town: Town, url: &str, opts: &Publish) -> Result<i32> {
    pushed(&publish(provider(town, "push")?()?, url, opts)?);
    Ok(0)
}

fn update_fields(update: &Update) -> Value {
    let mut v = json!({"title":update.title,"detail":update.detail,"recommended":update.recommended,"ticket":update.ticket});
    if !update.dep.is_empty() {
        v["deps"] = json!(update.dep);
    }
    if !update.option.is_empty() {
        v["options"] = json!(update.option);
    }
    v
}
pub fn progress(p: Progress) -> Result<i32> {
    if p.progress.is_some_and(|v| !v.is_finite()) {
        return Err("Progress must be a number from 0 to 100".into());
    }
    let mut fields = update_fields(&p.update);
    for (k, v) in [
        ("status", json!(p.status)),
        ("kind", json!(p.kind)),
        ("agent", json!(p.agent)),
        ("progress", json!(p.progress)),
        ("question", json!(p.question)),
    ] {
        fields[k] = v;
    }
    if !p.output.is_empty() {
        fields["outputs"] = json!(p.output);
    }
    fields.as_object_mut().unwrap().retain(|_, v| !v.is_null());
    let data = live::update_node_progress(
        &p.update.state,
        &p.update.node,
        p.item.as_deref(),
        &fields,
        p.clear,
    )?;
    finish_update(p.update, data)
}
pub fn ask(a: Ask) -> Result<i32> {
    let mut fields = update_fields(&a.update);
    fields["question"] = json!(a.question);
    if let Some(source) = a.questions_json {
        let raw = if source.trim_start().starts_with(['[', '{']) {
            source
        } else {
            fs::read_to_string(&source).map_err(|e| format!("{source}: {e}"))?
        };
        fields["questions"] = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    }
    fields["multi_select"] = json!(a.multi_select);
    fields["allow_note"] = json!(!a.no_note);
    fields["agent"] = json!(a.agent);
    let data = live::ask_node_question(&a.update.state, &a.update.node, &a.item, &fields)?;
    finish_update(a.update, data)
}
fn finish_update(update: Update, data: Value) -> Result<i32> {
    if update.push || update.hub_url.is_some() {
        let cwd = project_dir()?;
        let state = resolve(&update.state)?;
        let report = update.report.unwrap_or_else(|| report_for_state(&state));
        let mut workflow = update.workflow;
        if workflow.is_none() && report.is_file() {
            if let Some(reference) = read_object(&report)?
                .get("workflow_ref")
                .and_then(Value::as_str)
            {
                workflow = [cwd.as_path(), report.parent().unwrap_or(Path::new("."))]
                    .into_iter()
                    .map(|base| base.join(reference))
                    .find(|p| p.is_file());
            }
        }
        let project = live::allowed_kgraph_root(&state)
            .ok()
            .and_then(|root| root.parent().map(Path::to_owned))
            .unwrap_or(cwd);
        let mut snapshot = provider(
            Town {
                workflow,
                state: Some(state),
                report: Some(report),
                definition: Default::default(),
                title: None,
            },
            "push",
        )?()?;
        snapshot["project_dir"] = json!(project);
        let env = std::env::var("KGRAPH_HUB_URL").ok();
        let url = default_url(
            update
                .hub_url
                .as_deref()
                .filter(|s| !s.is_empty())
                .or(env.as_deref()),
        );
        let run = publish(snapshot, url, &update.publish)?;
        if !update.json {
            pushed(&run);
        }
    }
    if update.json {
        print_json(&data)
    } else {
        println!("{}", live::progress_path_for_state(&update.state).display());
        Ok(0)
    }
}
