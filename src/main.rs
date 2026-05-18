use cavalier_contours::polyline::{
    PlineSource, PlineSourceMut, PlineVertex, Polyline as CcPolyline,
};
use cavalier_contours::shape_algorithms::{Shape, ShapeOffsetOptions};
use clap::{ArgAction, Parser, ValueEnum};
use log::{info, warn};
use roxmltree::Document;
use std::collections::HashMap;
use std::str::FromStr;
use std::{
    fs::File,
    io::{self, Read},
    path::PathBuf,
};
use svg2polylines::{CoordinatePair, Polyline, StyledPath};
use svgtypes::{Length, LengthUnit, TransformListParser, TransformListToken, ViewBox};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Location of SVG File
    #[arg(short, long)]
    svg: Option<PathBuf>,

    /// Preprocess the SVG tree, normalizing transforms
    #[arg(short, long, action = ArgAction::SetTrue)]
    preprocess: bool,

    /// Reset origin to the bottom-left of the drawing
    #[arg(short, long, action = ArgAction::SetTrue)]
    reset_origin: bool,

    /// Maximum permitted variation between the linear line and the path
    #[arg(short, long, default_value_t = 0.15)]
    tolerance: f64,

    /// The maximum size of a linear line - smaller ensures smoother transition between strengths
    #[arg(short, long, default_value_t = 2.0)]
    max_line: f64,

    /// Maximum engraving strength — the `S` value used for a fully-black,
    /// fully-opaque polyline. White maps to S0 (skipped), and other colors
    /// are interpolated by Rec.709 grayscale luminance scaled by alpha.
    /// Find your machine's true max with `$$` in the console (the `$30`
    /// value).
    #[arg(long, default_value_t = 500.0)]
    strength: f64,
    
    /// Speed of the engraver (`S` argument during engraving movements - found using `$$` in the machine's console, and looking at the `$110` and `$111` values)
    #[arg(long, default_value_t = 500.0)]
    speed: f64,

    /// DPI - pixels per inch (if using pixels in your SVG file's width or height)
    #[arg(long, default_value_t = 96.0)]
    dpi: f64,

    /// Font Size in pixels (if using font-based sizes in your SVG file's width or height)
    #[arg(long, default_value_t = 16.0)]
    font_size: f64,

    /// Spacing (in mm) between infill passes for filled paths. Holes are
    /// detected via subpath containment (so glyphs with holes work
    /// correctly), and concentric offsets are robust on non-convex outlines.
    /// If unset, no infill is generated.
    #[arg(long)]
    infill: Option<f64>,

    /// Infill pattern to use when `--infill` is set.
    #[arg(long, value_enum, default_value_t = InfillPattern::Concentric)]
    infill_pattern: InfillPattern,

    /// Angle (degrees, CCW from +X) for parallel / cross / wavy infill.
    /// Ignored for concentric. Cross-hatch lays a second pass at this
    /// angle + 90°.
    #[arg(long, default_value_t = 0.0)]
    infill_angle: f64,

    /// Wave amplitude (mm) for the wavy pattern. Defaults to the infill
    /// step. Ignored for other patterns.
    #[arg(long)]
    infill_wave_amplitude: Option<f64>,

    /// Wave period (mm) for the wavy pattern. Defaults to 4× the infill
    /// step. Ignored for other patterns.
    #[arg(long)]
    infill_wave_period: Option<f64>,

    // TODO: Fill type? Gradients? etc
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum InfillPattern {
    /// Concentric rings stepping inward from the boundary.
    Concentric,
    /// Parallel straight lines at `--infill-angle`.
    Parallel,
    /// Two perpendicular sets of parallel lines.
    Cross,
    /// Parallel lines that wobble along a sine wave.
    Wavy,
}

fn main() -> io::Result<()> {
    env_logger::init();
    let args = Args::parse();

    let mut svg_source = open_input(args.svg)?;
    let mut svg_buf = String::new();
    svg_source.read_to_string(&mut svg_buf)?;

    info!("Parsing SVG document for viewBox and dimension extraction");
    let doc_options = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..roxmltree::ParsingOptions::default()
    };
    let doc = Document::parse_with_options(&svg_buf, doc_options).map_err(io::Error::other)?;
    info!("SVG parsed");

    let svg_node = doc
        .descendants()
        .find(|n| n.has_tag_name("svg"))
        .ok_or(io::Error::new(io::ErrorKind::Other, "no <svg> element found"))?;

    info!("Parsing viewBox value");
    let viewbox = svg_node.attribute("viewBox")
        .ok_or(io::Error::new(io::ErrorKind::Other, "no viewBox in <svg>"))
        .and_then(|vb| ViewBox::from_str(vb).map_err(io::Error::other))?;
    info!("Extracting width");
    let width = svg_node.attribute("width").map(|w| length_to_mm(w, args.dpi, args.font_size)).transpose().map_err(io::Error::other).map(|w| w.unwrap_or(viewbox.w))?;
    info!("Extracting height");
    let height = svg_node.attribute("height").map(|h| length_to_mm(h, args.dpi, args.font_size)).transpose().map_err(io::Error::other).map(|h| h.unwrap_or(viewbox.h))?;

    let paths: Vec<StyledPath> = svg2polylines::parse_paths(
        &svg_buf,
        args.tolerance,
        args.preprocess,
    )
    .map_err(io::Error::other)?;

    let mut gcodes: Vec<String> = vec![
        "G21".to_owned(), // use millimeters
        "G90".to_owned(), // absolute positioning
        "G94".to_owned(), // speed = mm/min
        "M5".to_owned(), // make sure laser is disengaged
        "G28".to_owned(), // move to stored home
    ];

    // Normalize coordinates of every polyline within every path to mm.
    let normalize = |p: CoordinatePair| CoordinatePair {
        x: (p.x / viewbox.w) * width,
        y: (p.y / viewbox.h) * height,
    };
    let paths: Vec<StyledPath> = paths
        .into_iter()
        .map(|sp| StyledPath {
            polylines: sp
                .polylines
                .into_iter()
                .map(|pl| Polyline::from_vec(pl.unwrap().into_iter().map(normalize).collect()))
                .collect(),
            style: sp.style,
        })
        .collect();

    // Bundle the infill knobs into a single spec, applying defaults for the
    // wavy pattern's amplitude/period that depend on the step itself.
    let infill_spec: Option<InfillSpec> = args.infill.filter(|s| *s > 0.0).map(|step| InfillSpec {
        step,
        pattern: args.infill_pattern,
        angle_deg: args.infill_angle,
        wave_amplitude: args.infill_wave_amplitude.unwrap_or(step),
        wave_period: args.infill_wave_period.unwrap_or(step * 4.0),
    });

    // Parse all gradient elements in the document so url(#id) references on
    // fills/strokes can be resolved. Done after we've parsed viewBox so we
    // can normalise userSpaceOnUse coords into the same mm space as the
    // polylines.
    let gradients = parse_gradients(&doc, &viewbox, width, height);

    // Expand each StyledPath into the polylines to engrave, each tagged
    // with the laser power derived from its source color (or gradient
    // sampled at the polyline's centroid).
    let polylines: Vec<PoweredPolyline> = paths
        .into_iter()
        .flat_map(|sp| {
            expand_path_with_infill(sp, infill_spec.as_ref(), args.strength, &gradients)
        })
        .collect();

    let points: Vec<CoordinatePair> = polylines
        .iter()
        .flat_map(|pp| pp.polyline.as_ref().iter().copied())
        .collect();

    let min_x = points.iter().map(|p| p.x).fold(f64::INFINITY, f64::min);
    let max_y = points.iter().map(|p| p.y).fold(f64::NEG_INFINITY, f64::max);

    for pp in polylines.into_iter() {
        let polyline = Polyline::from_vec(
            pp.polyline
                .unwrap()
                .into_iter()
                .map(|p| if args.reset_origin {
                    CoordinatePair { x: p.x - min_x, y: max_y - p.y }
                } else {
                    CoordinatePair { x: p.x, y: height - p.y }
                })
                .collect()
        );
        let mut gcodes_to_append = polyline2gcode(polyline, pp.power, args.speed)?;
        gcodes.append(&mut gcodes_to_append);
    }

    gcodes.push("M5".to_owned());
    gcodes.push("G28".to_owned());

    println!("{}", gcodes.join("\n"));

    Ok(())
}

