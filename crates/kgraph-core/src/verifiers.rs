//! Strict binary media validation and PNG alpha-coverage measurements.

use crate::types::VerifyOutcome;
use flate2::{Crc, Decompress, FlushDecompress, Status};
use png::ColorType;
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PngBBox {
    pub min_x: u32,
    pub min_y: u32,
    pub max_x: u32,
    pub max_y: u32,
    pub content_width: u32,
    pub content_height: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PngInspection {
    pub width: u32,
    pub height: u32,
    pub color_type: u8,
    pub has_alpha: bool,
    pub aspect_ratio: f64,
    pub opaque_ratio: f64,
    pub transparent_ratio: f64,
    pub edge_transparent_ratio: f64,
    pub bbox: Option<PngBBox>,
    pub bottom_margin_ratio: f64,
    pub soft_shadow_pixels: usize,
}

const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

fn split_at(data: &[u8], n: usize) -> Option<(&[u8], &[u8])> {
    Some((data.get(..n)?, &data[n..]))
}

fn png_chunk<'a>(input: &mut &'a [u8]) -> Result<(&'a [u8], &'a [u8]), String> {
    let header = input.get(..8).ok_or("Missing PNG chunk header")?;
    let length = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
    let kind = &header[4..];
    let (payload, rest) = split_at(&input[8..], length).ok_or("Truncated PNG chunk")?;
    let (checksum, next) = split_at(rest, 4).ok_or("Truncated PNG chunk CRC")?;
    let mut crc = Crc::new();
    crc.update(kind);
    crc.update(payload);
    if crc.sum().to_be_bytes() != checksum {
        return Err("Bad PNG chunk CRC".into());
    }
    *input = next;
    Ok((kind, payload))
}

// The png crate accepts extra compressed bytes after the image. Require the
// zlib stream end, checksum, entire input, and exact declared scanline length.
fn scanlines(compressed: &[u8], size: usize) -> Result<Vec<u8>, String> {
    let mut inflater = Decompress::new(true);
    let mut scratch = [0; 32 * 1024];
    let mut raw = Vec::new();
    loop {
        let offset = inflater.total_in() as usize;
        let status = inflater
            .decompress(&compressed[offset..], &mut scratch, FlushDecompress::None)
            .map_err(|e| e.to_string())?;
        let written = inflater.total_out() as usize - raw.len();
        if written > size - raw.len() {
            break;
        }
        raw.extend_from_slice(&scratch[..written]);
        if status == Status::StreamEnd {
            if raw.len() == size && inflater.total_in() as usize == compressed.len() {
                return Ok(raw);
            }
            break;
        }
        if written == 0 && inflater.total_in() as usize == offset {
            break;
        }
    }
    Err("PNG scanline data does not match the dimensions".into())
}

fn paeth(left: u8, up: u8, corner: u8) -> u8 {
    let estimate = i32::from(left) + i32::from(up) - i32::from(corner);
    let a = (estimate - i32::from(left)).abs();
    let b = (estimate - i32::from(up)).abs();
    let c = (estimate - i32::from(corner)).abs();
    if a <= b && a <= c {
        left
    } else if b <= c {
        up
    } else {
        corner
    }
}

fn unfilter(raw: &mut [u8], stride: usize, channels: usize) -> Result<(), String> {
    let mut previous: &[u8] = &[];
    for scanline in raw.chunks_exact_mut(stride + 1) {
        let (&mut method, row) = scanline.split_first_mut().unwrap();
        if method > 4 {
            return Err(format!("Unknown PNG filter {method}"));
        }
        if method != 0 {
            for i in 0..row.len() {
                let before = i.checked_sub(channels);
                let left = before.map_or(0, |j| row[j]);
                let up = previous.get(i).copied().unwrap_or(0);
                let predictor = match method {
                    1 => left,
                    2 => up,
                    3 => ((u16::from(left) + u16::from(up)) / 2) as u8,
                    _ => paeth(
                        left,
                        up,
                        before.and_then(|j| previous.get(j)).copied().unwrap_or(0),
                    ),
                };
                row[i] = row[i].wrapping_add(predictor);
            }
        }
        previous = row;
    }
    Ok(())
}

struct Png {
    width: u32,
    height: u32,
    color: ColorType,
    // Palette pixels already have their effective RGBA values, including tRNS.
    palette: Vec<[u8; 4]>,
    transparency: Option<Vec<u8>>,
    stride: usize,
    raw: Vec<u8>,
}

