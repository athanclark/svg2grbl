# svg2grbl

Convert SVG paths and embedded PNG/JPEG images into GRBL-compatible G-code for
laser engraving. Curves are flattened into straight movements, SVG dimensions
are converted to millimeters, and colors or image pixels determine laser power.

The converter supports outlines and four infill patterns: concentric, parallel,
cross-hatch, and wavy. Compound paths can contain holes, such as the centers of
letters. It writes G-code to standard output; send the resulting file to your
machine with your usual G-code sender.

## Build

You need Rust 1.88 or newer with Cargo.
The project currently depends on a local fork of `svg2polylines` through
`path = "../svg2polylines"` in `Cargo.toml`. Check out both projects beside each
other:

```sh
git clone https://github.com/athanclark/svg2polylines.git
git clone https://github.com/athanclark/svg2grbl.git
cd svg2grbl
cargo build --release --locked
```

The executable is `target/release/svg2grbl`. The sibling fork supplies styled
paths and subpath grouping used by the color and infill code; replacing it with
the upstream crate is not a drop-in change.

## Quick start

Engrave filled shapes with parallel passes spaced 0.2 mm apart:

```sh
./target/release/svg2grbl --svg drawing.svg --preprocess --reset-origin \
  --infill 0.2 --infill-pattern parallel --strength 500 --speed 1000 \
  > drawing.gcode
```

Engrave only explicitly stroked paths:

```sh
./target/release/svg2grbl --svg outlines.svg --preprocess --reset-origin \
  --strength 500 --speed 1000 > outlines.gcode
```

The SVG can also come from standard input:

```sh
./target/release/svg2grbl --infill 0.2 < drawing.svg > drawing.gcode
```

`--strength` sets the maximum laser `S` value. Choose a value at or below your
controller's configured maximum (`$30` in GRBL's `$$` output). `--speed` sets the
`F` feed rate in mm/min. The commands above use example settings; the power,
speed, and infill spacing needed for a particular material depend on the machine.

The output uses `M4` for dynamic laser power, which requires a compatible
controller and GRBL laser mode (`$32=1`). It includes `G28` at the beginning and
end to move to the controller's stored position; this is not a homing cycle.
Check that position and preview the toolpaths before running the file.

## Preparing an SVG

The root `<svg>` must have a `viewBox`. Set explicit physical dimensions to make
the engraving size predictable:

```xml
<svg xmlns="http://www.w3.org/2000/svg"
     width="40mm" height="20mm" viewBox="0 0 40 20">
  <path d="M 2 2 H 38 V 18 H 2 Z" fill="#808080" stroke="black"/>
</svg>
```

Without `--preprocess`, the converter reads `<path>` elements and styles set
directly on those paths, including inline `style` declarations. Path transforms
are applied, but inherited group styles and group transforms are not resolved.
Use `--preprocess` to normalize an SVG through `usvg`, resolve inherited styles,
and convert basic shapes such as rectangles and circles to paths. The converter
applies cumulative transforms even when preprocessing retains groups for clip
paths, masks, filters, or opacity. Paths inside definitions are excluded from
engraving. Convert text to paths in your editor before exporting.

### Dimensions and origin

- `mm`, `cm`, `in`, `pt`, and `pc` dimensions are converted to millimeters.
- Pixel dimensions and unitless `width`/`height` use `--dpi` (default: 96).
  `em` dimensions also use `--font-size` (default: 16 pixels).
- Percentage dimensions and `ex` dimensions are unsupported.
- If `width` or `height` is missing, the corresponding `viewBox` size is used
  directly as millimeters. This differs from specifying a unitless dimension.
- The Y axis is flipped so the SVG's bottom corresponds to the machine's lower
  edge. By default the drawing keeps its position within the SVG page;
  `--reset-origin` places the bottom-left of the generated engraving at `(0, 0)`.
- The `viewBox` origin is subtracted before scaling to millimeters, including
  negative origins. Paths, images, and user-space gradients share this mapping.

For predictable scaling, use a `viewBox` whose aspect ratio matches `width` and
`height`. The root page mapping scales X and Y independently and does not
implement the root `preserveAspectRatio` attribute.

## Fill colors, grayscale, and intensity

Different colors already produce different engraving strengths. There is no
need to convert the SVG to grayscale first. The converter parses CSS colors
(including named colors, hex, `rgb()`, and `rgba()`) and applies this mapping:

```text
gray = 0.2126 × red + 0.7152 × green + 0.0722 × blue
power = --strength × (1 − gray) × alpha
```

Red, green, blue, and alpha are normalized to the range 0–1. These are Rec.709
grayscale weights applied directly to the parsed RGB values, without gamma
linearization. Darker colors engrave with more power. White, `none`, and fully
transparent colors generate no engraving movements.

For `--strength 500`, the approximate output is:

