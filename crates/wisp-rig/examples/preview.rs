//! Dump rig frames as SVG, so a person can look at a skin without a GPU.
//!
//! `wisp-rig` is pure geometry: everything else about it can be unit-tested,
//! but "does she read as a creature at 96px" cannot. This writes what the
//! renderer would be handed into files you can open, which is the review
//! loop DESIGN.md §11 asks for — *look* at it, do not read the diff.
//!
//! ```text
//! cargo run -p wisp-rig --example preview -- /tmp/wisp
//! ```
//!
//! It never opens a window and never touches the operator's config.

use std::fmt::Write as _;

use wisp_rig::contour::ContourOptions;
use wisp_rig::frame::RigFrame;
use wisp_rig::math::Vec2;
use wisp_rig::paint::{Paint, Rgba};
use wisp_rig::path::Verb;
use wisp_rig::rig::{Rig, RigInput};
use wisp_rig::skin::Skin;

fn main() {
    let mut args = std::env::args().skip(1);
    let out = args.next().unwrap_or_else(|| ".".to_string());
    let skin_path = args.next();

    let skin = match &skin_path {
        Some(p) => Skin::load(std::path::Path::new(p)).unwrap_or_else(|e| {
            eprintln!("{e}");
            std::process::exit(1);
        }),
        None => wisp_rig::default_skin().expect("the shipped skin compiles"),
    };
    println!(
        "{} by {} — {} bones, {} shapes, {} clips, {} expressions",
        skin.meta.name,
        skin.meta.author,
        skin.skeleton.len(),
        skin.shapes.len(),
        skin.clips.len(),
        skin.expressions.len()
    );

    std::fs::create_dir_all(&out).expect("could not create the output directory");

    // A sheet at each size the slider spans, plus one per expression, plus a
    // few motion states.
    for size in [48.0f32, 96.0, 128.0, 256.0, 512.0] {
        let mut rig = Rig::new(skin.clone());
        let input = centred(size);
        for _ in 0..8 {
            rig.update(1.0 / 60.0, &input);
        }
        write(&out, &format!("size-{size:.0}"), &mut rig, size);
    }

    for name in wisp_rig::REQUIRED_EXPRESSIONS {
        let mut rig = Rig::new(skin.clone());
        rig.set_expression(name);
        let input = centred(256.0);
        // Land on the middle of the expression's loop, where it is strongest.
        for _ in 0..60 {
            rig.update(1.0 / 60.0, &input);
        }
        write(&out, &format!("expr-{name}"), &mut rig, 256.0);
    }

    for (label, velocity) in [
        ("moving-right", Vec2::new(900.0, 0.0)),
        ("moving-left", Vec2::new(-900.0, 0.0)),
        ("falling", Vec2::new(0.0, 1100.0)),
    ] {
        let mut rig = Rig::new(skin.clone());
        let mut input = centred(256.0);
        input.velocity = velocity;
        for _ in 0..40 {
            rig.update(1.0 / 60.0, &input);
        }
        write(&out, label, &mut rig, 256.0);
    }

    for (label, cursor) in [
        ("looking-left", Vec2::new(-600.0, 300.0)),
        ("looking-right", Vec2::new(1400.0, 300.0)),
    ] {
        let mut rig = Rig::new(skin.clone());
        let mut input = centred(256.0);
        input.cursor = Some(cursor);
        for _ in 0..20 {
            rig.update(1.0 / 60.0, &input);
        }
        write(&out, label, &mut rig, 256.0);
    }

    println!("wrote SVGs to {out}");
}

fn centred(size: f32) -> RigInput {
    RigInput { size_px: size, anchor: Vec2::new(size * 1.5, size * 1.9), ..Default::default() }
}

fn write(dir: &str, name: &str, rig: &mut Rig, size: f32) {
    let contour = rig.contour(ContourOptions::default());
    let svg = to_svg(rig.frame(), Some(&contour), size * 3.0);
    let path = std::path::Path::new(dir).join(format!("{name}.svg"));
    std::fs::write(&path, svg).expect("could not write the SVG");
    println!(
        "  {name:<18} {} shapes, outline {} points / {} bytes",
        rig.frame().shapes.len(),
        contour.points.len(),
        contour.approx_bytes()
    );
}

