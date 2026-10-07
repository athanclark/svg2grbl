use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

use base64::{Engine, engine::general_purpose::STANDARD};

const SVG: &str = include_str!("../examples/raster.svg");

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "svg2grbl-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run_stdin(source: &str, args: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_svg2grbl"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(source.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn movements(output: &Output) -> Vec<(f64, f64, f64)> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.starts_with("G1 "))
        .map(|line| {
            let value = |prefix: char| {
                line.split_whitespace()
                    .find_map(|word| word.strip_prefix(prefix))
                    .unwrap()
                    .parse::<f64>()
                    .unwrap()
            };
            (value('X'), value('Y'), value('S'))
        })
        .collect()
}

#[test]
fn embedded_png_works_from_stdin_with_and_without_preprocessing() {
    for preprocess in [false, true] {
        let mut args = vec![
            "--infill",
            "1",
            "--infill-pattern",
            "parallel",
            "--strength",
            "500",
        ];
        if preprocess {
            args.push("--preprocess");
        }
        let output = run_stdin(SVG, &args);
        let moves = movements(&output);
        assert_eq!(moves.len(), 32);
        let expected = [
            500.0,
            500.0 * (1.0 - 128.0 / 255.0),
            0.0,
            0.0,
            393.7,
            142.4,
            463.9,
            500.0 * 128.0 / 255.0,
        ];
        for (row_index, row) in moves.as_chunks::<8>().0.iter().enumerate() {
            for (pixel_index, (x, y, power)) in row.iter().enumerate() {
                let image_x = if row_index % 2 == 0 {
                    pixel_index
                } else {
                    7 - pixel_index
                };
                assert!((power - expected[image_x]).abs() < 1e-4);
                assert_eq!(*y, 3.5 - row_index as f64);
                assert_eq!(
                    *x,
                    if row_index % 2 == 0 {
                        (pixel_index + 1) as f64
                    } else {
                        (7 - pixel_index) as f64
                    }
                );
            }
        }
    }
}

#[test]
fn linked_png_is_resolved_relative_to_the_svg_file() {
    let scratch = Scratch::new();
    let doc = roxmltree::Document::parse(SVG).unwrap();
    let href = doc
        .descendants()
        .find(|n| n.has_tag_name("image"))
        .unwrap()
        .attribute("href")
        .unwrap();
    let png = STANDARD.decode(href.split_once(',').unwrap().1).unwrap();
    fs::write(scratch.0.join("tile image.png"), png).unwrap();
    let source = SVG.replace(href, "tile%20image.png");
    let svg_path = scratch.0.join("drawing.svg");
    fs::write(&svg_path, source).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_svg2grbl"))
        .current_dir(std::env::temp_dir())
        .arg("--svg")
        .arg(svg_path)
        .args([
            "--infill",
            "1",
            "--infill-pattern",
            "parallel",
            "--preprocess",
        ])
        .output()
        .unwrap();
    assert_eq!(movements(&output).len(), 32);
}

#[test]
fn image_and_vector_toolpaths_share_the_drawing_origin() {
    let source = SVG.replace(
        "</svg>",
        r#"<path d="M 10 2 H 12" fill="none" stroke="black"/></svg>"#,
    );
    let output = run_stdin(
        &source,
        &[
            "--infill",
            "1",
            "--infill-pattern",
            "parallel",
            "--reset-origin",
        ],
    );
    let moves = movements(&output);
    assert_eq!(moves.len(), 33);
    assert_eq!(moves[0], (12.0, 1.5, 500.0));
    assert!(moves.iter().all(|(x, y, _)| x.is_finite() && y.is_finite()));
    assert_eq!(moves.last().unwrap().1, 0.0);
}