| Color | Laser power |
| --- | ---: |
| `black` / `#000000` | `S500` |
| `#808080` | `S249` |
| `white` / `#ffffff` | Skipped |
| `red` / `#ff0000` | `S394` |
| `lime` / `#00ff00` | `S142` |
| `blue` / `#0000ff` | `S464` |
| `rgba(0, 0, 0, 0.5)` | `S250` |

These values are rounded for illustration; the output uses floating-point
values. Stroke colors control outlines; fill colors control infill. A missing
fill defaults to black, and a missing stroke defaults to none.

Try the supplied color swatches, arranged left to right in the table's order:

```sh
./target/release/svg2grbl --svg examples/colors.svg --reset-origin \
  --infill 0.2 --infill-pattern parallel --strength 500 > colors.gcode
```

### Engraving a background

An editor's page background is not automatically engraving geometry. Add an
explicit filled path covering the page, and enable `--infill`. For example,
save this as `background.svg`:

```xml
<svg xmlns="http://www.w3.org/2000/svg"
     width="40mm" height="20mm" viewBox="0 0 40 20">
  <path d="M 0 0 H 40 V 20 H 0 Z" fill="#bfbfbf"/>
</svg>
```

```sh
./target/release/svg2grbl --svg background.svg --infill 0.2 \
  --infill-pattern parallel --strength 500 > background.gcode
```

That gray produces roughly 25% of the selected maximum power. Change the fill
to a darker gray to increase intensity or a lighter gray to decrease it.
`--strength` scales power for the whole drawing; there is currently no separate
brightness, contrast, or gamma control.

Paths are processed independently, rather than rendered into a composited
image. A white shape over a dark background is skipped and does not erase the
background's engraving; overlapping colored paths can engrave the same area
more than once. To leave a blank region in a background, cut it out as a hole in
the same compound path, or subtract it in your SVG editor before exporting.

### Gradients

Linear and radial gradients referenced with `url(#id)` are supported, including
gradient stops, stop opacity, `href`/`xlink:href` inheritance, gradient transforms,
and `pad`, `reflect`, and `repeat` spread methods. The parser handles both
`objectBoundingBox` and `userSpaceOnUse` coordinate systems.

Each generated polyline receives one power value, sampled at the mean of its
vertices. Power does not vary along a single polyline. For a black-to-white
vertical gradient, horizontal parallel infill gives successive rows different
strengths:

```sh
./target/release/svg2grbl --svg examples/gradient.svg --infill 0.2 \
  --infill-pattern parallel --infill-angle 0 --strength 500 > gradient.gcode
```

This example uses direct path styles and does not need `--preprocess`. Gradient
references are resolved from the original SVG; preprocessing can rewrite those
references, so check the output if combining gradients with `--preprocess`.
Concentric rings and long gradient strokes can show little variation when their
sample points lie near the same location.

## PNG and JPEG images

SVG `<image>` elements can contain PNG or JPEG data URIs, or reference local
PNG/JPEG files through `href` or `xlink:href`. Referenced files are resolved
relative to the input SVG's directory; when reading the SVG from standard input,
they are resolved relative to the current working directory. Remote image URLs
are unsupported.

Images use the selected infill pattern and require a positive `--infill` spacing.
Parallel passes provide a conventional raster scan:

```sh
./target/release/svg2grbl --svg examples/raster.svg --infill 0.2 \
  --infill-pattern parallel --strength 500 --speed 1000 > raster.gcode
```

For a local image, an SVG wrapper can look like this:

```xml
<svg xmlns="http://www.w3.org/2000/svg"
     width="40mm" height="30mm" viewBox="0 0 40 30">
  <image x="0" y="0" width="40" height="30" href="photo.jpg"/>
</svg>
```

Each source pixel uses the same grayscale-to-power formula as vector colors.
Passes are split at source pixel boundaries, and each movement receives the
power of the pixel at its midpoint. `--max-line` additionally limits the segment
length in millimeters (default: 2 mm). White or fully transparent regions inside
a pass are traversed with `S0`; entirely blank passes and blank ends are skipped.

`--infill` controls spacing across passes; source pixel boundaries control detail
along each pass. Choose pass spacing small enough to capture the image's detail
at its printed size. Concentric, cross, and wavy infill also sample pixels; cross
infill engraves two sets of passes over the image.

Image placement respects `x`, `y`, `width`, `height`, image/group transforms, and
numeric image/group opacity. PNG alpha is multiplied by opacity. The default
`preserveAspectRatio="xMidYMid meet"` centers the image while keeping its aspect
ratio; `slice` crops to the image viewport, and `none` stretches to fit. Image
geometry can use SVG user units, physical lengths, or percentages. These image
settings work with and without `--preprocess`.

The image stage reads attributes and inline styles from the original SVG;
stylesheet rules and classes are not resolved for images. Images in definitions
are skipped, and `<use>` does not instantiate them. Masks, clip paths, filters,
and nested `<svg>` viewports on rendered images produce an error; apply those
effects to the PNG/JPEG before embedding it. Invalid or unreadable image data
also produces an error before any G-code is written.

