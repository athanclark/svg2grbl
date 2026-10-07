use std::{
    io::Write,
    process::{Command, Stdio},
};

fn movements(source: &str, args: &[&str]) -> Vec<(f64, f64, Option<f64>)> {
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
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.starts_with("G0 ") || line.starts_with("G1 "))
        .map(|line| {
            let value = |prefix: char| {
                line.split_whitespace()
                    .find_map(|word| word.strip_prefix(prefix))
                    .map(|word| word.parse::<f64>().unwrap())
            };
            (value('X').unwrap(), value('Y').unwrap(), value('S'))
        })
        .collect()
}

fn assert_moves(actual: &[(f64, f64, Option<f64>)], expected: &[(f64, f64, Option<f64>)]) {
    assert_eq!(actual.len(), expected.len(), "{actual:?}");
    for (actual, expected) in actual.iter().zip(expected) {
        assert!(
            (actual.0 - expected.0).abs() < 1e-6,
            "{actual:?} != {expected:?}"
        );
        assert!(
            (actual.1 - expected.1).abs() < 1e-6,
            "{actual:?} != {expected:?}"
        );
        match (actual.2, expected.2) {
            (Some(a), Some(e)) => assert!((a - e).abs() < 1e-4, "{a} != {e}"),
            (a, e) => assert_eq!(a, e),
        }
    }
}

#[test]
fn nonzero_viewbox_origins_preserve_page_placement_with_and_without_preprocessing() {
    for (x, y) in [(-20, -10), (20, 10)] {
        let source = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="100mm" height="50mm" viewBox="{x} {y} 200 100">
                <path d="M {} {} L {} {}" fill="none" stroke="black"/>
            </svg>"#,
            x + 10,
            y + 10,
            x + 180,
            y + 80,
        );
        for preprocess in [false, true] {
            let mut args = Vec::new();
            if preprocess {
                args.push("--preprocess");
            }
            assert_moves(
                &movements(&source, &args),
                &[(5.0, 45.0, None), (90.0, 10.0, Some(500.0))],
            );
            args.push("--reset-origin");
            assert_moves(
                &movements(&source, &args),
                &[(0.0, 35.0, None), (85.0, 0.0, Some(500.0))],
            );
        }
    }
}

#[test]
fn retained_nested_group_and_path_transforms_match_flattened_geometry() {
    let wrapper = |body: &str| {
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="20mm" height="20mm" viewBox="-10 -10 20 20">{body}</svg>"#
        )
    };
    let nested = wrapper(
        r#"
        <defs><clipPath id="clip"><path d="M -100 -100 H 100 V 100 H -100 Z"/></clipPath></defs>
        <g transform="translate(-8 -7)" clip-path="url(#clip)" fill="none" stroke="red">
            <g transform="rotate(90)" opacity="0.5">
                <path transform="scale(2)" d="M 1 1 L 2 1"/>
            </g>
        </g>
    "#,
    );
    let flattened = wrapper(r#"<path d="M -10 -5 L -10 -3" fill="none" stroke="red"/>"#);
    let expected = [(0.0, 15.0, None), (0.0, 13.0, Some(393.7))];
    assert_moves(&movements(&nested, &["--preprocess"]), &expected);
    assert_moves(&movements(&flattened, &["--preprocess"]), &expected);
}

#[test]
fn clip_definition_paths_are_not_engraved_as_infill() {
    let source = r#"<svg xmlns="http://www.w3.org/2000/svg" width="20mm" height="20mm" viewBox="0 0 20 20">
        <defs><clipPath id="clip"><path d="M 0 0 H 20 V 20 H 0 Z"/></clipPath></defs>
        <g clip-path="url(#clip)"><path d="M 2 3 H 8" fill="none" stroke="black"/></g>
    </svg>"#;
    assert_moves(
        &movements(
            source,
            &[
                "--preprocess",
                "--infill",
                "1",
                "--infill-pattern",
                "parallel",
            ],
        ),
        &[(2.0, 17.0, None), (8.0, 17.0, Some(500.0))],
    );
}

#[test]
fn preprocessing_applies_a_path_matrix_exactly_once() {
    let source = r#"<svg xmlns="http://www.w3.org/2000/svg" width="20mm" height="20mm" viewBox="0 0 20 20">
        <path d="M 1 2 L 3 4" transform="matrix(2 0 0 3 5 6)" fill="none" stroke="black"/>
    </svg>"#;
    for args in [vec![], vec!["--preprocess"]] {
        assert_moves(
            &movements(source, &args),
            &[(7.0, 8.0, None), (11.0, 2.0, Some(500.0))],
        );
    }
}

#[test]
fn user_space_gradient_and_paths_share_the_viewbox_offset() {
    for gradient in [
        r#"<linearGradient id="shade" gradientUnits="userSpaceOnUse" x1="-10" y1="-20" x2="70" y2="-20">"#,
        r#"<linearGradient id="shade" gradientUnits="userSpaceOnUse" x1="0" y1="0" x2="80" y2="0" gradientTransform="translate(-10 -20)">"#,
        r#"<radialGradient id="shade" gradientUnits="userSpaceOnUse" cx="-10" cy="-20" r="80">"#,
    ] {
        let end = if gradient.contains("radialGradient") {
            "radialGradient"
        } else {
            "linearGradient"
        };
        let source = format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="8mm" height="4mm" viewBox="-10 -20 80 40">
            <defs>{gradient}<stop offset="0" stop-color="black"/><stop offset="1" stop-color="white"/></{end}></defs>
            <path d="M 0 -20 L 20 -20" fill="none" stroke="url(#shade)"/>
        </svg>"##
        );
        assert_moves(
            &movements(&source, &[]),
            &[(1.0, 4.0, None), (3.0, 4.0, Some(375.0))],
        );
    }
}