impl Png {
    fn decode(data: &[u8]) -> Result<Self, String> {
        let mut input = data
            .strip_prefix(SIGNATURE)
            .ok_or("Missing PNG signature")?;
        let (kind, header) = png_chunk(&mut input)?;
        if kind != b"IHDR" || header.len() != 13 {
            return Err("Missing or invalid IHDR chunk".into());
        }
        let width = u32::from_be_bytes(header[..4].try_into().unwrap());
        let height = u32::from_be_bytes(header[4..8].try_into().unwrap());
        if width == 0 || height == 0 || width > i32::MAX as u32 || height > i32::MAX as u32 {
            return Err("Invalid PNG dimensions".into());
        }
        let color = ColorType::from_u8(header[9]).ok_or("Unsupported PNG color type")?;
        if header[8] != 8 || header[10..] != [0, 0, 0] {
            return Err("Expected an 8-bit, non-interlaced PNG".into());
        }
        let mut palette = Vec::new();
        let mut transparency = None;
        let mut compressed = Vec::new();
        let mut idat_ended = false;
        loop {
            let (kind, payload) = png_chunk(&mut input)?;
            if kind == b"IDAT" {
                if idat_ended {
                    return Err("PNG IDAT chunks must be consecutive".into());
                }
                compressed.extend_from_slice(payload);
                continue;
            }
            idat_ended |= !compressed.is_empty();
            match kind {
                b"PLTE" => {
                    if idat_ended
                        || !palette.is_empty()
                        || transparency.is_some()
                        || matches!(color, ColorType::Grayscale | ColorType::GrayscaleAlpha)
                    {
                        return Err("Unexpected PNG palette".into());
                    }
                    if !(3..=768).contains(&payload.len()) || payload.len() % 3 != 0 {
                        return Err("Invalid PNG palette".into());
                    }
                    palette.extend(payload.chunks(3).map(|p| [p[0], p[1], p[2], 255]));
                }
                b"tRNS" => {
                    if idat_ended || transparency.is_some() {
                        return Err("Unexpected PNG transparency chunk".into());
                    }
                    let key = match color {
                        ColorType::Indexed if (1..=palette.len()).contains(&payload.len()) => {
                            for (pixel, &alpha) in palette.iter_mut().zip(payload) {
                                pixel[3] = alpha;
                            }
                            Vec::new()
                        }
                        ColorType::Grayscale | ColorType::Rgb
                            if payload.len() == 2 * color.samples() =>
                        {
                            if payload.iter().step_by(2).any(|&high| high != 0) {
                                return Err("PNG transparency key exceeds bit depth".into());
                            }
                            payload.iter().skip(1).step_by(2).copied().collect()
                        }
                        _ => return Err("Invalid PNG transparency data".into()),
                    };
                    transparency = Some(key);
                }
                b"IEND" => {
                    if !payload.is_empty() || !input.is_empty() {
                        return Err("Invalid PNG end".into());
                    }
                    break;
                }
                _ if kind[0] & 32 == 0 => return Err("Unsupported critical PNG chunk".into()),
                _ => (),
            }
        }
        if color == ColorType::Indexed && palette.is_empty() {
            return Err("Indexed PNG requires a palette".into());
        }
        let stride = (width as usize)
            .checked_mul(color.samples())
            .ok_or("Invalid PNG dimensions")?;
        let size = stride
            .checked_add(1)
            .and_then(|n| n.checked_mul(height as usize))
            .ok_or("Invalid PNG dimensions")?;
        let mut raw = scanlines(&compressed, size)?;
        unfilter(&mut raw, stride, color.samples())?;
        let png = Self {
            width,
            height,
            color,
            palette,
            transparency,
            stride,
            raw,
        };
        if color == ColorType::Indexed
            && png
                .rows()
                .any(|row| row.iter().any(|&i| i as usize >= png.palette.len()))
        {
            return Err("PNG pixel refers to a missing palette entry".into());
        }
        Ok(png)
    }

    fn rows(&self) -> impl Iterator<Item = &[u8]> {
        self.raw.chunks_exact(self.stride + 1).map(|row| &row[1..])
    }

