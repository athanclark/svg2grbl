//! Decode SVG images and sample their pixels along the existing infill geometry.

use std::{fs, io, path::Path, str::FromStr};

use base64::{Engine, engine::general_purpose::STANDARD};
use image::RgbaImage;
use log::{info, warn};
use roxmltree::{Document, Node};
use svgtypes::{Align, AspectRatio, Length, LengthUnit, ViewBox};

use crate::{
    Affine, Bbox, CoordinatePair, InfillPattern, InfillSpec, LaserPower, Polyline, PoweredPolyline,
    length_to_mm, parallel_infill_all, parse_transform, rgba_to_power, shape_concentric_infill,
    wavy_infill_all,
};

pub(crate) struct ImageContext<'a> {
    pub viewbox: &'a ViewBox,
    pub width_mm: f64,
    pub height_mm: f64,
    pub dpi: f64,
    pub font_size: f64,
    pub base_dir: Option<&'a Path>,
}

struct RasterImage {
    pixels: RgbaImage,
    mm_to_pixel: Affine,
    // Visible pixel rectangle after preserveAspectRatio fitting/cropping.
    bounds: Bbox,
    outline: Vec<CoordinatePair>,
    opacity: f64,
}

pub(crate) fn image_toolpaths(
    doc: &Document,
    context: &ImageContext,
    infill: Option<&InfillSpec>,
    max_power: f64,
    max_line: f64,
) -> io::Result<Vec<PoweredPolyline>> {
    let images: Vec<_> = doc
        .descendants()
        .filter(|n| n.has_tag_name("image"))
        .collect();
    let Some(spec) = infill else {
        if !images.is_empty() {
            warn!(
                "SVG images are skipped without --infill; set a positive pass spacing to engrave them"
            );
        }
        return Ok(Vec::new());
    };
    let mut result = Vec::new();
    for (index, node) in images.into_iter().enumerate() {
        let label = node
            .attribute("id")
            .map(|id| format!("image #{id}"))
            .unwrap_or_else(|| format!("image {}", index + 1));
        let image = parse_image(node, context)
            .map_err(|e| io::Error::new(e.kind(), format!("{label}: {e}")))?;
        let Some(image) = image else { continue };
        info!(
            "Rasterizing {} ({} × {} pixels)",
            label,
            image.pixels.width(),
            image.pixels.height()
        );
        for pass in image.infill(spec) {
            image.sample_pass(&pass, max_power, max_line, &mut result);
        }
    }
    Ok(result)
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

/// Inline style takes precedence over presentation attributes. The image stage
/// deliberately does not depend on usvg rewriting or retaining image elements.
fn property<'a>(node: Node<'a, 'a>, name: &str) -> Option<&'a str> {
    node.attribute("style")
        .into_iter()
        .flat_map(|style| style.split(';'))
        .filter_map(|decl| decl.split_once(':'))
        .filter(|(key, _)| key.trim() == name)
        .map(|(_, value)| value.trim())
        .next_back()
        .or_else(|| node.attribute(name))
}

fn parse_image(node: Node, context: &ImageContext) -> io::Result<Option<RasterImage>> {
    let ancestors: Vec<_> = node.ancestors().filter(|n| n.is_element()).collect();
    if ancestors.iter().any(|n| {
        matches!(
            n.tag_name().name(),
            "defs" | "symbol" | "clipPath" | "mask" | "pattern" | "foreignObject"
        )
    }) {
        return Ok(None);
    }
    if ancestors
        .iter()
        .any(|n| property(*n, "display") == Some("none"))
    {
        return Ok(None);
    }
    if matches!(
        ancestors.iter().find_map(|n| property(*n, "visibility")),
        Some("hidden" | "collapse")
    ) {
        return Ok(None);
    }
    let mut opacity = 1.0;
    for ancestor in &ancestors {
        if let Some(value) = property(*ancestor, "opacity") {
            let value = if let Some(percent) = value.strip_suffix('%') {
                percent.parse::<f64>().map(|v| v / 100.0)
            } else {
                value.parse::<f64>()
            }
            .map_err(|_| invalid("invalid image/group opacity"))?;
            if !value.is_finite() {
                return Err(invalid("image opacity must be finite"));
            }
            opacity *= value.clamp(0.0, 1.0);
        }
    }
    if opacity <= 0.0 {
        return Ok(None);
    }
    if ancestors.iter().filter(|n| n.has_tag_name("svg")).count() > 1 {
        return Err(invalid(
            "images inside nested <svg> viewports are not supported",
        ));
    }
    for name in ["clip-path", "mask", "filter"] {
        if ancestors
            .iter()
            .any(|n| property(*n, name).is_some_and(|value| value != "none"))
        {
            return Err(invalid(format!(
                "{name} is not supported for raster images; apply the effect to the PNG/JPEG before embedding it"
            )));
        }
    }

    let dimension = |name: &str, default: &str, horizontal: bool| {
        let extent = if horizontal {
            context.viewbox.w
        } else {
            context.viewbox.h
        };
        image_length(property(node, name).unwrap_or(default), extent, context)
    };
    let x = dimension("x", "0", true)?;
    let y = dimension("y", "0", false)?;
    let width = dimension("width", "0", true)?;
    let height = dimension("height", "0", false)?;
    if width < 0.0 || height < 0.0 {
        return Err(invalid("image dimensions must be non-negative"));
    }
    if width == 0.0 || height == 0.0 {
        return Ok(None);
    }

    let href = node
        .attribute("href")
        .or_else(|| node.attribute(("http://www.w3.org/1999/xlink", "href")))
        .ok_or_else(|| invalid("missing href or xlink:href"))?;
    let bytes = image_bytes(href, context.base_dir)?;
    let pixels = image::load_from_memory(&bytes)
        .map_err(|e| invalid(format!("cannot decode PNG/JPEG: {e}")))?
        .to_rgba8();
    let ratio = node
        .attribute("preserveAspectRatio")
        .map(AspectRatio::from_str)
        .transpose()
        .map_err(|e| invalid(e.to_string()))?
        .unwrap_or_default();
    let (sx, sy, image_x, image_y) =
        fit_image(x, y, width, height, pixels.width(), pixels.height(), ratio);
    let bounds = Bbox {
        min_x: ((x - image_x) / sx).max(0.0),
        min_y: ((y - image_y) / sy).max(0.0),
        max_x: ((x + width - image_x) / sx).min(pixels.width() as f64),
        max_y: ((y + height - image_y) / sy).min(pixels.height() as f64),
    };
    let mut transform = Affine::identity();
    for ancestor in ancestors.iter().rev() {
        if let Some(value) = property(*ancestor, "transform") {
            transform = transform.compose(
                &parse_transform(value)
                    .map_err(|e| invalid(format!("invalid image transform: {e}")))?,
            );
        }
    }
    let normalize = Affine {
        a: context.width_mm / context.viewbox.w,
        d: context.height_mm / context.viewbox.h,
        ..Affine::identity()
    };
    let pixel_to_mm = normalize.compose(&transform).compose(&Affine {
        a: sx,
        d: sy,
        e: image_x,
        f: image_y,
        ..Affine::identity()
    });
    if ![
        pixel_to_mm.a,
        pixel_to_mm.b,
        pixel_to_mm.c,
        pixel_to_mm.d,
        pixel_to_mm.e,
        pixel_to_mm.f,
    ]
    .iter()
    .all(|v| v.is_finite())
    {
        return Err(invalid("image transform must be finite"));
    }
    let Some(mm_to_pixel) = pixel_to_mm.invert() else {
        // Zero-scale images have no visible area.
        return Ok(None);
    };
    let outline = [
        (bounds.min_x, bounds.min_y),
        (bounds.max_x, bounds.min_y),
        (bounds.max_x, bounds.max_y),
        (bounds.min_x, bounds.max_y),
    ]
    .into_iter()
    .map(|(x, y)| {
        let (x, y) = pixel_to_mm.apply(x, y);
        CoordinatePair::new(x, y)
    })
    .collect();
    Ok(Some(RasterImage {
        pixels,
        mm_to_pixel,
        bounds,
        outline,
        opacity,
    }))
}

fn image_length(value: &str, extent: f64, context: &ImageContext) -> io::Result<f64> {
    let length =
        Length::from_str(value).map_err(|e| invalid(format!("invalid image length: {e}")))?;
    let result = match length.unit {
        LengthUnit::None | LengthUnit::Px => length.number,
        LengthUnit::Percent => length.number * extent / 100.0,
        LengthUnit::Em => length.number * context.font_size,
        // Physical units first resolve into CSS pixels/current user units.
        // The root viewBox and element transforms then scale those lengths,
        // just as they do for vector geometry (not directly into final mm).
        _ => {
            length_to_mm(value, context.dpi, context.font_size).map_err(invalid)? * context.dpi
                / 25.4
        }
    };
    if !result.is_finite() {
        return Err(invalid("image lengths must be finite"));
    }
    Ok(result)
}

fn fit_image(
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    pixel_width: u32,
    pixel_height: u32,
    ratio: AspectRatio,
) -> (f64, f64, f64, f64) {
    let sx = width / pixel_width as f64;
    let sy = height / pixel_height as f64;
    if ratio.align == Align::None {
        return (sx, sy, x, y);
    }
    let scale = if ratio.slice { sx.max(sy) } else { sx.min(sy) };
    let align_x = match ratio.align {
        Align::XMinYMin | Align::XMinYMid | Align::XMinYMax => 0.0,
        Align::XMaxYMin | Align::XMaxYMid | Align::XMaxYMax => 1.0,
        _ => 0.5,
    };
    let align_y = match ratio.align {
        Align::XMinYMin | Align::XMidYMin | Align::XMaxYMin => 0.0,
        Align::XMinYMax | Align::XMidYMax | Align::XMaxYMax => 1.0,
        _ => 0.5,
    };
    (
        scale,
        scale,
        x + (width - pixel_width as f64 * scale) * align_x,
        y + (height - pixel_height as f64 * scale) * align_y,
    )
}

fn image_bytes(href: &str, base_dir: Option<&Path>) -> io::Result<Vec<u8>> {
    let href = href.trim();
    if let Some(data) = href.strip_prefix("data:") {
        let (header, payload) = data
            .split_once(',')
            .ok_or_else(|| invalid("malformed image data URI"))?;
        let mime = header.split(';').next().unwrap_or_default();
        if !["image/png", "image/jpeg", "image/jpg"]
            .iter()
            .any(|m| mime.eq_ignore_ascii_case(m))
        {
            return Err(invalid("only PNG and JPEG data URIs are supported"));
        }
        let payload = percent_decode(payload)?;
        if header.split(';').any(|s| s.eq_ignore_ascii_case("base64")) {
            let payload: Vec<_> = payload
                .into_iter()
                .filter(|b| !b.is_ascii_whitespace())
                .collect();
            return STANDARD
                .decode(payload)
                .map_err(|e| invalid(format!("invalid image base64: {e}")));
        }
        return Ok(payload);
    }
    if href.is_empty() || href.contains("://") || href.starts_with('#') {
        return Err(invalid(
            "image href must be a PNG/JPEG data URI or a local file path",
        ));
    }
    let path = String::from_utf8(percent_decode(href)?)
        .map_err(|_| invalid("image path must be UTF-8"))?;
    let path = base_dir.unwrap_or_else(|| Path::new(".")).join(path);
    fs::read(&path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("cannot read image '{}': {e}", path.display()),
        )
    })
}