## Infill

Filled paths and images require a positive `--infill` spacing in millimeters.
Without it, only stroked paths contribute engraving movements; a fill-only or
image-only SVG produces no engraving. Smaller spacing produces denser coverage
and more G-code.

| Pattern | Behavior |
| --- | --- |
| `concentric` (default) | Offset rings stepping inward, including the boundary when there is no stroke. |
| `parallel` | Straight passes at `--infill-angle`. |
| `cross` | Two sets of straight passes at the chosen angle and that angle + 90°. |
| `wavy` | Parallel passes following sine waves. |

For parallel, cross, and wavy patterns, a fill-only path contributes infill but
no separate boundary outline. Set a stroke if you also want an outline.

```sh
./target/release/svg2grbl --svg drawing.svg --preprocess --infill 0.3 \
  --infill-pattern cross --infill-angle 45 > cross.gcode

./target/release/svg2grbl --svg drawing.svg --preprocess --infill 0.3 \
  --infill-pattern wavy --infill-wave-amplitude 0.2 \
  --infill-wave-period 1.2 > wavy.gcode
```

Holes must be subpaths within the same `<path>` element. Infill uses containment
and even-odd parity regardless of the SVG's `fill-rule`; nested holes and islands
are supported. Laser power is disabled during travel between separate polylines,
including travel across holes.

## Command-line options

| Option | Default | Purpose |
| --- | --- | --- |
| `-s`, `--svg <FILE>` | Standard input | SVG input file. |
| `-p`, `--preprocess` | Off | Normalize SVG shapes, transforms, and styles with `usvg`. |
| `-r`, `--reset-origin` | Off | Place the generated drawing's bottom-left at `(0, 0)`. |
| `-t`, `--tolerance <VALUE>` | `0.15` | Curve flattening tolerance in SVG path units, before conversion to mm. |
| `-m`, `--max-line <MM>` | `2` | Maximum raster segment length; segments also split at source pixel boundaries. Does not change vector gradient sampling. |
| `--strength <VALUE>` | `500` | Maximum laser power (`S`) for opaque black. |
| `--speed <VALUE>` | `500` | Engraving feed rate (`F`) in mm/min. |
| `--dpi <VALUE>` | `96` | DPI used for pixel and unitless SVG dimensions. |
| `--font-size <VALUE>` | `16` | Font size in pixels used for `em` dimensions. |
| `--infill <MM>` | Disabled | Spacing between fill/image passes; must be positive. |
| `--infill-pattern <PATTERN>` | `concentric` | `concentric`, `parallel`, `cross`, or `wavy`. |
| `--infill-angle <DEGREES>` | `0` | Angle counterclockwise from +X for non-concentric infill. |
| `--infill-wave-amplitude <MM>` | Infill spacing | Sine-wave amplitude for wavy infill. |
| `--infill-wave-period <MM>` | 4 × infill spacing | Sine-wave period for wavy infill. |
| `-h`, `--help` | | Show help. |
| `-V`, `--version` | | Show version. |

## Output and limitations

The G-code sets millimeters (`G21`), absolute coordinates (`G90`), and feed per
minute (`G94`). Each polyline begins with the laser off (`M5`) and a rapid
positioning move (`G0`), then arms dynamic power with `M4 S0` before engraving
with `G1 ... F... S...`. Power is cleared with `S0` and `M5` after each polyline.

Vectors and images are processed independently, rather than compositing the
whole SVG into one raster image. Important limits are:

- SVG patterns, masks, clipping, and filters are not interpreted as rendered
  pixel coverage for vector paths. See the image section for supported fitting
  and explicit image errors.
- Stroke width and dash patterns are not reproduced by the G-code emitter.
- Color alpha and gradient stop opacity affect vector power, but separate
  `opacity`, `fill-opacity`, and `stroke-opacity` values are not applied to vector
  power. Images support numeric image/group opacity and pixel alpha.
- Unrecognized solid colors are skipped with a warning. Unresolved `url(...)`
  paints, including unsupported patterns, fall back to black at maximum power.
- Gradient sampling approximates SVG rendering; transformed paths, percentage
  coordinates in `userSpaceOnUse`, and nonuniform scaling can differ from an
  editor's preview.

## Development

```sh
cargo test --locked
cargo run --locked -- --help
```

Enable progress and diagnostic messages on standard error with `RUST_LOG`:

```sh
RUST_LOG=info ./target/release/svg2grbl --svg drawing.svg --infill 0.2 \
  > drawing.gcode
```

Vector geometry and G-code generation live in `src/main.rs`; image decoding,
placement, and sampling live in `src/raster.rs`. Unit and CLI tests cover
laser-off positioning, hole-aware infill, offset arcs, PNG/JPEG decoding,
per-pixel power, blank gaps, transforms, cropping, and local image references.
