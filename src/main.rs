use cavalier_contours::polyline::{
    PlineSource, PlineSourceMut, PlineVertex, Polyline as CcPolyline,
};
use clap::{ArgAction, Parser};
use log::{info, warn};
use roxmltree::Document;
use std::str::FromStr;
use std::{
    fs::File,
    io::{self, Read},
    path::PathBuf,
};
use svg2polylines::{CoordinatePair, Polyline, StyledPolyline};
use svgtypes::{Length, LengthUnit, ViewBox};

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

    /// Strength during engraving (`F` argument during engraving movement - max is found using `$$` in your machine's console, and looking at the `$30` value)
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

    /// Spacing (in mm) between concentric infill passes for filled, convex paths.
    /// If unset, no infill is generated.
    #[arg(long)]
    infill: Option<f64>,

    // TODO: Fill type? Gradients? etc
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

    let styled: Vec<StyledPolyline> = svg2polylines::parse_styled(
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

    // Normalize coordinates of every polyline to millimeters via viewBox/width/height.
    let styled: Vec<StyledPolyline> = styled
        .into_iter()
        .map(|sp| StyledPolyline {
            polyline: Polyline::from_vec(
                sp.polyline
                    .unwrap()
                    .into_iter()
                    .map(|p| CoordinatePair {
                        x: (p.x / viewbox.w) * width,
                        y: (p.y / viewbox.h) * height,
                    })
                    .collect(),
            ),
            style: sp.style,
        })
        .collect();

    // For filled convex polylines, expand each into [outline, ring_1, ring_2, ...].
    // Non-convex filled paths emit a warning and pass through unchanged.
    let polylines: Vec<Polyline> = styled
        .into_iter()
        .flat_map(|sp| expand_with_infill(sp, args.infill))
        .collect();

    let points: Vec<CoordinatePair> = polylines
        .clone()
        .into_iter()
        .map(|polyline| polyline.unwrap())
        .flatten()
        .collect();

    let min_x = points.iter().map(|p| p.x).fold(f64::INFINITY, f64::min);
    let max_y = points.iter().map(|p| p.y).fold(f64::NEG_INFINITY, f64::max);

    for polyline in polylines.into_iter() {
        let polyline = Polyline::from_vec(
            polyline
                .unwrap()
                .into_iter()
                .map(|p| if args.reset_origin {
                    CoordinatePair { x: p.x - min_x, y: max_y - p.y }
                } else {
                    CoordinatePair { x: p.x, y: height - p.y }
                })
                .collect()
        );
        let mut gcodes_to_append = polyline2gcode(polyline, args.strength, args.speed)?;
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

/// Expand a styled polyline into its outline plus, if filled/convex/closed and
/// `infill_step` is set, a sequence of inward concentric infill rings.
fn expand_with_infill(sp: StyledPolyline, infill_step: Option<f64>) -> Vec<Polyline> {
    let outline = sp.polyline;
    let step = match infill_step {
        Some(s) if s > 0.0 => s,
        _ => return vec![outline],
    };
    if !sp.style.has_fill() {
        return vec![outline];
    }
    if !is_closed_polyline(&outline) {
        // A filled-but-open subpath would be implicitly closed by an SVG renderer.
        // We're stricter here and skip infill; the user likely wants to fix the path.
        warn!("Filled path is not closed (first != last vertex); skipping infill");
        return vec![outline];
    }
    if !is_convex(&outline) {
        warn!("Filled path is non-convex; skipping infill (only convex paths are supported)");
        return vec![outline];
    }
    let rings = concentric_infill(&outline, step);
    // Outline first, then rings going inward — keeps the engrave ordered
    // outermost-to-innermost.
    let mut out = Vec::with_capacity(rings.len() + 1);
    out.push(outline);
    out.extend(rings);
    out
}

/// `true` iff the polyline's first vertex equals its last (within a tight
/// tolerance). svg2polylines emits this shape when the source SVG path used
/// `Z` to close.
fn is_closed_polyline(polyline: &Polyline) -> bool {
    let pts = polyline.as_ref();
    if pts.len() < 3 {
        return false;
    }
    let a = pts[0];
    let b = pts[pts.len() - 1];
    (a.x - b.x).abs() < 1e-9 && (a.y - b.y).abs() < 1e-9
}

/// `true` iff the closed polyline is convex (all turns have the same sign,
/// ignoring straight-through / collinear vertices).
fn is_convex(polyline: &Polyline) -> bool {
    let pts = unique_vertices(polyline);
    if pts.len() < 3 {
        return false;
    }
    let n = pts.len();
    let mut sign: f64 = 0.0;
    for i in 0..n {
        let a = pts[i];
        let b = pts[(i + 1) % n];
        let c = pts[(i + 2) % n];
        let cross = (b.x - a.x) * (c.y - b.y) - (b.y - a.y) * (c.x - b.x);
        if cross.abs() < 1e-9 {
            continue;
        }
        if sign == 0.0 {
            sign = cross;
        } else if sign.signum() != cross.signum() {
            return false;
        }
    }
    sign != 0.0
}

/// Strip a trailing duplicate of the first vertex if present (svg2polylines'
/// way of marking a closed loop). Returns the open vertex list.
fn unique_vertices(polyline: &Polyline) -> Vec<CoordinatePair> {
    let pts = polyline.as_ref();
    if pts.len() >= 2
        && (pts[0].x - pts[pts.len() - 1].x).abs() < 1e-9
        && (pts[0].y - pts[pts.len() - 1].y).abs() < 1e-9
    {
        pts[..pts.len() - 1].to_vec()
    } else {
        pts.clone()
    }
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

/// Generate concentric inward offsets of `outline` spaced by `step`. The
/// outline itself is not included in the returned vector. Iteration stops when
/// the offset operation produces no more loops (the shape has collapsed) or
/// splits into multiple loops (shouldn't happen for a convex input, but we
/// stop conservatively if it does).
fn concentric_infill(outline: &Polyline, step: f64) -> Vec<Polyline> {
    let verts = unique_vertices(outline);
    if verts.len() < 3 {
        return vec![];
    }
    let area = signed_area(&verts);
    if area.abs() < 1e-12 {
        return vec![];
    }
    // cavalier_contours: positive offset = left of segment direction. For our
    // input (math-CCW => area > 0), positive offset is inward. Match offset
    // sign to area sign so we always shrink the enclosed region.
    let offset_signed = step.copysign(area);

    let mut current: CcPolyline<f64> = CcPolyline::new_closed();
    for cp in &verts {
        current.add(cp.x, cp.y, 0.0);
    }

    let mut rings: Vec<Polyline> = Vec::new();
    loop {
        let next = current.parallel_offset(offset_signed);
        if next.is_empty() {
            break;
        }
        for ring in &next {
            rings.push(cc_polyline_to_polyline(ring));
        }
        if next.len() != 1 {
            // Convex input shouldn't split — bail out rather than recurse on
            // each sub-loop. If we relax convexity later, recurse here.
            break;
        }
        current = next.into_iter().next().unwrap();
    }
    rings
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