fn percent_decode(value: &str) -> io::Result<Vec<u8>> {
    let mut output = Vec::with_capacity(value.len());
    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = bytes.next().and_then(|b| (b as char).to_digit(16));
            let low = bytes.next().and_then(|b| (b as char).to_digit(16));
            let (Some(high), Some(low)) = (high, low) else {
                return Err(invalid("invalid percent escape in image href"));
            };
            output.push((high * 16 + low) as u8);
        } else {
            output.push(byte);
        }
    }
    Ok(output)
}

impl RasterImage {
    fn infill(&self, spec: &InfillSpec) -> Vec<Polyline> {
        let subpaths = [self.outline.as_slice()];
        match spec.pattern {
            InfillPattern::Concentric => {
                let mut passes = vec![Polyline::from_vec(
                    self.outline
                        .iter()
                        .copied()
                        .chain(self.outline.first().copied())
                        .collect(),
                )];
                passes.extend(shape_concentric_infill(&subpaths, &[0], spec.step));
                passes
            }
            InfillPattern::Parallel => parallel_infill_all(&subpaths, spec.step, spec.angle_deg),
            InfillPattern::Cross => {
                let mut passes = parallel_infill_all(&subpaths, spec.step, spec.angle_deg);
                passes.extend(parallel_infill_all(
                    &subpaths,
                    spec.step,
                    spec.angle_deg + 90.0,
                ));
                passes
            }
            InfillPattern::Wavy => wavy_infill_all(
                &subpaths,
                spec.step,
                spec.angle_deg,
                spec.wave_amplitude,
                spec.wave_period,
            ),
        }
    }