    fn alpha(&self, pixel: &[u8]) -> u8 {
        match self.color {
            ColorType::Indexed => self.palette[pixel[0] as usize][3],
            ColorType::GrayscaleAlpha | ColorType::Rgba => pixel[pixel.len() - 1],
            _ if self.transparency.as_deref() == Some(pixel) => 0,
            _ => 255,
        }
    }

    fn has_variation(&self) -> bool {
        let mut pixels = self
            .rows()
            .flat_map(|row| row.chunks_exact(self.color.samples()));
        let visible = |pixel: &'_ [u8]| {
            let value = match self.color {
                ColorType::Indexed => self.palette[pixel[0] as usize],
                ColorType::Grayscale => [pixel[0], 0, 0, 255],
                ColorType::Rgb => [pixel[0], pixel[1], pixel[2], 255],
                ColorType::GrayscaleAlpha => [pixel[0], 0, 0, pixel[1]],
                ColorType::Rgba => [pixel[0], pixel[1], pixel[2], pixel[3]],
            };
            if value[3] == 0 {
                [0; 4]
            } else {
                value
            }
        };
        let first = visible(pixels.next().unwrap());
        pixels.any(|p| visible(p) != first)
    }

    fn inspect(&self) -> PngInspection {
        let (w, h) = (self.width, self.height);
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (w, h, 0, 0);
        let (mut opaque, mut transparent, mut edge_count, mut edge_clear) =
            (0u64, 0u64, 0u64, 0u64);
        for (y, row) in self.rows().enumerate() {
            let y = y as u32;
            for (x, pixel) in row.chunks_exact(self.color.samples()).enumerate() {
                let x = x as u32;
                let alpha = self.alpha(pixel);
                if x == 0 || x == w - 1 || y == 0 || y == h - 1 {
                    edge_count += 1;
                    edge_clear += u64::from(alpha <= 16);
                }
                if alpha <= 16 {
                    transparent += 1;
                    continue;
                }
                opaque += u64::from(alpha == 255);
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
        let bbox = (min_y < h).then(|| PngBBox {
            min_x,
            min_y,
            max_x,
            max_y,
            content_width: max_x - min_x + 1,
            content_height: max_y - min_y + 1,
        });
        let mut bottom_margin_ratio = 1.0;
        let mut soft_shadow_pixels = 0;
        if let Some(bounds) = &bbox {
            bottom_margin_ratio = f64::from(h - max_y - 1) / f64::from(h);
            let band = (u64::from(bounds.content_height) * 3).div_ceil(10) as u32;
            let channels = self.color.samples();
            for row in self
                .rows()
                .skip((max_y + 1 - band) as usize)
                .take(band as usize + 1)
            {
                soft_shadow_pixels += row
                    [min_x as usize * channels..(max_x as usize + 1) * channels]
                    .chunks_exact(channels)
                    .filter(|p| (10..=200).contains(&self.alpha(p)))
                    .count();
            }
        }
        let area = f64::from(w) * f64::from(h);
        PngInspection {
            width: w,
            height: h,
            color_type: self.color as u8,
            has_alpha: self.transparency.is_some()
                || matches!(self.color, ColorType::GrayscaleAlpha | ColorType::Rgba),
            aspect_ratio: f64::from(w) / f64::from(h),
            opaque_ratio: opaque as f64 / area,
            transparent_ratio: transparent as f64 / area,
            edge_transparent_ratio: edge_clear as f64 / edge_count as f64,
            bbox,
            bottom_margin_ratio,
            soft_shadow_pixels,
        }
    }
}

fn read_png(path: &Path) -> Result<Png, String> {
    fs::read(path)
        .map_err(|e| e.to_string())
        .and_then(|data| Png::decode(&data))
        .map_err(|e| format!("{}: {e}", path.display()))
}

pub fn inspect_png(path: &Path) -> Result<PngInspection, String> {
    Ok(read_png(path)?.inspect())
}

pub fn verify_png_file(path: &Path, min_width: u32, min_height: u32) -> VerifyOutcome {
    let name = path.display();
    if !path.is_file() {
        return VerifyOutcome::fail("missing_file", format!("Missing screenshot {name}"));
    }
    let image = match read_png(path) {
        Ok(image) => image,
        Err(e) => return VerifyOutcome::fail("invalid_png", e),
    };
    let (w, h) = (image.width, image.height);
    if w < min_width || h < min_height {
        return VerifyOutcome::fail(
            "too_small",
            format!("{name}: {w}×{h}px, minimum {min_width}×{min_height}px"),
        );
    }
    if !image.has_variation() {
        return VerifyOutcome::fail(
            "blank_png",
            format!("{name}: screenshot is blank or a solid color"),
        );
    }
    VerifyOutcome::pass(format!("{w}×{h}px PNG with visible variation"))
}

fn jpeg_marker(data: &[u8], offset: usize) -> Option<(u8, usize)> {
    if data.get(offset) != Some(&0xff) {
        return None;
    }
    let end = data[offset..].iter().position(|&b| b != 0xff)? + offset;
    Some((data[end], end + 1))
}

pub fn check_jpeg(data: &[u8]) -> Result<(), String> {
    if !data.starts_with(&[0xff, 0xd8]) {
        return Err("Missing JPEG start marker".into());
    }
    let mut offset = 2;
    let (mut frame, mut scan) = (false, false);
    while let Some((marker, start)) = jpeg_marker(data, offset) {
        if marker == 0xd9 && frame && scan {
            return Ok(());
        }
        let Some(header) = data[start..].get(..2) else {
            break;
        };
        let len = u16::from_be_bytes(header.try_into().unwrap()) as usize;
        let Some(segment) = data[start..].get(..len) else {
            break;
        };
        if len < 2 {
            break;
        }
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            if len < 11 || segment[3..5] == [0, 0] || segment[5..7] == [0, 0] {
                break;
            }
            frame = true;
        }
        offset = start + len;
        if marker == 0xda {
            if len < 8 {
                break;
            }
            let scan_start = offset;
            while let Some(index) = data[offset..].iter().position(|&b| b == 0xff) {
                offset += index;
                let Some((code, end)) = jpeg_marker(data, offset) else {
                    break;
                };
                if !matches!(code, 0 | 0xd0..=0xd7) {
                    scan |= offset > scan_start;
                    break;
                }
                offset = end;
            }
            if offset == scan_start {
                break;
            }
        }
    }
    Err("JPEG is missing a valid frame, scan, or end marker".into())
}