#[test]
fn image_and_vector_toolpaths_share_a_nonzero_viewbox_origin() {
    let original = SVG.replace(
        "</svg>",
        r#"<path d="M 1 1 H 3" fill="none" stroke="black"/></svg>"#,
    );
    let shifted = original
        .replace("viewBox=\"0 0 8 4\"", "viewBox=\"-10 -20 8 4\"")
        .replace("<image", "<g transform=\"matrix(1 0 0 1 -10 -20)\"><image")
        .replace("</svg>", "</g></svg>");
    assert_ne!(original, shifted);
    for preprocess in [false, true] {
        let mut args = vec!["--infill", "1", "--infill-pattern", "parallel"];
        if preprocess {
            args.push("--preprocess");
        }
        // Raw vector parsing only applies path transforms, so give the path
        // its own matrix and keep the image's transform on its parent group.
        let shifted = if preprocess {
            shifted.clone()
        } else {
            shifted.replace("<path", "<path transform=\"matrix(1 0 0 1 -10 -20)\"")
        };
        let expected = movements(&run_stdin(&original, &args));
        let actual = movements(&run_stdin(&shifted, &args));
        assert_eq!(actual.len(), expected.len());
        for (actual, expected) in actual.iter().zip(expected) {
            assert!(
                (actual.0 - expected.0).abs() < 1e-8,
                "{actual:?} != {expected:?}"
            );
            assert!(
                (actual.1 - expected.1).abs() < 1e-8,
                "{actual:?} != {expected:?}"
            );
            assert!(
                (actual.2 - expected.2).abs() < 1e-4,
                "{actual:?} != {expected:?}"
            );
        }
    }
}

#[test]
fn image_physical_lengths_match_vector_shapes_in_a_scaled_viewbox() {
    let href = roxmltree::Document::parse(SVG)
        .unwrap()
        .descendants()
        .find(|n| n.has_tag_name("image"))
        .unwrap()
        .attribute("href")
        .unwrap()
        .to_owned();
    let wrapper = |body: &str| {
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="20mm" height="10mm" viewBox="0 0 200 100">{body}</svg>"#
        )
    };
    let image = wrapper(&format!(
        r#"<image x="20" y="20" width="4mm" height="1mm" href="{href}" preserveAspectRatio="none"/>"#
    ));
    let vector = wrapper(r#"<rect x="20" y="20" width="4mm" height="1mm" fill="black"/>"#);
    let args = [
        "--preprocess",
        "--infill",
        "0.1",
        "--infill-pattern",
        "parallel",
    ];
    let image_moves = movements(&run_stdin(&image, &args));
    let vector_moves = movements(&run_stdin(&vector, &args));
    let bounds = |moves: &[(f64, f64, f64)]| {
        (
            moves
                .iter()
                .map(|(x, _, _)| *x)
                .fold(f64::INFINITY, f64::min),
            moves
                .iter()
                .map(|(x, _, _)| *x)
                .fold(f64::NEG_INFINITY, f64::max),
            moves
                .iter()
                .map(|(_, y, _)| *y)
                .fold(f64::INFINITY, f64::min),
            moves
                .iter()
                .map(|(_, y, _)| *y)
                .fold(f64::NEG_INFINITY, f64::max),
        )
    };
    let (ix0, ix1, iy0, iy1) = bounds(&image_moves);
    let (vx0, vx1, vy0, vy1) = bounds(&vector_moves);
    for (actual, expected) in [(ix0, vx0), (ix1, vx1), (iy0, vy0), (iy1, vy1)] {
        assert!((actual - expected).abs() < 1e-8, "{actual} != {expected}");
    }
}

#[test]
fn no_infill_skips_images_and_bad_images_produce_no_partial_gcode() {
    assert!(movements(&run_stdin(SVG, &[])).is_empty());
    let doc = roxmltree::Document::parse(SVG).unwrap();
    let href = doc
        .descendants()
        .find(|n| n.has_tag_name("image"))
        .unwrap()
        .attribute("href")
        .unwrap();
    let source = SVG.replace(href, "data:image/png;base64,AAAA");
    let output = run_stdin(&source, &["--infill", "1"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot decode PNG/JPEG"));
}

#[test]
fn invalid_sampling_parameters_fail_before_generating_movements() {
    for option in [
        "--max-line=0",
        "--infill=0",
        "--infill-wave-period=0",
        "--infill=NaN",
    ] {
        let output = run_stdin(SVG, &[option]);
        assert!(!output.status.success(), "{option}");
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("must be finite"));
    }
}