fn open_input(path: Option<PathBuf>) -> io::Result<Box<dyn Read>> {
    match path {
        None => Ok(Box::new(io::stdin())),
        Some(path) => Ok(Box::new(File::open(path)?)),
    }
}

fn polyline2gcode(polyline: Polyline, strength: f64, speed: f64) -> io::Result<Vec<String>> {
    let coordinates: Vec<CoordinatePair> = polyline.unwrap();
    
    if coordinates.len() < 2 {
        return Err(
            io::Error::new(
                io::ErrorKind::Other,
                format!("Polyline with less than 2 coordinates: {coordinates:?}")
            )
        );
    }

    let mut gcodes = vec![];
    gcodes.push("M4".to_owned());
    let first_coord = coordinates[0];
    gcodes.push(format!("G0 X{} Y{}", first_coord.x, first_coord.y));
    for next_coord in &coordinates[1..] {
        gcodes.push(format!("G1 X{} Y{} F{speed} S{strength}", next_coord.x, next_coord.y));
    }
    gcodes.push("M5".to_owned());

    Ok(gcodes)
}

/// All the knobs the infill stage needs, bundled so we can pass one
/// `Option<&InfillSpec>` around instead of half a dozen parameters.
#[derive(Debug, Clone)]
struct InfillSpec {
    step: f64,
    pattern: InfillPattern,
    angle_deg: f64,
    wave_amplitude: f64,
    wave_period: f64,
}

/// A polyline together with the laser-S value to engrave it at. Polylines
/// whose computed power is non-positive are filtered out before this struct
/// is built, so `power > 0.0` is an invariant for anything that reaches the
/// G-code emitter.
#[derive(Debug, Clone)]
struct PoweredPolyline {
    polyline: Polyline,
    power: f64,
}

/// Where a gradient's geometric attributes (`x1`, `cx`, etc.) live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GradientUnits {
    /// Default per the SVG spec: coordinates are 0..1 within the path's bbox.
    ObjectBoundingBox,
    /// Coordinates are in user space (mm here, since we normalise both
    /// gradient and polyline coordinates the same way).
    UserSpaceOnUse,
}

/// What happens for sample offsets outside `[0, 1]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpreadMethod {
    /// Clamp to `[0, 1]`.
    Pad,
    /// Mirror across each integer boundary.
    Reflect,
    /// Wrap (modulo 1).
    Repeat,
}

#[derive(Debug, Clone, Copy)]
struct GradientStop {
    offset: f64,
    /// Pre-resolved RGBA in `[0, 1]`. Stored unpacked rather than as a
    /// `csscolorparser::Color` so the struct is `Copy`.
    rgba: [f32; 4],
}

#[derive(Debug, Clone, Copy)]
enum GradientShape {
    Linear { x1: f64, y1: f64, x2: f64, y2: f64 },
    Radial {
        cx: f64,
        cy: f64,
        fx: f64,
        fy: f64,
        r: f64,
    },
}

#[derive(Debug, Clone)]
struct Gradient {
    shape: GradientShape,
    stops: Vec<GradientStop>,
    units: GradientUnits,
    spread: SpreadMethod,
    /// `gradientTransform` mapping from the gradient's local coordinate
    /// system to the target system (mm for userSpaceOnUse, or bbox-unit for
    /// objectBoundingBox). Identity if none was specified.
    transform: Affine,
}

/// Minimal 2D affine transform. `p' = (a*x + c*y + e, b*x + d*y + f)`.
#[derive(Debug, Clone, Copy)]
struct Affine {
    a: f64,
    b: f64,
    c: f64,
    d: f64,
    e: f64,
    f: f64,
}

impl Affine {
    const fn identity() -> Self {
        Self {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: 0.0,
            f: 0.0,
        }
    }

    fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }

    /// Compose `self * other`: applying the result to a point is equivalent
    /// to applying `other` first, then `self`. This matches SVG's
    /// left-to-right transform-list semantics: for `"translate(...) rotate(...)"`
    /// we want translate-outer, rotate-inner, so we fold tokens by
    /// `M = M * token`.
    fn compose(&self, other: &Affine) -> Affine {
        Affine {
            a: self.a * other.a + self.c * other.b,
            b: self.b * other.a + self.d * other.b,
            c: self.a * other.c + self.c * other.d,
            d: self.b * other.c + self.d * other.d,
            e: self.a * other.e + self.c * other.f + self.e,
            f: self.b * other.e + self.d * other.f + self.f,
        }
    }

    /// Returns None for a singular (zero-determinant) transform.
    fn invert(&self) -> Option<Affine> {
        let det = self.a * self.d - self.b * self.c;
        if det.abs() < 1e-12 {
            return None;
        }
        Some(Affine {
            a: self.d / det,
            b: -self.b / det,
            c: -self.c / det,
            d: self.a / det,
            e: (self.c * self.f - self.d * self.e) / det,
            f: (self.b * self.e - self.a * self.f) / det,
        })
    }
}

/// Parse a CSS/SVG transform-list string (`"translate(10) rotate(45)"`,
/// `"matrix(a b c d e f)"`, etc.) into a single affine.
fn parse_gradient_transform(s: &str) -> Affine {
    let mut m = Affine::identity();
    for tok in TransformListParser::from(s) {
        let tok = match tok {
            Ok(t) => t,
            Err(e) => {
                warn!("Skipping malformed gradientTransform token: {}", e);
                break;
            }
        };
        let t = match tok {
            TransformListToken::Matrix { a, b, c, d, e, f } => Affine { a, b, c, d, e, f },
            TransformListToken::Translate { tx, ty } => Affine {
                a: 1.0,
                b: 0.0,
                c: 0.0,
                d: 1.0,
                e: tx,
                f: ty,
            },
            TransformListToken::Scale { sx, sy } => Affine {
                a: sx,
                b: 0.0,
                c: 0.0,
                d: sy,
                e: 0.0,
                f: 0.0,
            },
            TransformListToken::Rotate { angle } => {
                let r = angle.to_radians();
                let (s, c) = (r.sin(), r.cos());
                Affine {
                    a: c,
                    b: s,
                    c: -s,
                    d: c,
                    e: 0.0,
                    f: 0.0,
                }
            }
            TransformListToken::SkewX { angle } => Affine {
                a: 1.0,
                b: 0.0,
                c: angle.to_radians().tan(),
                d: 1.0,
                e: 0.0,
                f: 0.0,
            },
            TransformListToken::SkewY { angle } => Affine {
                a: 1.0,
                b: angle.to_radians().tan(),
                c: 0.0,
                d: 1.0,
                e: 0.0,
                f: 0.0,
            },
        };
        m = m.compose(&t);
    }
    m
}