fn webp_images(mut chunks: &[u8], animated: bool) -> Result<usize, String> {
    let mut count = 0;
    while !chunks.is_empty() {
        let header = chunks.get(..8).ok_or("Truncated WebP chunk header")?;
        let len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        let (payload, rest) = split_at(&chunks[8..], len).ok_or("Truncated WebP chunk")?;
        chunks = rest.get(len % 2..).ok_or("Truncated WebP chunk")?;
        match &header[..4] {
            b"VP8 " => {
                if len <= 10
                    || payload[0] & 1 != 0
                    || payload[3..6] != [0x9d, 1, 0x2a]
                    || u16::from_le_bytes([payload[6], payload[7]]) & 0x3fff == 0
                    || u16::from_le_bytes([payload[8], payload[9]]) & 0x3fff == 0
                {
                    return Err("Invalid lossy WebP frame".into());
                }
            }
            b"VP8L" => {
                if len <= 5 || payload[0] != 0x2f || payload[4] >> 5 != 0 {
                    return Err("Invalid lossless WebP frame".into());
                }
            }
            b"ANMF" => {
                if !animated || len <= 16 || webp_images(&payload[16..], false)? != 1 {
                    return Err("Invalid animated WebP frame".into());
                }
            }
            _ => continue,
        }
        count += 1;
    }
    Ok(count)
}