/// Turn a frame into an SVG. This is the *only* renderer in this crate and it
/// exists purely for review — the real one is `wisp-paint`.
fn to_svg(f: &RigFrame, contour: Option<&wisp_rig::Polygon>, canvas: f32) -> String {
    let mut s = String::new();
    let _ = write!(
        s,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{canvas}" height="{canvas}" viewBox="0 0 {canvas} {canvas}">
<rect width="100%" height="100%" fill="#0a0714"/>
<defs>
"##
    );

    // Gradients first.
    for (i, shape) in f.shapes.iter().enumerate() {
        for (slot, paint) in [
            ("f", shape.fill.as_ref()),
            ("s", shape.stroke.as_ref().map(|st| &st.paint)),
        ] {
            let Some(p) = paint else { continue };
            match p {
                Paint::Solid(_) => {}
                Paint::Linear(g) => {
                    let _ = write!(
                        s,
                        r#"<linearGradient id="g{slot}{i}" gradientUnits="userSpaceOnUse" x1="{}" y1="{}" x2="{}" y2="{}">"#,
                        g.start.x, g.start.y, g.end.x, g.end.y
                    );
                    for st in &g.stops {
                        let _ = write!(s, "{}", stop(st.at, st.color));
                    }
                    let _ = writeln!(s, "</linearGradient>");
                }
                Paint::Radial(g) => {
                    let _ = write!(
                        s,
                        r#"<radialGradient id="g{slot}{i}" gradientUnits="userSpaceOnUse" cx="{}" cy="{}" r="{}" fx="{}" fy="{}">"#,
                        g.center.x, g.center.y, g.radius, g.focus.x, g.focus.y
                    );
                    for st in &g.stops {
                        let _ = write!(s, "{}", stop(st.at, st.color));
                    }
                    let _ = writeln!(s, "</radialGradient>");
                }
            }
        }
    }
    let _ = writeln!(s, "</defs>");

    for (i, shape) in f.shapes.iter().enumerate() {
        if !shape.is_visible() {
            continue;
        }
        let d = path_data(shape);
        let fill = match shape.fill.as_ref() {
            None => "none".to_string(),
            Some(Paint::Solid(c)) => solid(*c),
            Some(_) => format!("url(#gf{i})"),
        };
        let mut attrs = format!(r#"fill="{fill}""#);
        if let Some(st) = &shape.stroke {
            let stroke = match &st.paint {
                Paint::Solid(c) => solid(*c),
                _ => format!("url(#gs{i})"),
            };
            let _ = write!(attrs, r#" stroke="{stroke}" stroke-width="{}""#, st.width);
        } else {
            let _ = write!(attrs, r#" stroke="none""#);
        }
        if shape.opacity < 1.0 {
            let _ = write!(attrs, r#" opacity="{:.3}""#, shape.opacity);
        }
        let _ = writeln!(s, r#"<path d="{d}" {attrs}/>"#);
    }

    if let Some(c) = contour {
        if !c.is_empty() {
            let pts: Vec<String> = c
                .points
                .iter()
                .map(|p| format!("{:.1},{:.1}", p.x, p.y))
                .collect();
            let _ = writeln!(
                s,
                r##"<polygon points="{}" fill="none" stroke="#00e5ff" stroke-width="0.75" stroke-dasharray="3 3" opacity="0.5"/>"##,
                pts.join(" ")
            );
        }
    }

    let _ = writeln!(s, "</svg>");
    s
}

fn stop(at: f32, c: Rgba) -> String {
    format!(
        r#"<stop offset="{at}" stop-color="{}" stop-opacity="{:.3}"/>"#,
        Rgba { a: 1.0, ..c }.to_hex(),
        c.a
    )
}

/// SVG carries opacity in its own attribute, so the hex here is always the
/// opaque colour.
fn solid(c: Rgba) -> String {
    Rgba { a: 1.0, ..c }.to_hex()
}

fn path_data(shape: &wisp_rig::DrawShape) -> String {
    let mut d = String::new();
    for (verb, pts) in shape.segments() {
        match verb {
            Verb::Move => {
                let _ = write!(d, "M {:.2} {:.2} ", pts[0].x, pts[0].y);
            }
            Verb::Line => {
                let _ = write!(d, "L {:.2} {:.2} ", pts[0].x, pts[0].y);
            }
            Verb::Quad => {
                let _ = write!(
                    d,
                    "Q {:.2} {:.2} {:.2} {:.2} ",
                    pts[0].x, pts[0].y, pts[1].x, pts[1].y
                );
            }
            Verb::Cubic => {
                let _ = write!(
                    d,
                    "C {:.2} {:.2} {:.2} {:.2} {:.2} {:.2} ",
                    pts[0].x, pts[0].y, pts[1].x, pts[1].y, pts[2].x, pts[2].y
                );
            }
            Verb::Close => d.push_str("Z "),
        }
    }
    d
}