type GradientRegistry = HashMap<String, Gradient>;

/// Everything `power_from_color` needs to resolve a CSS color — including
/// gradient lookups, the path's bounding box for `objectBoundingBox` units,
/// and the sample point on the polyline (its centroid in mm).
struct ColorEnv<'a> {
    gradients: &'a GradientRegistry,
    bbox: Bbox,
    sample: CoordinatePair,
}

/// Axis-aligned bounding box. `(min_x, min_y, max_x, max_y)`.
#[derive(Debug, Clone, Copy)]
struct Bbox {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

impl Bbox {
    fn width(&self) -> f64 {
        self.max_x - self.min_x
    }
    fn height(&self) -> f64 {
        self.max_y - self.min_y
    }
}

/// Compute the axis-aligned bbox of all vertices across the given polylines.
/// Returns None if there are no vertices.
fn polylines_bbox(polylines: &[Polyline]) -> Option<Bbox> {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for pl in polylines {
        for p in pl.as_ref() {
            if p.x < min_x {
                min_x = p.x;
            }
            if p.x > max_x {
                max_x = p.x;
            }
            if p.y < min_y {
                min_y = p.y;
            }
            if p.y > max_y {
                max_y = p.y;
            }
        }
    }
    if !min_x.is_finite() {
        None
    } else {
        Some(Bbox {
            min_x,
            min_y,
            max_x,
            max_y,
        })
    }
}

/// Arithmetic mean of a polyline's vertices. Sufficient as the "center of
/// the polyline" sample point for both short two-point scan-line segments
/// (where it equals the midpoint) and longer concentric rings. If the
/// polyline is closed (its last vertex repeats the first), the closure
/// vertex is excluded so it doesn't bias the mean toward the start vertex.
fn polyline_centroid(pl: &Polyline) -> CoordinatePair {
    let pts = pl.as_ref();
    if pts.is_empty() {
        return CoordinatePair::new(0.0, 0.0);
    }
    let n_full = pts.len();
    let count = if n_full >= 2 && pts_eq(pts[0], pts[n_full - 1]) {
        n_full - 1
    } else {
        n_full
    };
    let mut sx = 0.0;
    let mut sy = 0.0;
    for p in &pts[..count] {
        sx += p.x;
        sy += p.y;
    }
    let n = count as f64;
    CoordinatePair::new(sx / n, sy / n)
}

/// Apply a spread method to a raw gradient offset `t`, returning a value
/// in `[0, 1]`.
fn apply_spread(t: f64, spread: SpreadMethod) -> f64 {
    match spread {
        SpreadMethod::Pad => t.clamp(0.0, 1.0),
        SpreadMethod::Repeat => t.rem_euclid(1.0),
        SpreadMethod::Reflect => {
            let two = t.rem_euclid(2.0);
            if two <= 1.0 { two } else { 2.0 - two }
        }
    }
}

/// Linearly interpolate between sorted-by-offset stops at parameter `t`
/// (already in `[0, 1]`). Stops are assumed non-empty; the caller handles
/// degenerate cases.
fn interpolate_stops(stops: &[GradientStop], t: f64) -> [f32; 4] {
    if t <= stops.first().expect("non-empty").offset {
        return stops.first().unwrap().rgba;
    }
    if t >= stops.last().unwrap().offset {
        return stops.last().unwrap().rgba;
    }
    let pos = stops.windows(2).find(|w| t >= w[0].offset && t <= w[1].offset);
    let (a, b) = match pos {
        Some(w) => (w[0], w[1]),
        None => return stops.last().unwrap().rgba,
    };
    let span = b.offset - a.offset;
    let u = if span > 0.0 { (t - a.offset) / span } else { 0.0 };
    let lerp = |x: f32, y: f32| (x as f64 + (y - x) as f64 * u) as f32;
    [
        lerp(a.rgba[0], b.rgba[0]),
        lerp(a.rgba[1], b.rgba[1]),
        lerp(a.rgba[2], b.rgba[2]),
        lerp(a.rgba[3], b.rgba[3]),
    ]
}

/// Sample a gradient at the given environment's `sample` point. The point
/// and the gradient must live in the same coordinate system — for
/// `objectBoundingBox` we normalise the sample point to `[0, 1]` within the
/// path bbox first.
fn sample_gradient(g: &Gradient, env: &ColorEnv) -> Option<[f32; 4]> {
    if g.stops.is_empty() {
        return None;
    }
    // Express the sample in the target coord system (mm for userSpaceOnUse,
    // bbox-unit for objectBoundingBox).
    let (sx, sy) = match g.units {
        GradientUnits::UserSpaceOnUse => (env.sample.x, env.sample.y),
        GradientUnits::ObjectBoundingBox => {
            let w = env.bbox.width();
            let h = env.bbox.height();
            if w <= 0.0 || h <= 0.0 {
                return Some(g.stops[0].rgba);
            }
            (
                (env.sample.x - env.bbox.min_x) / w,
                (env.sample.y - env.bbox.min_y) / h,
            )
        }
    };

    // gradientTransform maps gradient-local coords into the target system.
    // To evaluate the gradient at a target-system sample, inverse-transform
    // back to gradient-local first. If the transform is singular fall back
    // to identity (logged warning at parse time would be ideal but we keep
    // the sampler quiet).
    let inv = g.transform.invert().unwrap_or_else(Affine::identity);
    let (gx, gy) = inv.apply(sx, sy);

    let t = match g.shape {
        GradientShape::Linear { x1, y1, x2, y2 } => {
            let dx = x2 - x1;
            let dy = y2 - y1;
            let len_sq = dx * dx + dy * dy;
            if len_sq <= 0.0 {
                0.0
            } else {
                ((gx - x1) * dx + (gy - y1) * dy) / len_sq
            }
        }
        GradientShape::Radial { cx, cy, fx, fy, r } => {
            radial_offset(gx, gy, cx, cy, fx, fy, r)
        }
    };
    let t = apply_spread(t, g.spread);
    Some(interpolate_stops(&g.stops, t))
}

/// SVG 1.1 radial gradient evaluation: the gradient parameter at point `P`
/// is `|FP| / |FQ|` where `F = (fx, fy)` is the focal point and `Q` is the
/// intersection of the ray from `F` through `P` with the bounding circle
/// `(cx, cy, r)`. If `F` lies outside the bounding circle, the spec says
/// to move it onto the boundary — we approximate by clamping `F` to the
/// boundary along the `C → F` direction before computing.
fn radial_offset(px: f64, py: f64, cx: f64, cy: f64, mut fx: f64, mut fy: f64, r: f64) -> f64 {
    if r <= 0.0 {
        return 0.0;
    }
    // Clamp F to the boundary circle when it's outside.
    let fcx = fx - cx;
    let fcy = fy - cy;
    let fc_len = (fcx * fcx + fcy * fcy).sqrt();
    if fc_len > r {
        let scale = (r * 0.999) / fc_len;
        fx = cx + fcx * scale;
        fy = cy + fcy * scale;
    }
    let dx = px - fx;
    let dy = py - fy;
    let a = dx * dx + dy * dy;
    if a <= 0.0 {
        // Sample point coincides with the focal point.
        return 0.0;
    }
    let gx = cx - fx;
    let gy = cy - fy;
    let dg = dx * gx + dy * gy;
    let gg = gx * gx + gy * gy;
    let c_coef = gg - r * r; // <= 0 since F is inside (we clamped above)
    let disc = dg * dg - a * c_coef;
    if disc < 0.0 {
        return 0.0;
    }
    // Take the positive root: the ray crosses the boundary in the +d
    // direction at distance s = (dg + sqrt(disc))/a along (dx, dy). Since
    // |FQ| = s * |d| and |FP| = |d|, the offset is 1/s.
    let s_q = (dg + disc.sqrt()) / a;
    if s_q <= 0.0 {
        return 0.0;
    }
    1.0 / s_q
}

/// Convert a CSS color string into engraving power. SVG semantics:
///   * a missing `fill` defaults to black; a missing `stroke` defaults to
///     none. Callers handle that — here, `None` is interpreted as black so
///     the function does the right thing for fill defaults.
///   * `"none"` / `"transparent"` map to 0 power (the caller should already
///     have stripped these via `has_fill`/`has_stroke`, so reaching this
///     branch is defensive).
///   * `"url(#id)"` resolves the referenced gradient at the env's sample
///     point. Unknown ids and non-gradient `url(...)` references warn and
///     fall back to solid black.
///   * Anything csscolorparser can't parse warns and returns 0.
///
/// The mapping is `(1 - luminance) * alpha * max_power`, using Rec.709
/// luminance weights. Black + opaque → `max_power`; white or fully
/// transparent → 0.
fn power_from_color(color: Option<&str>, max_power: f64, env: &ColorEnv) -> f64 {
    let color = color.unwrap_or("black");
    let trimmed = color.trim();
    if trimmed.eq_ignore_ascii_case("none") || trimmed.eq_ignore_ascii_case("transparent") {
        return 0.0;
    }
    if let Some(id) = parse_url_ref(trimmed) {
        match env.gradients.get(id) {
            Some(grad) => match sample_gradient(grad, env) {
                Some(rgba) => return rgba_to_power(rgba, max_power),
                None => {
                    warn!("Gradient '#{}' has no stops; treating as black", id);
                    return max_power;
                }
            },
            None => {
                warn!(
                    "Unknown url() reference '{}' (likely a pattern or non-gradient paint); treating as black",
                    trimmed
                );
                return max_power;
            }
        }
    }
    match csscolorparser::parse(trimmed) {
        Ok(c) => rgba_to_power([c.r, c.g, c.b, c.a], max_power),
        Err(e) => {
            warn!("Could not parse color '{}': {}; skipping polyline", trimmed, e);
            0.0
        }
    }
}

/// Rec.709 luminance → engraving power for a pre-unpacked RGBA tuple.
fn rgba_to_power(rgba: [f32; 4], max_power: f64) -> f64 {
    let (r, g, b, a) = (
        rgba[0] as f64,
        rgba[1] as f64,
        rgba[2] as f64,
        rgba[3] as f64,
    );
    let luminance = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    (1.0 - luminance) * a * max_power
}

/// Extract the `id` from a CSS-style `url(#id)` reference. Returns `None`
/// if the input doesn't match that shape.
fn parse_url_ref(s: &str) -> Option<&str> {
    let rest = s.strip_prefix("url(")?.strip_suffix(')')?.trim();
    // Strip optional quotes.
    let rest = rest.strip_prefix('"').and_then(|r| r.strip_suffix('"'))
        .or_else(|| rest.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')))
        .unwrap_or(rest);
    rest.strip_prefix('#')
}

/// Intermediate per-element data captured during the first pass over the
/// XML tree. Bare-string attributes; defaults and href inheritance are
/// resolved in a second pass.
#[derive(Debug, Default, Clone)]
struct RawGradient {
    is_linear: bool,
    href: Option<String>,
    units: Option<GradientUnits>,
    spread: Option<SpreadMethod>,
    x1: Option<f64>,
    y1: Option<f64>,
    x2: Option<f64>,
    y2: Option<f64>,
    cx: Option<f64>,
    cy: Option<f64>,
    fx: Option<f64>,
    fy: Option<f64>,
    r: Option<f64>,
    own_stops: Option<Vec<GradientStop>>,
    /// Raw `gradientTransform` string, parsed lazily during resolution so
    /// child gradients can inherit a parent's transform via `href`.
    transform: Option<String>,
}

/// Walk the SVG tree, collect every `<linearGradient>` / `<radialGradient>`
/// into a registry, resolving `xlink:href` / `href` inheritance and
/// normalising `userSpaceOnUse` coordinates to mm using the same scale
/// factor we apply to polylines.
fn parse_gradients(
    doc: &Document,
    viewbox: &ViewBox,
    width_mm: f64,
    height_mm: f64,
) -> GradientRegistry {
    // First pass: raw data per id.
    let mut raw: HashMap<String, RawGradient> = HashMap::new();
    for node in doc.descendants() {
        let name = node.tag_name().name();
        let is_linear = name == "linearGradient";
        let is_radial = name == "radialGradient";
        if !is_linear && !is_radial {
            continue;
        }
        let id = match node.attribute("id") {
            Some(id) => id.to_string(),
            None => continue,
        };
        let mut g = RawGradient::default();
        g.is_linear = is_linear;
        g.href = node
            .attribute("href")
            .or_else(|| node.attribute(("http://www.w3.org/1999/xlink", "href")))
            .and_then(|h| h.strip_prefix('#'))
            .map(str::to_string);
        g.units = node.attribute("gradientUnits").and_then(|s| match s {
            "userSpaceOnUse" => Some(GradientUnits::UserSpaceOnUse),
            "objectBoundingBox" => Some(GradientUnits::ObjectBoundingBox),
            _ => None,
        });
        g.spread = node.attribute("spreadMethod").and_then(|s| match s {
            "pad" => Some(SpreadMethod::Pad),
            "reflect" => Some(SpreadMethod::Reflect),
            "repeat" => Some(SpreadMethod::Repeat),
            _ => None,
        });
        g.transform = node.attribute("gradientTransform").map(str::to_string);
        let parse_num = |s: &str| -> Option<f64> {
            if let Some(p) = s.strip_suffix('%') {
                p.trim().parse::<f64>().ok().map(|n| n / 100.0)
            } else {
                s.trim().parse::<f64>().ok()
            }
        };
        g.x1 = node.attribute("x1").and_then(parse_num);
        g.y1 = node.attribute("y1").and_then(parse_num);
        g.x2 = node.attribute("x2").and_then(parse_num);
        g.y2 = node.attribute("y2").and_then(parse_num);
        g.cx = node.attribute("cx").and_then(parse_num);
        g.cy = node.attribute("cy").and_then(parse_num);
        g.fx = node.attribute("fx").and_then(parse_num);
        g.fy = node.attribute("fy").and_then(parse_num);
        g.r = node.attribute("r").and_then(parse_num);
        let stops = parse_stops(node);
        if !stops.is_empty() {
            g.own_stops = Some(stops);
        }
        raw.insert(id, g);
    }

    // Second pass: collapse each gradient's href chain into a single
    // RawGradient by following parents iteratively. Per SVG 1.1, every
    // gradient attribute (including gradientTransform) inherits via href.
    // We use `Option::or` so this-set values win over inherited ones, and
    // a `visited` set to break cycles.
    let original = raw.clone();
    let ids: Vec<String> = raw.keys().cloned().collect();
    for id in &ids {
        let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
        visited.insert(id.clone());
        let mut next_href = raw.get(id).and_then(|g| g.href.clone());
        while let Some(parent_id) = next_href {
            if !visited.insert(parent_id.clone()) {
                warn!(
                    "Cycle in gradient href chain involving '#{}'; stopping resolution",
                    parent_id
                );
                break;
            }
            let parent = match original.get(&parent_id) {
                Some(p) => p,
                None => {
                    warn!(
                        "Gradient '#{}' references unknown '#{}'; stopping resolution",
                        id, parent_id
                    );
                    break;
                }
            };
            let g = raw.get_mut(id).expect("id from keys");
            if g.own_stops.is_none() {
                g.own_stops = parent.own_stops.clone();
            }
            if g.units.is_none() {
                g.units = parent.units;
            }
            if g.spread.is_none() {
                g.spread = parent.spread;
            }
            if g.transform.is_none() {
                g.transform = parent.transform.clone();
            }
            g.x1 = g.x1.or(parent.x1);
            g.y1 = g.y1.or(parent.y1);
            g.x2 = g.x2.or(parent.x2);
            g.y2 = g.y2.or(parent.y2);
            g.cx = g.cx.or(parent.cx);
            g.cy = g.cy.or(parent.cy);
            g.fx = g.fx.or(parent.fx);
            g.fy = g.fy.or(parent.fy);
            g.r = g.r.or(parent.r);
            next_href = parent.href.clone();
        }
    }

    // Third pass: apply spec defaults, normalise userSpaceOnUse to mm,
    // build the final Gradient values.
    let mut registry: GradientRegistry = HashMap::new();
    for (id, g) in raw {
        let units = g.units.unwrap_or(GradientUnits::ObjectBoundingBox);
        let spread = g.spread.unwrap_or(SpreadMethod::Pad);
        let stops_raw = g.own_stops.unwrap_or_default();
        if stops_raw.is_empty() {
            warn!("Gradient '#{}' has no stops; skipping", id);
            continue;
        }
        // Sort stops by offset for the interpolator.
        let mut stops = stops_raw;
        stops.sort_by(|a, b| a.offset.partial_cmp(&b.offset).unwrap_or(std::cmp::Ordering::Equal));

        // Spec defaults.
        let (x1, y1, x2, y2) = (
            g.x1.unwrap_or(0.0),
            g.y1.unwrap_or(0.0),
            g.x2.unwrap_or(1.0),
            g.y2.unwrap_or(0.0),
        );
        let (cx, cy, r) = (g.cx.unwrap_or(0.5), g.cy.unwrap_or(0.5), g.r.unwrap_or(0.5));
        let (fx, fy) = (g.fx.unwrap_or(cx), g.fy.unwrap_or(cy));

        let transform = g
            .transform
            .as_deref()
            .map(parse_gradient_transform)
            .unwrap_or_else(Affine::identity);

        // Convert coords to mm when in user space, matching what we do to
        // polylines elsewhere.
        let scale_to_mm = |x: f64, y: f64| -> (f64, f64) {
            match units {
                GradientUnits::UserSpaceOnUse => (
                    (x / viewbox.w) * width_mm,
                    (y / viewbox.h) * height_mm,
                ),
                GradientUnits::ObjectBoundingBox => (x, y),
            }
        };
        let scale_len = |v: f64| -> f64 {
            // Radial r in userSpaceOnUse units: scale by an average of x/y
            // factors. Anisotropic SVGs are rare; this matches the common
            // case where width/height share the same per-mm scale.
            match units {
                GradientUnits::UserSpaceOnUse => {
                    let sx = width_mm / viewbox.w;
                    let sy = height_mm / viewbox.h;
                    v * 0.5 * (sx + sy)
                }
                GradientUnits::ObjectBoundingBox => v,
            }
        };

        let shape = if g.is_linear {
            let (x1, y1) = scale_to_mm(x1, y1);
            let (x2, y2) = scale_to_mm(x2, y2);
            GradientShape::Linear { x1, y1, x2, y2 }
        } else {
            let (cx, cy) = scale_to_mm(cx, cy);
            let (fx, fy) = scale_to_mm(fx, fy);
            let r = scale_len(r);
            GradientShape::Radial { cx, cy, fx, fy, r }
        };

        registry.insert(
            id,
            Gradient {
                shape,
                stops,
                units,
                spread,
                transform,
            },
        );
    }
    registry
}

/// Parse `<stop>` children of a gradient element into our GradientStop list.
fn parse_stops(node: roxmltree::Node) -> Vec<GradientStop> {
    let mut out = Vec::new();
    for child in node.children().filter(|c| c.is_element() && c.tag_name().name() == "stop") {
        let offset = child
            .attribute("offset")
            .map(|s| {
                if let Some(p) = s.strip_suffix('%') {
                    p.trim().parse::<f64>().unwrap_or(0.0) / 100.0
                } else {
                    s.trim().parse::<f64>().unwrap_or(0.0)
                }
            })
            .unwrap_or(0.0)
            .clamp(0.0, 1.0);

        // Stop color and opacity can come from presentation attrs or from
        // an inline `style="stop-color:...; stop-opacity:..."`.
        let mut stop_color = child.attribute("stop-color").map(str::to_string);
        let mut stop_opacity = child
            .attribute("stop-opacity")
            .and_then(|s| s.trim().parse::<f64>().ok());
        if let Some(style) = child.attribute("style") {
            for decl in style.split(';') {
                if let Some((k, v)) = decl.split_once(':') {
                    match k.trim() {
                        "stop-color" => stop_color = Some(v.trim().to_string()),
                        "stop-opacity" => {
                            stop_opacity = v.trim().parse::<f64>().ok().or(stop_opacity);
                        }
                        _ => {}
                    }
                }
            }
        }

        let color_str = stop_color.unwrap_or_else(|| "black".to_string());
        let color = match csscolorparser::parse(&color_str) {
            Ok(c) => c,
            Err(e) => {
                warn!("Could not parse stop-color '{}': {}", color_str, e);
                csscolorparser::Color::new(0.0, 0.0, 0.0, 1.0)
            }
        };
        let alpha_mult = stop_opacity.unwrap_or(1.0).clamp(0.0, 1.0) as f32;
        let rgba = [color.r, color.g, color.b, color.a * alpha_mult];
        out.push(GradientStop { offset, rgba });
    }
    out
}

/// Expand a styled `<path>` into the polylines to engrave (each paired with
/// the power level derived from its colour), observing the path's stroke
/// and fill states:
///
/// - **Stroked path**: subpath outlines are always engraved at the stroke
///   color's power.
/// - **Filled-but-not-stroked path** with `--infill`:
///   - `concentric`: outlines are emitted at the fill power (they *are*
///     the outermost concentric ring).
///   - `parallel`/`cross`/`wavy`: only the infill polylines are emitted,
///     also at the fill power.
/// - **Path with neither stroke nor fill** (or filled but `--infill` unset
///   and not stroked): contributes nothing.
///
/// Polylines whose computed power is non-positive are filtered out, so a
/// white-on-black SVG (where `white → 0`) naturally produces an empty pass
/// instead of `S0` engraves.
fn expand_path_with_infill(
    sp: StyledPath,
    infill: Option<&InfillSpec>,
    max_power: f64,
    gradients: &GradientRegistry,
) -> Vec<PoweredPolyline> {
    let has_stroke = sp.style.has_stroke();
    let has_fill = sp.style.has_fill();
    let spec = match infill {
        Some(s) if has_fill => Some(s),
        _ => None,
    };

    // Fast path: nothing to do. Filled-only-no-infill is included here on
    // purpose — the user opted out of both stroke and infill.
    if !has_stroke && spec.is_none() {
        return vec![];
    }

    // When stroke is absent we still emit outlines for concentric infill so
    // the outermost ring (= the outline itself) is engraved and the area is
    // fully covered.
    let emit_outlines = has_stroke
        || matches!(spec.map(|s| s.pattern), Some(InfillPattern::Concentric));

    // The bounding box of this path's polylines is needed both for
    // objectBoundingBox gradient sampling and for safety against zero-area
    // paths.
    let bbox = match polylines_bbox(&sp.polylines) {
        Some(b) => b,
        None => return vec![],
    };

    // Outlines use stroke color (or fall back to fill for the
    // concentric-no-stroke case where the outline acts as the outermost
    // ring). Infill always uses fill color.
    let outline_color = if has_stroke {
        sp.style.stroke.clone()
    } else {
        sp.style.fill.clone()
    };
    let infill_color = sp.style.fill.clone();

    // Build a PoweredPolyline by sampling the path's color at the polyline's
    // centroid. Returns None if the resulting power is non-positive (white
    // / transparent / unparseable color); such polylines are dropped.
    let into_powered = |polyline: Polyline, color: Option<&str>| -> Option<PoweredPolyline> {
        let env = ColorEnv {
            gradients,
            bbox,
            sample: polyline_centroid(&polyline),
        };
        let power = power_from_color(color, max_power, &env);
        if power > 0.0 {
            Some(PoweredPolyline { polyline, power })
        } else {
            None
        }
    };

    let spec = match spec {
        Some(s) => s,
        None => {
            // Stroked-only branch.
            if !emit_outlines {
                return vec![];
            }
            return sp
                .polylines
                .into_iter()
                .filter_map(|pl| into_powered(pl, outline_color.as_deref()))
                .collect();
        }
    };

    // Clean each subpath to a unique-vertex loop. Subpaths that don't form a
    // closed loop with at least 3 points are passed through without infill.
    let mut subpaths: Vec<Vec<CoordinatePair>> = Vec::with_capacity(sp.polylines.len());
    let mut originals: Vec<Polyline> = Vec::with_capacity(sp.polylines.len());
    for pl in sp.polylines {
        let verts = unique_vertices(&pl);
        originals.push(pl);
        subpaths.push(verts);
    }

    // Classify each subpath by containment depth. Depth-even subpaths are
    // filled outers; depth-odd are holes of their nearest even-depth ancestor.
    let depths = containment_depths(&subpaths);

    struct ShapeUnit {
        outer: usize,
        holes: Vec<usize>,
    }
    let mut units: Vec<ShapeUnit> = Vec::new();
    for (i, &d) in depths.iter().enumerate() {
        if d % 2 == 0 && subpaths[i].len() >= 3 {
            units.push(ShapeUnit {
                outer: i,
                holes: Vec::new(),
            });
        }
    }
    for (i, &d) in depths.iter().enumerate() {
        if d % 2 == 0 || subpaths[i].len() < 3 {
            continue;
        }
        // Attach this hole to its immediate parent (depth d-1) outer.
        let test = subpaths[i][0];
        let mut best: Option<(usize, usize)> = None; // (unit_idx, parent_depth)
        for (u_idx, unit) in units.iter().enumerate() {
            let parent_d = depths[unit.outer];
            if parent_d + 1 != d {
                continue;
            }
            if point_in_polygon(test, &subpaths[unit.outer])
                && best.map_or(true, |(_, bd)| parent_d > bd)
            {
                best = Some((u_idx, parent_d));
            }
        }
        if let Some((u_idx, _)) = best {
            units[u_idx].holes.push(i);
        }
    }

    let mut out: Vec<PoweredPolyline> = Vec::new();

    // Outlines first. Each gets sampled at its own centroid, so a stroke
    // that varies along a gradient ends up with subpath-by-subpath powers.
    if emit_outlines {
        for pl in &originals {
            if let Some(pp) = into_powered(pl.clone(), outline_color.as_deref()) {
                out.push(pp);
            }
        }
    }

    for unit in &units {
        let outer_pts: &[CoordinatePair] = &subpaths[unit.outer];
        let hole_pts: Vec<&[CoordinatePair]> = unit
            .holes
            .iter()
            .map(|&i| subpaths[i].as_slice())
            .filter(|s| s.len() >= 3)
            .collect();
        let rings = match spec.pattern {
            InfillPattern::Concentric => shape_concentric_infill(outer_pts, &hole_pts, spec.step),
            InfillPattern::Parallel => parallel_infill(outer_pts, &hole_pts, spec.step, spec.angle_deg),
            InfillPattern::Cross => {
                let mut lines =
                    parallel_infill(outer_pts, &hole_pts, spec.step, spec.angle_deg);
                lines.extend(parallel_infill(
                    outer_pts,
                    &hole_pts,
                    spec.step,
                    spec.angle_deg + 90.0,
                ));
                lines
            }
            InfillPattern::Wavy => wavy_infill(
                outer_pts,
                &hole_pts,
                spec.step,
                spec.angle_deg,
                spec.wave_amplitude,
                spec.wave_period,
            ),
        };
        for pl in rings {
            if let Some(pp) = into_powered(pl, infill_color.as_deref()) {
                out.push(pp);
            }
        }
    }

    out
}

/// `true` iff two points are "the same" by cavalier_contours' position
/// equality tolerance. The offset routines `debug_assert!` that input has no
/// repeat-position vertices within their `pos_equal_eps` (default 1e-5), so
/// we need to dedupe at least that aggressively or risk panicking on
/// near-duplicates produced by curve flattening.
fn pts_eq(a: CoordinatePair, b: CoordinatePair) -> bool {
    const EPS: f64 = 1e-5;
    (a.x - b.x).abs() < EPS && (a.y - b.y).abs() < EPS
}

/// Open-loop vertex list with adjacent duplicates collapsed. Drops the
/// trailing closure point (svg2polylines repeats the first vertex on `Z`) as
/// well as any other consecutive coincident vertices that can come out of
/// curve flattening — cavalier_contours panics if it sees repeats.
fn unique_vertices(polyline: &Polyline) -> Vec<CoordinatePair> {
    let pts = polyline.as_ref();
    let mut out: Vec<CoordinatePair> = Vec::with_capacity(pts.len());
    for &p in pts {
        match out.last() {
            Some(&last) if pts_eq(last, p) => continue,
            _ => out.push(p),
        }
    }
    if out.len() >= 2 {
        let first = out[0];
        let last = out[out.len() - 1];
        if pts_eq(first, last) {
            out.pop();
        }
    }
    out
}

/// Math-convention signed area (positive when vertices are CCW in y-up).
fn signed_area(pts: &[CoordinatePair]) -> f64 {
    let n = pts.len();
    if n < 3 {
        return 0.0;
    }
    let mut sum = 0.0;
    for i in 0..n {
        let a = pts[i];
        let b = pts[(i + 1) % n];
        sum += a.x * b.y - b.x * a.y;
    }
    sum * 0.5
}

/// Standard ray-casting point-in-polygon test. The polygon is given as an
/// open vertex loop (no repeated closing vertex). Boundary cases are not
/// special-cased; that's fine for our use of testing whether one subpath's
/// first vertex is inside another subpath, because subpaths from distinct
/// SVG `M` segments don't share vertices.
fn point_in_polygon(p: CoordinatePair, poly: &[CoordinatePair]) -> bool {
    let n = poly.len();
    if n < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let pi = poly[i];
        let pj = poly[j];
        if (pi.y > p.y) != (pj.y > p.y) {
            let x_intersect = (pj.x - pi.x) * (p.y - pi.y) / (pj.y - pi.y) + pi.x;
            if p.x < x_intersect {
                inside = !inside;
            }
        }
        j = i;
    }
    inside
}

/// For each subpath, count how many other subpaths strictly contain it (via
/// `point_in_polygon` on its first vertex). The result lets us classify
/// each subpath as outer (even depth) or hole (odd depth).
fn containment_depths(subpaths: &[Vec<CoordinatePair>]) -> Vec<usize> {
    let n = subpaths.len();
    let mut depths = vec![0usize; n];
    for i in 0..n {
        if subpaths[i].is_empty() {
            continue;
        }
        let test = subpaths[i][0];
        for j in 0..n {
            if i == j || subpaths[j].len() < 3 {
                continue;
            }
            if point_in_polygon(test, &subpaths[j]) {
                depths[i] += 1;
            }
        }
    }
    depths
}

/// Build a cavalier `Shape` for a single filled region (one outer + its
/// immediate holes), then iteratively offset inward by `step` until the
/// shape collapses. The boundary loops in the input are not returned; only
/// the resulting inset rings are.
fn shape_concentric_infill(
    outer: &[CoordinatePair],
    holes: &[&[CoordinatePair]],
    step: f64,
) -> Vec<Polyline> {
    if outer.len() < 3 || signed_area(outer).abs() < 1e-12 {
        return vec![];
    }

    // Build a Shape with outer forced to CCW (positive math area) and each
    // hole forced to CW (negative math area). Shape::from_plines classifies
    // based on area sign, so getting orientations right is what tells
    // cavalier which loop is filled and which is a hole.
    let outer_pl = vertices_to_oriented_cc(outer, true);
    let mut plines: Vec<CcPolyline<f64>> = vec![outer_pl];
    for &hole in holes {
        if hole.len() < 3 {
            continue;
        }
        plines.push(vertices_to_oriented_cc(hole, false));
    }

    // Establish the input's bbox up front. Subsequent inward offsets should
    // never produce vertices outside it — if cavalier returns extreme
    // coordinates (a known failure mode on thin / numerically tricky
    // inputs), we discard that iteration and stop rather than emit a giant
    // diagonal across the work area.
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    for p in outer {
        if p.x < min_x {
            min_x = p.x;
        }
        if p.y < min_y {
            min_y = p.y;
        }
        if p.x > max_x {
            max_x = p.x;
        }
        if p.y > max_y {
            max_y = p.y;
        }
    }
    // Allow a small slack so we don't reject legitimate floating-point
    // wobble at convex corners. Half a step is plenty.
    let slack = step * 0.5;
    let (lo_x, lo_y, hi_x, hi_y) = (min_x - slack, min_y - slack, max_x + slack, max_y + slack);

    let mut current = Shape::from_plines(plines);
    let mut rings: Vec<Polyline> = Vec::new();
    const MAX_ITERS: usize = 10_000;
    let mut iters = 0;
    loop {
        if iters >= MAX_ITERS {
            warn!("Offset iteration hit cap of {} loops; stopping", MAX_ITERS);
            break;
        }
        iters += 1;
        let next = current.parallel_offset(step, ShapeOffsetOptions::new());
        if next.ccw_plines.is_empty() && next.cw_plines.is_empty() {
            break;
        }
        // Sanity-check the new shape's vertices: anything outside the
        // input's bbox (with small slack) means cavalier produced a
        // runaway offset on a numerically tricky input; bail.
        let mut runaway = false;
        for ipl in next.ccw_plines.iter().chain(next.cw_plines.iter()) {
            for i in 0..ipl.polyline.vertex_count() {
                let v = ipl.polyline.at(i);
                if v.x < lo_x || v.x > hi_x || v.y < lo_y || v.y > hi_y {
                    runaway = true;
                    break;
                }
            }
            if runaway {
                break;
            }
        }
        if runaway {
            warn!("Discarding degenerate offset iteration with out-of-bbox vertices");
            break;
        }
        for ipl in next.ccw_plines.iter().chain(next.cw_plines.iter()) {
            // Skip degenerate "spike" polylines (e.g. 2 distinct vertices
            // marked is_closed → A→B→A zero-area segment) that cavalier
            // can emit at concave-corner offsets.
            if ipl.polyline.vertex_count() < 3 {
                continue;
            }
            rings.push(cc_polyline_to_polyline(&ipl.polyline));
        }
        current = next;
    }
    rings
}

/// Straight-line scan-line infill of the polygon-with-holes. Each scan line
/// produces zero or more 2-point polylines, one per "in-out" segment pair
/// from the scan's intersections with edges of the outer and the holes.
/// Lines alternate direction (boustrophedon) to minimise pen-up travel.
fn parallel_infill(
    outer: &[CoordinatePair],
    holes: &[&[CoordinatePair]],
    step: f64,
    angle_deg: f64,
) -> Vec<Polyline> {
    let theta = angle_deg.to_radians();
    let (s, c) = (theta.sin(), theta.cos());

    // Rotate by -theta so the scan direction becomes the +X axis (and so
    // "horizontal scan lines" become lines of constant y in the rotated
    // frame). Inverse rotation is +theta.
    let rot = |p: CoordinatePair| CoordinatePair::new(c * p.x + s * p.y, -s * p.x + c * p.y);
    let unrot =
        |p: CoordinatePair| CoordinatePair::new(c * p.x - s * p.y, s * p.x + c * p.y);

    let outer_rot: Vec<CoordinatePair> = outer.iter().copied().map(rot).collect();
    let holes_rot: Vec<Vec<CoordinatePair>> = holes
        .iter()
        .map(|h| h.iter().copied().map(rot).collect())
        .collect();

    let (ymin, ymax) = match y_bounds(&outer_rot) {
        Some(b) => b,
        None => return vec![],
    };

    let mut polylines = Vec::new();
    // Offset the first line a half-step in from the edge so we don't try to
    // scan exactly along a vertex or edge.
    let mut y = ymin + step * 0.5;
    let mut flip = false;
    while y < ymax {
        let mut xs: Vec<f64> = Vec::new();
        push_scan_intersections(&outer_rot, y, &mut xs);
        for h in &holes_rot {
            push_scan_intersections(h, y, &mut xs);
        }
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        for chunk in xs.chunks_exact(2) {
            let (x0, x1) = (chunk[0], chunk[1]);
            let (a, b) = if flip { (x1, x0) } else { (x0, x1) };
            polylines.push(Polyline::from_vec(vec![
                unrot(CoordinatePair::new(a, y)),
                unrot(CoordinatePair::new(b, y)),
            ]));
        }
        flip = !flip;
        y += step;
    }
    polylines
}

/// Like [`parallel_infill`], but each filled segment is sampled along a
/// sine wave: `y_sample = scan_y + amplitude * sin(2π * x / period)`. The
/// wave's centerline is the scan line, so the segment endpoints sit exactly
/// on the polygon boundary in the rotated frame.
fn wavy_infill(
    outer: &[CoordinatePair],
    holes: &[&[CoordinatePair]],
    step: f64,
    angle_deg: f64,
    amplitude: f64,
    period: f64,
) -> Vec<Polyline> {
    let theta = angle_deg.to_radians();
    let (s, c) = (theta.sin(), theta.cos());
    let rot = |p: CoordinatePair| CoordinatePair::new(c * p.x + s * p.y, -s * p.x + c * p.y);
    let unrot =
        |p: CoordinatePair| CoordinatePair::new(c * p.x - s * p.y, s * p.x + c * p.y);

    let outer_rot: Vec<CoordinatePair> = outer.iter().copied().map(rot).collect();
    let holes_rot: Vec<Vec<CoordinatePair>> = holes
        .iter()
        .map(|h| h.iter().copied().map(rot).collect())
        .collect();

    let (ymin, ymax) = match y_bounds(&outer_rot) {
        Some(b) => b,
        None => return vec![],
    };

    // Sample density: small enough to keep the sine looking smooth.
    let sample_step = (period / 16.0).max(1e-3);

    let mut polylines = Vec::new();
    let mut y = ymin + step * 0.5;
    let mut flip = false;
    while y < ymax {
        let mut xs: Vec<f64> = Vec::new();
        push_scan_intersections(&outer_rot, y, &mut xs);
        for h in &holes_rot {
            push_scan_intersections(h, y, &mut xs);
        }
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        for chunk in xs.chunks_exact(2) {
            let (x0, x1) = (chunk[0], chunk[1]);
            let pts = sine_samples(x0, x1, y, amplitude, period, sample_step, flip);
            polylines.push(Polyline::from_vec(pts.into_iter().map(unrot).collect()));
        }
        flip = !flip;
        y += step;
    }
    polylines
}

/// Generate the sampled points of a single sine-wave segment from `x0` to
/// `x1` (or reversed if `flip`) along scan line `y`, with the given
/// amplitude / period. Endpoints are forced to (x*, y) so the wave starts
/// and ends exactly on the polygon boundary.
fn sine_samples(
    x_start: f64,
    x_end: f64,
    y: f64,
    amplitude: f64,
    period: f64,
    sample_step: f64,
    flip: bool,
) -> Vec<CoordinatePair> {
    let (a, b) = if flip {
        (x_end, x_start)
    } else {
        (x_start, x_end)
    };
    let direction = (b - a).signum();
    let length = (b - a).abs();
    let n = (length / sample_step).floor() as usize;
    let mut pts = Vec::with_capacity(n + 2);
    pts.push(CoordinatePair::new(a, y));
    let omega = std::f64::consts::TAU / period;
    for k in 1..=n {
        let x = a + direction * (k as f64) * sample_step;
        // If the last sample lands within an epsilon of the segment's end,
        // skip it — the forced endpoint below covers it without producing a
        // duplicate vertex.
        if k == n && (x - b).abs() < 1e-9 {
            break;
        }
        let dy = amplitude * (omega * x).sin();
        pts.push(CoordinatePair::new(x, y + dy));
    }
    pts.push(CoordinatePair::new(b, y));
    pts
}

/// y-range of a polygon's vertices in the rotated frame. Returns None for
/// degenerate (empty or vanishingly thin) input.
fn y_bounds(verts: &[CoordinatePair]) -> Option<(f64, f64)> {
    let mut ymin = f64::INFINITY;
    let mut ymax = f64::NEG_INFINITY;
    for p in verts {
        ymin = ymin.min(p.y);
        ymax = ymax.max(p.y);
    }
    if !ymin.is_finite() || !ymax.is_finite() || ymax - ymin <= 0.0 {
        None
    } else {
        Some((ymin, ymax))
    }
}

/// Push the x-coordinates of every intersection of the horizontal line
/// `y = scan_y` with the edges of `polygon` into `xs`. The convention
/// `(pi.y > y) != (pj.y > y)` excludes purely horizontal edges and treats
/// a vertex shared between two upward-or-two-downward edges as one
/// crossing, both of which are necessary for correct in/out pairing.
fn push_scan_intersections(polygon: &[CoordinatePair], scan_y: f64, xs: &mut Vec<f64>) {
    let n = polygon.len();
    if n < 3 {
        return;
    }
    let mut j = n - 1;
    for i in 0..n {
        let pi = polygon[i];
        let pj = polygon[j];
        if (pi.y > scan_y) != (pj.y > scan_y) {
            let x = (pj.x - pi.x) * (scan_y - pi.y) / (pj.y - pi.y) + pi.x;
            xs.push(x);
        }
        j = i;
    }
}

/// Convert a vertex list to a closed cavalier polyline with the requested
/// orientation (`make_ccw=true` => positive math area, otherwise negative).
fn vertices_to_oriented_cc(verts: &[CoordinatePair], make_ccw: bool) -> CcPolyline<f64> {
    let area = signed_area(verts);
    let reverse = (area > 0.0) != make_ccw;
    let iter: Box<dyn Iterator<Item = &CoordinatePair>> = if reverse {
        Box::new(verts.iter().rev())
    } else {
        Box::new(verts.iter())
    };
    let mut pl = CcPolyline::new_closed();
    for cp in iter {
        pl.add(cp.x, cp.y, 0.0);
    }
    pl
}

/// Convert a closed cavalier polyline back to an svg2polylines `Polyline`,
/// repeating the first vertex at the end so downstream g-code emission draws
/// the closing segment.
fn cc_polyline_to_polyline(pl: &CcPolyline<f64>) -> Polyline {
    let n = pl.vertex_count();
    let mut pts: Vec<CoordinatePair> = Vec::with_capacity(n + 1);
    for i in 0..n {
        let v: PlineVertex<f64> = pl.at(i);
        pts.push(CoordinatePair::new(v.x, v.y));
    }
    if let Some(first) = pts.first().copied() {
        pts.push(first);
    }
    Polyline::from_vec(pts)
}

pub fn length_to_mm(s: &str, dpi: f64, font_px: f64) -> Result<f64, String> {
    let len = Length::from_str(s).map_err(|e| format!("parse error: {e:?}"))?;

    let mm = match len.unit {
        LengthUnit::None => {
            // For SVG, bare numbers are often user units; treat as px if that's your policy.
            len.number * 25.4 / dpi
        }
        LengthUnit::Px => len.number * 25.4 / dpi,
        LengthUnit::Mm => len.number,
        LengthUnit::Cm => len.number * 10.0,
        LengthUnit::In => len.number * 25.4,
        LengthUnit::Pt => len.number * 25.4 / 72.0,
        LengthUnit::Pc => len.number * 25.4 / 6.0,
        LengthUnit::Em => (len.number * font_px) * 25.4 / dpi,
        LengthUnit::Ex => {
            return Err("ex needs an x-height assumption".into());
        }
        LengthUnit::Percent => {
            return Err("% needs a reference length".into());
        }
    };

    Ok(mm)
}