    fn sample_pass(
        &self,
        pass: &Polyline,
        max_power: f64,
        max_line: f64,
        output: &mut Vec<PoweredPolyline>,
    ) {
        let mut points = Vec::new();
        let mut powers = Vec::new();
        for pair in pass.as_ref().windows(2) {
            let (ax, ay) = self.mm_to_pixel.apply(pair[0].x, pair[0].y);
            let (bx, by) = self.mm_to_pixel.apply(pair[1].x, pair[1].y);
            let Some((start, end)) = clip_segment(ax, ay, bx, by, self.bounds) else {
                finish_pass(&mut points, &mut powers, output);
                continue;
            };
            if end - start <= 1e-12 {
                continue;
            }
            let point_at = |t: f64| {
                CoordinatePair::new(
                    pair[0].x + (pair[1].x - pair[0].x) * t,
                    pair[0].y + (pair[1].y - pair[0].y) * t,
                )
            };
            let first = point_at(start);
            if points
                .last()
                .is_some_and(|p: &CoordinatePair| (p.x - first.x).hypot(p.y - first.y) > 1e-8)
            {
                finish_pass(&mut points, &mut powers, output);
            }
            if points.is_empty() {
                points.push(first);
            }

            let length = (pair[1].x - pair[0].x).hypot(pair[1].y - pair[0].y) * (end - start);
            let count = (length / max_line).ceil().max(1.0) as usize;
            let mut splits: Vec<_> = (0..=count)
                .map(|i| start + (end - start) * i as f64 / count as f64)
                .collect();
            // Split exactly at every source pixel edge, even for diagonal or
            // transformed passes. A midpoint then identifies the pixel covering
            // the whole movement rather than averaging across dark/blank pixels.
            for (a, b) in [(ax, bx), (ay, by)] {
                let delta = b - a;
                if delta.abs() <= 1e-12 {
                    continue;
                }
                let v0 = a + delta * start;
                let v1 = a + delta * end;
                let first_edge = v0.min(v1).floor() as i64 + 1;
                let last_edge = v0.max(v1).ceil() as i64 - 1;
                for edge in first_edge..=last_edge {
                    let t = (edge as f64 - a) / delta;
                    if t > start && t < end {
                        splits.push(t);
                    }
                }
            }
            splits.sort_by(f64::total_cmp);
            splits.dedup_by(|a, b| (*a - *b).abs() < 1e-12);
            for ts in splits.windows(2) {
                let t = (ts[0] + ts[1]) * 0.5;
                let x = (ax + (bx - ax) * t)
                    .floor()
                    .clamp(0.0, (self.pixels.width() - 1) as f64) as u32;
                let y = (ay + (by - ay) * t)
                    .floor()
                    .clamp(0.0, (self.pixels.height() - 1) as f64) as u32;
                let rgba = self.pixels.get_pixel(x, y).0.map(|v| v as f32 / 255.0);
                powers.push((rgba_to_power(rgba, max_power) * self.opacity).max(0.0));
                points.push(point_at(ts[1]));
            }
        }
        finish_pass(&mut points, &mut powers, output);
    }
}

