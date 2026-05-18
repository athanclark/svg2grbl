use cavalier_contours::polyline::{
    PlineSource, PlineSourceMut, PlineVertex, Polyline as CcPolyline,
};
use cavalier_contours::shape_algorithms::{Shape, ShapeOffsetOptions};
use clap::{ArgAction, Parser, ValueEnum};
use log::info;
use roxmltree::Document;
use std::str::FromStr;
use std::{
    fs::File,
    io::{self, Read},
    path::PathBuf,
};
use svg2polylines::{CoordinatePair, Polyline, StyledPath};
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

    // Expand each StyledPath into the polylines to engrave. For filled paths
    // with --infill set, this includes the outline(s), any hole boundaries,
    // and the chosen infill pattern of the polygon-with-holes.
    let polylines: Vec<Polyline> = paths
        .into_iter()
        .flat_map(|sp| expand_path_with_infill(sp, infill_spec.as_ref()))
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

/// Expand a styled `<path>` into the polylines to engrave. For filled paths
/// with `infill` set, this includes the outline of every subpath plus the
/// chosen infill pattern's polylines for the polygon-with-holes (holes are
/// detected by containment within the same `<path>`). For unfilled paths,
/// or when no infill is requested, the subpath outlines pass through
/// unchanged.
fn expand_path_with_infill(sp: StyledPath, infill: Option<&InfillSpec>) -> Vec<Polyline> {
    let spec = match infill {
        Some(s) if sp.style.has_fill() => s,
        _ => return sp.polylines,
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

    let mut out: Vec<Polyline> = Vec::new();

    // Outlines first, in source order, so the engraver does outline then infill.
    for pl in &originals {
        out.push(pl.clone());
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
        out.extend(rings);
    }

    out
}

/// `true` iff a vertex equals its predecessor — used inside the cleanup loop
/// in [`unique_vertices`].
fn pts_eq(a: CoordinatePair, b: CoordinatePair) -> bool {
    const EPS: f64 = 1e-9;
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

    let mut current = Shape::from_plines(plines);
    let mut rings: Vec<Polyline> = Vec::new();
    loop {
        let next = current.parallel_offset(step, ShapeOffsetOptions::new());
        if next.ccw_plines.is_empty() && next.cw_plines.is_empty() {
            break;
        }
        for ipl in &next.ccw_plines {
            rings.push(cc_polyline_to_polyline(&ipl.polyline));
        }
        for ipl in &next.cw_plines {
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