pub fn check_webp(data: &[u8]) -> Result<(), String> {
    if data.len() < 12
        || &data[..4] != b"RIFF"
        || &data[8..12] != b"WEBP"
        || u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize != data.len() - 8
        || webp_images(&data[12..], true)? == 0
    {
        return Err("Missing or invalid WebP container or frame".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{write::ZlibEncoder, Compression};
    use std::io::Write;
    use tempfile::TempDir;

    fn chunk(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut crc = Crc::new();
        crc.update(kind);
        crc.update(payload);
        [
            (payload.len() as u32).to_be_bytes().as_slice(),
            kind,
            payload,
            &crc.sum().to_be_bytes(),
        ]
        .concat()
    }

    fn compressed(raw: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(raw).unwrap();
        encoder.finish().unwrap()
    }

    fn header(w: u32, h: u32, color: ColorType) -> Vec<u8> {
        [
            &w.to_be_bytes()[..],
            &h.to_be_bytes(),
            &[8, color as u8, 0, 0, 0],
        ]
        .concat()
    }

    fn png(header: &[u8], before: &[(&[u8; 4], &[u8])], idat: &[u8]) -> Vec<u8> {
        let mut bytes = SIGNATURE.to_vec();
        bytes.extend(chunk(b"IHDR", header));
        for (kind, payload) in before {
            bytes.extend(chunk(kind, payload));
        }
        bytes.extend(chunk(b"IDAT", idat));
        bytes.extend(chunk(b"IEND", &[]));
        bytes
    }

    #[test]
    fn visible_variation_and_alpha_for_every_color_type() {
        use ColorType::*;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("screenshot.png");
        let palette = &[1, 2, 3, 4, 5, 6];
        type Case<'a> = (
            ColorType,
            &'a [(&'a [u8; 4], &'a [u8])],
            &'a [u8],
            bool,
            f64,
        );
        let cases: &[Case<'_>] = &[
            (Grayscale, &[], &[0, 10, 20], false, 0.0),
            (Grayscale, &[(b"tRNS", &[0, 10])], &[0, 10, 20], true, 0.5),
            (Rgb, &[], &[0, 1, 2, 3, 1, 2, 4], false, 0.0),
            (
                Rgb,
                &[(b"tRNS", &[0, 1, 0, 2, 0, 3])],
                &[0, 1, 2, 3, 1, 2, 4],
                true,
                0.5,
            ),
            (GrayscaleAlpha, &[], &[0, 20, 0, 20, 255], true, 0.5),
            (Rgba, &[], &[0, 1, 2, 3, 0, 1, 2, 3, 255], true, 0.5),
            (Indexed, &[(b"PLTE", palette)], &[0, 0, 1], false, 0.0),
            (
                Indexed,
                &[(b"PLTE", palette), (b"tRNS", &[0])],
                &[0, 0, 1],
                true,
                0.5,
            ),
        ];
        for &(color, before, raw, alpha, clear) in cases {
            fs::write(&path, png(&header(2, 1, color), before, &compressed(raw))).unwrap();
            let result = verify_png_file(&path, 2, 1);
            assert!(result.ok, "{color:?}: {}", result.detail);
            assert_eq!(result.detail, "2×1px PNG with visible variation");
            let inspection = inspect_png(&path).unwrap();
            assert_eq!(inspection.color_type, color as u8);
            assert_eq!(inspection.has_alpha, alpha);
            assert_eq!(inspection.transparent_ratio, clear);
            assert_eq!(inspection.edge_transparent_ratio, clear);
            assert_eq!(inspection.opaque_ratio, 1.0 - clear);
        }
    }

    #[test]
    fn uniform_colors_duplicate_palettes_and_transparent_noise_are_blank() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("shot");
        assert_eq!(verify_png_file(&path, 1, 1).error_signature, "missing_file");
        for (color, raw) in [
            (ColorType::Rgb, vec![0, 1, 2, 3, 1, 2, 3]),
            (ColorType::Grayscale, vec![0, 7, 7]),
            (ColorType::GrayscaleAlpha, vec![0, 7, 0, 17, 0]),
            (ColorType::Rgba, vec![0, 1, 2, 3, 0, 4, 5, 6, 0]),
        ] {
            fs::write(&path, png(&header(2, 1, color), &[], &compressed(&raw))).unwrap();
            assert_eq!(verify_png_file(&path, 1, 1).error_signature, "blank_png");
            assert_eq!(verify_png_file(&path, 3, 2).error_signature, "too_small");
            if color == ColorType::Rgba {
                let stats = inspect_png(&path).unwrap();
                assert!(stats.bbox.is_none());
                assert_eq!(
                    (
                        stats.transparent_ratio,
                        stats.bottom_margin_ratio,
                        stats.soft_shadow_pixels
                    ),
                    (1.0, 1.0, 0)
                );
            }
        }
        for (palette, trns) in [
            (&[1, 2, 3, 1, 2, 3], &[255, 255]),
            (&[1, 2, 3, 4, 5, 6], &[0, 0]),
        ] {
            let bytes = png(
                &header(2, 1, ColorType::Indexed),
                &[(b"PLTE", palette), (b"tRNS", trns)],
                &compressed(&[0, 0, 1]),
            );
            fs::write(&path, bytes).unwrap();
            assert_eq!(verify_png_file(&path, 1, 1).error_signature, "blank_png");
        }
    }

    #[test]
    fn measures_content_shadow_and_single_column_edges() {
        let mut raw = Vec::new();
        for row in [
            [0, 0, 0, 0],
            [0, 255, 0, 0],
            [0, 16, 17, 0],
            [0, 10, 200, 0],
            [0, 10, 16, 0],
        ] {
            raw.push(0);
            for alpha in row {
                raw.extend([42, alpha]);
            }
        }
        let image = Png::decode(&png(
            &header(4, 5, ColorType::GrayscaleAlpha),
            &[],
            &compressed(&raw),
        ))
        .unwrap();
        let stats = image.inspect();
        let bbox = stats.bbox.unwrap();
        assert_eq!(
            (
                bbox.min_x,
                bbox.min_y,
                bbox.max_x,
                bbox.max_y,
                bbox.content_width,
                bbox.content_height
            ),
            (1, 1, 2, 3, 2, 3)
        );
        assert_eq!(
            (
                stats.aspect_ratio,
                stats.opaque_ratio,
                stats.transparent_ratio,
                stats.edge_transparent_ratio
            ),
            (0.8, 0.05, 0.85, 1.0)
        );
        assert_eq!(
            (stats.bottom_margin_ratio, stats.soft_shadow_pixels),
            (0.2, 4)
        );
        let image = Png::decode(&png(
            &header(1, 3, ColorType::GrayscaleAlpha),
            &[],
            &compressed(&[0, 0, 0, 0, 0, 255, 0, 0, 0]),
        ))
        .unwrap();
        assert_eq!(image.inspect().edge_transparent_ratio, 2.0 / 3.0);
    }

    #[test]
    fn decodes_all_five_filters_against_png_encoder() {
        let pixels: Vec<u8> = (0..80).map(|i| (i * 71 + i / 7) as u8).collect();
        for filter in [
            png::FilterType::NoFilter,
            png::FilterType::Sub,
            png::FilterType::Up,
            png::FilterType::Avg,
            png::FilterType::Paeth,
        ] {
            let mut bytes = Vec::new();
            {
                let mut encoder = png::Encoder::new(&mut bytes, 4, 5);
                encoder.set_color(ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                encoder.set_filter(filter);
                encoder
                    .write_header()
                    .unwrap()
                    .write_image_data(&pixels)
                    .unwrap();
            }
            let decoded = Png::decode(&bytes).unwrap();
            assert_eq!(
                decoded.rows().flatten().copied().collect::<Vec<_>>(),
                pixels
            );
        }
    }

    #[test]
    fn malformed_pngs_fail_before_size_checks() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("invalid");
        let h = header(2, 1, ColorType::Grayscale);
        let z = compressed(&[0, 9, 7]);
        let valid = png(&h, &[], &z);
        let mut ancillary_crc = chunk(b"tEXt", b"x\0y");
        *ancillary_crc.last_mut().unwrap() ^= 1;
        let nonconsecutive = [
            chunk(b"IDAT", &z[..2]),
            chunk(b"tEXt", b"x\0y"),
            chunk(b"IDAT", &z[2..]),
        ]
        .concat();
        let with_extra =
            |chunks: Vec<u8>| [&valid[..33], &chunks, &valid[valid.len() - 12..]].concat();
        let mut bad_crc = valid.clone();
        bad_crc[29] ^= 1;
        let mut checksum = z.clone();
        *checksum.last_mut().unwrap() ^= 1;
        let mut invalid = vec![
            Vec::new(),
            valid[..valid.len() - 1].to_vec(),
            [valid.as_slice(), &[0]].concat(),
            bad_crc,
            with_extra([ancillary_crc, chunk(b"IDAT", &z)].concat()),
            with_extra(nonconsecutive),
            png(&h, &[(b"WHAT", b"")], &z),
            png(&h, &[(b"PLTE", &[0; 3])], &z),
            png(&h, &[(b"tRNS", &[1, 0])], &z),
            png(&h, &[], &checksum),
            png(&h, &[], &z[..z.len() - 1]),
            png(&h, &[], &[z.as_slice(), &[0]].concat()),
            png(&h, &[], &compressed(&[0, 0])),
            png(&h, &[], &compressed(&[0, 0, 0, 0])),
            png(&h, &[], &compressed(&[5, 0, 0])),
            png(
                &header(2, 1, ColorType::Indexed),
                &[(b"PLTE", &[0, 1, 2])],
                &compressed(&[0, 0, 1]),
            ),
            png(&header(2, 1, ColorType::Indexed), &[], &z),
            png(
                &header(2, 1, ColorType::Indexed),
                &[(b"PLTE", &[0, 1, 2]), (b"tRNS", &[1, 2])],
                &z,
            ),
        ];
        for (byte, value) in [(8, 16), (9, 1), (10, 1), (11, 1), (12, 1)] {
            let mut bad = h.clone();
            bad[byte] = value;
            invalid.push(png(&bad, &[], &z));
        }
        for (i, bytes) in invalid.into_iter().enumerate() {
            fs::write(&path, bytes).unwrap();
            assert!(inspect_png(&path).is_err(), "fixture {i}");
            assert_eq!(
                verify_png_file(&path, 16, 16).error_signature,
                "invalid_png",
                "fixture {i}"
            );
        }
    }

    #[test]
    fn jpeg_requires_frame_entropy_scan_and_end_marker() {
        let frame = [0xff, 0xff, 0xc2, 0, 11, 8, 0, 1, 0, 1, 1, 1, 0, 0];
        let sos = [0xff, 0xda, 0, 8, 1, 1, 0, 0, 0, 0];
        let entropy = [0xff, 0, 9, 0xff, 0xd1, 5];
        let jpeg = |f: &[u8], s: &[u8], scan: &[u8], end: &[u8]| {
            [&[0xff, 0xd8][..], f, s, scan, end].concat()
        };
        let end = [0xff, 0xd9];
        assert!(check_jpeg(&jpeg(&frame, &sos, &entropy, &[0xff, 0xd9, 0])).is_ok());
        assert!(check_jpeg(&jpeg(
            &frame,
            &sos,
            &entropy,
            &[sos.as_slice(), &entropy, &end].concat()
        ))
        .is_ok());
        let mut empty_height = frame;
        empty_height[7] = 0;
        for invalid in [
            vec![0],
            jpeg(&[], &sos, &entropy, &end),
            jpeg(&empty_height, &sos, &entropy, &end),
            jpeg(&frame, &[], &[], &end),
            jpeg(&frame, &sos, &[], &end),
            jpeg(&frame, &sos, &entropy, &[]),
            jpeg(&frame, &[0xff, 0xda, 0, 7, 0, 0, 0, 0, 0], &entropy, &end),
        ] {
            assert!(check_jpeg(&invalid).is_err());
        }
    }

    fn webp_chunk(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut bytes = [
            kind.as_slice(),
            &(payload.len() as u32).to_le_bytes(),
            payload,
        ]
        .concat();
        if payload.len() % 2 == 1 {
            bytes.push(0);
        }
        bytes
    }

    #[test]
    fn webp_requires_valid_riff_lengths_and_image_frames() {
        let container = |chunks: &[u8]| {
            [
                b"RIFF".as_slice(),
                &(4 + chunks.len() as u32).to_le_bytes(),
                b"WEBP",
                chunks,
            ]
            .concat()
        };
        let lossy = webp_chunk(b"VP8 ", &[0, 0, 0, 0x9d, 1, 0x2a, 1, 0, 1, 0, 0]);
        let lossless = webp_chunk(b"VP8L", &[0x2f, 0, 0, 0, 0, 1]);
        let anim = |nested: &[u8]| webp_chunk(b"ANMF", &[&[0; 16][..], nested].concat());
        for chunks in [
            &lossy[..],
            &lossless,
            &anim(&lossy),
            &[webp_chunk(b"JUNK", &[1]), lossless.clone()].concat(),
        ] {
            assert!(check_webp(&container(chunks)).is_ok());
        }
        for bad in [
            container(&[]),
            container(&lossy[..lossy.len() - 1]),
            container(&lossless[..7]),
            container(&webp_chunk(b"VP8 ", &[0; 11])),
            container(&webp_chunk(b"VP8L", &[0x2f, 0, 0, 0, 0x20, 0])),
            container(&anim(&[])),
            container(&anim(&anim(&lossless))),
            container(&anim(&[lossless.as_slice(), &lossless].concat())),
            [container(&lossy), vec![0]].concat(),
        ] {
            assert!(check_webp(&bad).is_err());
        }
    }
}