/// Liang-Barsky clipping in image coordinates also keeps wavy passes inside
/// the image viewport; disconnected pieces become separate laser-off travels.
fn clip_segment(ax: f64, ay: f64, bx: f64, by: f64, bounds: Bbox) -> Option<(f64, f64)> {
    let mut start: f64 = 0.0;
    let mut end: f64 = 1.0;
    for (a, b, min, max) in [
        (ax, bx, bounds.min_x, bounds.max_x),
        (ay, by, bounds.min_y, bounds.max_y),
    ] {
        let delta = b - a;
        if delta.abs() < 1e-12 {
            if a < min - 1e-9 || a > max + 1e-9 {
                return None;
            }
        } else {
            let t0 = (min - a) / delta;
            let t1 = (max - a) / delta;
            start = start.max(t0.min(t1));
            end = end.min(t0.max(t1));
        }
    }
    (start <= end).then_some((start, end))
}

fn finish_pass(
    points: &mut Vec<CoordinatePair>,
    powers: &mut Vec<f64>,
    output: &mut Vec<PoweredPolyline>,
) {
    if let (Some(first), Some(last)) = (
        powers.iter().position(|p| *p > 0.0),
        powers.iter().rposition(|p| *p > 0.0),
    ) {
        output.push(PoweredPolyline {
            polyline: Polyline::from_vec(points[first..=last + 1].to_vec()),
            power: LaserPower::Segments(powers[first..=last].to_vec()),
        });
    }
    points.clear();
    powers.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, ImageFormat, Rgba};
    use std::io::Cursor;

    fn png_data(pixels: RgbaImage) -> String {
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(pixels)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        format!(
            "data:image/png;base64,{}",
            STANDARD.encode(bytes.into_inner())
        )
    }

    fn svg(body: &str) -> String {
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="10mm" height="10mm" viewBox="0 0 10 10">{body}</svg>"#
        )
    }

    fn spec(pattern: InfillPattern) -> InfillSpec {
        InfillSpec {
            step: 1.0,
            pattern,
            angle_deg: 0.0,
            wave_amplitude: 0.5,
            wave_period: 2.0,
        }
    }

    fn convert(source: &str, spec: &InfillSpec, max_line: f64) -> Vec<PoweredPolyline> {
        let doc = Document::parse(source).unwrap();
        image_toolpaths(
            &doc,
            &ImageContext {
                viewbox: &ViewBox::from_str("0 0 10 10").unwrap(),
                width_mm: 10.0,
                height_mm: 10.0,
                dpi: 96.0,
                font_size: 16.0,
                base_dir: None,
            },
            Some(spec),
            500.0,
            max_line,
        )
        .unwrap()
    }

    fn powers(pass: &PoweredPolyline) -> &[f64] {
        match &pass.power {
            LaserPower::Segments(powers) => powers,
            _ => panic!("image must have per-segment power"),
        }
    }

    #[test]
    fn png_pixels_change_power_within_a_pass_and_preserve_blank_gaps() {
        let pixels = RgbaImage::from_fn(7, 1, |x, _| {
            Rgba(match x {
                0 | 6 => [0, 0, 0, 255],
                1 => [128, 128, 128, 255],
                2 => [255, 255, 255, 255],
                3 => [0, 0, 0, 0],
                4 => [255, 0, 0, 255],
                _ => [0, 0, 0, 128],
            })
        });
        let source = svg(&format!(
            r#"<image width="7" height="1" href="{}"/>"#,
            png_data(pixels)
        ));
        let passes = convert(&source, &spec(InfillPattern::Parallel), 100.0);
        assert_eq!(passes.len(), 1);
        let values = powers(&passes[0]);
        let expected = [
            500.0,
            500.0 * (1.0 - 128.0 / 255.0),
            0.0,
            0.0,
            393.7,
            500.0 * 128.0 / 255.0,
            500.0,
        ];
        assert_eq!(values.len(), expected.len());
        for (actual, expected) in values.iter().zip(expected) {
            assert!((actual - expected).abs() < 1e-4, "{actual} != {expected}");
        }
        let gcode =
            crate::segmented_polyline2gcode(passes[0].polyline.clone(), values, 1000.0).unwrap();
        assert_eq!(gcode[0], "M5");
        assert_eq!(gcode[2], "M4 S0");
        assert_eq!(gcode[5], "G1 X3 Y0.5 F1000 S0");
        assert_eq!(gcode[6], "G1 X4 Y0.5 F1000 S0");
        assert_eq!(&gcode[gcode.len() - 2..], ["S0", "M5"]);
    }

    #[test]
    fn reversed_scanlines_sample_the_correct_pixels() {
        let pixels = RgbaImage::from_fn(3, 2, |x, _| Rgba([x as u8 * 80; 4]));
        // Use opaque grayscale pixels so direction is the only variable.
        let pixels = RgbaImage::from_fn(3, 2, |x, y| {
            let mut rgba = *pixels.get_pixel(x, y);
            rgba.0[3] = 255;
            rgba
        });
        let source = svg(&format!(
            r#"<image width="3" height="2" href="{}"/>"#,
            png_data(pixels)
        ));
        let passes = convert(&source, &spec(InfillPattern::Parallel), 100.0);
        assert_eq!(passes.len(), 2);
        assert_eq!(passes[0].polyline[0], CoordinatePair::new(0.0, 0.5));
        assert_eq!(passes[1].polyline[0], CoordinatePair::new(3.0, 1.5));
        assert_eq!(
            powers(&passes[0]),
            powers(&passes[1]).iter().copied().rev().collect::<Vec<_>>()
        );
    }

    #[test]
    fn image_and_parent_transforms_and_opacity_are_applied() {
        let data = png_data(RgbaImage::from_pixel(2, 1, Rgba([0, 0, 0, 255])));
        let source = svg(&format!(
            r#"<g transform="translate(2 3)" opacity="0.5"><image x="1" y="1" width="2" height="1" transform="scale(2)" opacity="1" style="opacity: 0.25" xlink:href="{data}"/></g>"#
        ));
        let passes = convert(&source, &spec(InfillPattern::Parallel), 100.0);
        assert_eq!(passes.len(), 2);
        assert_eq!(passes[0].polyline[0], CoordinatePair::new(4.0, 5.5));
        assert_eq!(
            *passes[0].polyline.as_ref().last().unwrap(),
            CoordinatePair::new(8.0, 5.5)
        );
        assert!(
            passes
                .iter()
                .flat_map(powers)
                .all(|p| (*p - 62.5).abs() < 1e-9)
        );
    }

    #[test]
    fn aspect_ratio_meet_slice_and_none_fit_the_viewport() {
        let data = png_data(RgbaImage::from_pixel(4, 2, Rgba([0, 0, 0, 255])));
        let image = |ratio: &str| {
            svg(&format!(
                r#"<image x="1" y="1" width="4" height="4" preserveAspectRatio="{ratio}" href="{data}"/>"#
            ))
        };
        let meet = convert(
            &image("xMidYMid meet"),
            &spec(InfillPattern::Parallel),
            100.0,
        );
        assert_eq!(meet.len(), 2);
        assert_eq!(meet[0].polyline[0], CoordinatePair::new(1.0, 2.5));
        let slice = convert(
            &image("xMidYMid slice"),
            &spec(InfillPattern::Parallel),
            100.0,
        );
        assert_eq!(slice.len(), 4);
        assert_eq!(slice[0].polyline[0], CoordinatePair::new(1.0, 1.5));
        assert_eq!(
            *slice[0].polyline.as_ref().last().unwrap(),
            CoordinatePair::new(5.0, 1.5)
        );
        let stretch = convert(&image("none"), &spec(InfillPattern::Parallel), 100.0);
        assert_eq!(stretch.len(), 4);
        assert_eq!(stretch[0].polyline[0], CoordinatePair::new(1.0, 1.5));
    }

    #[test]
    fn slice_crops_the_source_pixels_and_rotation_keeps_their_mapping() {
        let data = png_data(RgbaImage::from_fn(4, 1, |x, _| {
            let gray = x as u8 * 80;
            Rgba([gray, gray, gray, 255])
        }));
        let source = svg(&format!(
            r#"<image width="2" height="1" preserveAspectRatio="xMidYMid slice" href="{data}"/>"#
        ));
        let passes = convert(&source, &spec(InfillPattern::Parallel), 100.0);
        assert_eq!(passes.len(), 1);
        assert_eq!(powers(&passes[0]).len(), 2);
        for (actual, gray) in powers(&passes[0]).iter().zip([80.0, 160.0]) {
            assert!((actual - 500.0 * (1.0 - gray / 255.0)).abs() < 1e-4);
        }

        let data = png_data(RgbaImage::from_fn(3, 1, |x, _| {
            Rgba(match x {
                0 => [0, 0, 0, 255],
                1 => [255, 255, 255, 255],
                _ => [255, 0, 0, 255],
            })
        }));
        let source = svg(&format!(
            r#"<image width="3" height="1" transform="translate(5 1) rotate(90)" href="{data}"/>"#
        ));
        let passes = convert(&source, &spec(InfillPattern::Parallel), 100.0);
        assert_eq!(passes.len(), 2); // The middle, white row is skipped.
        assert!((passes[0].polyline[0].y - 1.5).abs() < 1e-8);
        assert!((passes[1].polyline[0].y - 3.5).abs() < 1e-8);
        assert!(powers(&passes[0]).iter().all(|p| (*p - 500.0).abs() < 1e-8));
        assert!(powers(&passes[1]).iter().all(|p| (*p - 393.7).abs() < 1e-8));
    }

    #[test]
    fn physical_and_percentage_image_dimensions_scale_to_mm() {
        let data = png_data(RgbaImage::from_pixel(2, 1, Rgba([0, 0, 0, 255])));
        let source = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="20mm" height="10mm" viewBox="0 0 200 100"><image x="10%" y="2mm" width="4mm" height="1mm" href="{data}" preserveAspectRatio="none"/></svg>"#
        );
        let doc = Document::parse(&source).unwrap();
        let mut spec = spec(InfillPattern::Parallel);
        spec.step = 0.1;
        let passes = image_toolpaths(
            &doc,
            &ImageContext {
                viewbox: &ViewBox::from_str("0 0 200 100").unwrap(),
                width_mm: 20.0,
                height_mm: 10.0,
                dpi: 96.0,
                font_size: 16.0,
                base_dir: None,
            },
            Some(&spec),
            500.0,
            100.0,
        )
        .unwrap();
        assert_eq!(passes.len(), 4);
        assert_eq!(passes[0].polyline[0].x, 2.0);
        assert!((passes[0].polyline[0].y - (2.0 * 96.0 / 25.4 * 0.1 + 0.05)).abs() < 1e-8);
        assert!(
            (passes[0].polyline.as_ref().last().unwrap().x - (2.0 + 4.0 * 96.0 / 25.4 * 0.1)).abs()
                < 1e-8
        );
    }

    #[test]
    fn unsupported_rendering_effects_are_reported() {
        for effect in ["clip-path", "mask", "filter"] {
            let source = svg(&format!(
                r#"<g {effect}="url(#effect)"><image width="2" height="2" href="missing.png"/></g>"#
            ));
            let doc = Document::parse(&source).unwrap();
            let context = ImageContext {
                viewbox: &ViewBox::from_str("0 0 10 10").unwrap(),
                width_mm: 10.0,
                height_mm: 10.0,
                dpi: 96.0,
                font_size: 16.0,
                base_dir: None,
            };
            let error = image_toolpaths(
                &doc,
                &context,
                Some(&spec(InfillPattern::Parallel)),
                500.0,
                1.0,
            )
            .unwrap_err();
            assert!(error.to_string().contains(effect));
        }
    }

    #[test]
    fn all_patterns_sample_pixels_and_wavy_paths_are_clipped() {
        let data = png_data(RgbaImage::from_pixel(4, 4, Rgba([0, 0, 0, 255])));
        let source = svg(&format!(
            r#"<image x="1" y="1" width="4" height="4" href="{data}"/>"#
        ));
        for pattern in [
            InfillPattern::Concentric,
            InfillPattern::Parallel,
            InfillPattern::Cross,
            InfillPattern::Wavy,
        ] {
            let mut spec = spec(pattern);
            spec.angle_deg = 30.0;
            spec.wave_amplitude = 2.0;
            let passes = convert(&source, &spec, 0.25);
            assert!(!passes.is_empty(), "{pattern:?}");
            for pass in passes {
                assert_eq!(powers(&pass).len(), pass.polyline.as_ref().len() - 1);
                for point in pass.polyline.as_ref() {
                    assert!(
                        (1.0 - 1e-8..=5.0 + 1e-8).contains(&point.x),
                        "{pattern:?}: {point:?}"
                    );
                    assert!(
                        (1.0 - 1e-8..=5.0 + 1e-8).contains(&point.y),
                        "{pattern:?}: {point:?}"
                    );
                }
                for pair in pass.polyline.as_ref().windows(2) {
                    assert!((pair[1].x - pair[0].x).hypot(pair[1].y - pair[0].y) <= 0.25 + 1e-8);
                }
            }
        }
    }

    #[test]
    fn white_transparent_hidden_and_definition_images_are_skipped() {
        for rgba in [[255, 255, 255, 255], [0, 0, 0, 0]] {
            let source = svg(&format!(
                r#"<image width="2" height="2" href="{}"/>"#,
                png_data(RgbaImage::from_pixel(2, 2, Rgba(rgba)))
            ));
            assert!(convert(&source, &spec(InfillPattern::Parallel), 1.0).is_empty());
        }
        // These images should not even attempt to open the missing resource.
        for body in [
            r#"<defs><image width="2" height="2" href="missing.png"/></defs>"#,
            r#"<g display="none"><image width="2" height="2" href="missing.png"/></g>"#,
            r#"<image opacity="0" width="2" height="2" href="missing.png"/>"#,
            r#"<g visibility="hidden"><image width="2" height="2" href="missing.png"/></g>"#,
        ] {
            assert!(convert(&svg(body), &spec(InfillPattern::Parallel), 1.0).is_empty());
        }
    }

    #[test]
    fn jpeg_data_is_decoded_and_converted_to_power() {
        let pixels = image::RgbImage::from_pixel(2, 2, image::Rgb([128, 128, 128]));
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(pixels)
            .write_to(&mut bytes, ImageFormat::Jpeg)
            .unwrap();
        let source = svg(&format!(
            r#"<image width="2" height="2" href="data:image/jpeg;base64,{}"/>"#,
            STANDARD.encode(bytes.into_inner())
        ));
        let passes = convert(&source, &spec(InfillPattern::Parallel), 100.0);
        assert_eq!(passes.len(), 2);
        assert!(
            passes
                .iter()
                .flat_map(powers)
                .all(|p| (*p - 249.0).abs() < 3.0)
        );
    }

    #[test]
    fn malformed_images_fail_instead_of_becoming_full_power() {
        for href in [
            "data:image/png;base64,%%%",
            "data:image/jpeg;base64,AAAA",
            "https://example.com/image.png",
        ] {
            let source = svg(&format!(
                r#"<image id="bad" width="2" height="2" href="{href}"/>"#
            ));
            let doc = Document::parse(&source).unwrap();
            let viewbox = ViewBox::from_str("0 0 10 10").unwrap();
            let error = image_toolpaths(
                &doc,
                &ImageContext {
                    viewbox: &viewbox,
                    width_mm: 10.0,
                    height_mm: 10.0,
                    dpi: 96.0,
                    font_size: 16.0,
                    base_dir: None,
                },
                Some(&spec(InfillPattern::Parallel)),
                500.0,
                1.0,
            )
            .unwrap_err();
            assert!(error.to_string().contains("image #bad"));
        }
    }

    #[test]
    fn percent_encoded_image_data_and_whitespace_in_base64_are_supported() {
        assert_eq!(
            image_bytes("data:image/png,%89PNG%0d%0a", None).unwrap(),
            b"\x89PNG\r\n"
        );
        assert_eq!(
            image_bytes("data:image/png;base64,Y W\nJj", None).unwrap(),
            b"abc"
        );
    }
}
