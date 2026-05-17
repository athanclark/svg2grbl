use clap::{Parser, ArgAction};
use std::{io::{self, Read}, fs::File, path::PathBuf};
use svg2polylines::{Polyline, CoordinatePair};
use roxmltree::Document;
use std::str::FromStr;
use svgtypes::{Length, LengthUnit, ViewBox};
use log::{info};

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

    let polylines: Vec<Polyline> = svg2polylines::parse(
        &svg_buf,
        args.tolerance,
        args.preprocess
    ).map_err(io::Error::other)?; // TODO customize how strokes, fill, etc are parsed

    let mut gcodes: Vec<String> = vec![
        "G21".to_owned(), // use millimeters
        "G90".to_owned(), // absolute positioning
        "G94".to_owned(), // speed = mm/min
        "M5".to_owned(), // make sure laser is disengaged
        "G28".to_owned(), // move to stored home
    ];

    // this normalizes the units of the paths with respect to millimeters
    let polylines: Vec<Polyline> = polylines
        .into_iter()
        .map(|polyline| Polyline::from_vec(
            polyline
                .unwrap()
                .into_iter()
                .map(|p| CoordinatePair { x: (p.x / viewbox.w) * width, y: (p.y / viewbox.h) * height })
                .collect()
        ))
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

