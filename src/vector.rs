//! Resolve vector styles and cumulative transforms before converting to polylines.

use std::io;

use roxmltree::Document;
use svg2polylines::{CoordinatePair, Polyline, StyledPath};

use crate::{Affine, parse_transform};

pub(crate) fn parse_paths(
    source: &str,
    tolerance: f64,
    preprocess: bool,
    dpi: f64,
    font_size: f64,
) -> io::Result<Vec<StyledPath>> {
    if !preprocess {
        return svg2polylines::parse_paths(source, tolerance, false).map_err(io::Error::other);
    }

    let options = usvg::Options {
        dpi,
        font_size,
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_str(source, &options.to_ref()).map_err(io::Error::other)?;
    let normalized = tree.to_string(&usvg::XmlOptions::default());
    let doc = Document::parse(&normalized).map_err(io::Error::other)?;
    let mut result = Vec::new();
    for node in doc.descendants().filter(|n| n.has_tag_name("path")) {
        // Definition geometry describes effects and paint servers, not separate
        // engraving paths. usvg has already expanded rendered <use> instances.
        if node.ancestors().any(|n| n.has_tag_name("defs"))
            || matches!(node.attribute("visibility"), Some("hidden" | "collapse"))
        {
            continue;
        }

        // usvg retains groups needed for clipping, masks, filters, or opacity.
        // svg2polylines only applies a path's own matrix, so apply its remaining
        // ancestor transforms explicitly, from the outermost to the innermost.
        let ancestors: Vec<_> = node.ancestors().skip(1).collect();
        let mut transform = Affine::identity();
        for ancestor in ancestors.iter().rev() {
            if let Some(value) = ancestor.attribute("transform") {
                transform = transform.compose(&parse_transform(value).map_err(io::Error::other)?);
            }
        }
        let paths = svg2polylines::parse_paths(&normalized[node.range()], tolerance, false)
            .map_err(io::Error::other)?;
        result.extend(paths.into_iter().map(|path| {
            StyledPath {
                polylines: path
                    .polylines
                    .into_iter()
                    .map(|polyline| {
                        Polyline::from_vec(
                            polyline
                                .unwrap()
                                .into_iter()
                                .map(|point| {
                                    let (x, y) = transform.apply(point.x, point.y);
                                    CoordinatePair { x, y }
                                })
                                .collect(),
                        )
                    })
                    .collect(),
                style: path.style,
            }
        }));
    }
    Ok(result)
}
