// SPDX-License-Identifier: MIT OR Apache-2.0

//! `Icon`: draws a glyph from [`crate::icons`].

use dioxus::prelude::*;

use crate::icons::{Glyph, Paint, Shape};

/// Draws `glyph` as an inline SVG in the current text color.
///
/// One component for every icon, so the app pays for its render glue once.
/// `size` sets both `width` and `height` (leave it out to size the icon from
/// CSS). `stroke_width` overrides the default 2 on an outlined glyph.
///
/// The icon adds no ARIA of its own. Inside a named control it says nothing
/// to a screen reader either way; pass `"aria-hidden": "true"` where it sits
/// next to text, as the call sites in dioxus-ui do.
#[component]
pub fn Icon(
    glyph: &'static Glyph,
    #[props(default)] size: Option<&'static str>,
    #[props(default)] stroke_width: Option<&'static str>,
    #[props(default)] class: Option<String>,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
) -> Element {
    let shapes = glyph.shapes.iter().map(shape);
    // One template for both paints: the stroke attributes are left out
    // (None) on a solid glyph.
    let stroked = glyph.paint == Paint::Stroke;
    rsx! {
        svg {
            xmlns: "http://www.w3.org/2000/svg",
            view_box: glyph.view_box,
            fill: if stroked { "none" } else { "currentColor" },
            stroke: stroked.then_some("currentColor"),
            stroke_width: stroked.then(|| stroke_width.unwrap_or("2")),
            stroke_linecap: stroked.then_some("round"),
            stroke_linejoin: stroked.then_some("round"),
            width: size,
            height: size,
            class,
            ..attributes,
            {shapes}
        }
    }
}

fn shape(shape: &'static Shape) -> Element {
    match *shape {
        Shape::Path(d) => rsx! {
            path { d }
        },
        Shape::PathEvenOdd(d) => rsx! {
            path { d, fill_rule: "evenodd" }
        },
        Shape::Line(x1, y1, x2, y2) => rsx! {
            line { x1, y1, x2, y2 }
        },
        Shape::Circle(cx, cy, r) => rsx! {
            circle { cx, cy, r }
        },
        Shape::Rect {
            x,
            y,
            width,
            height,
            rx,
        } => rsx! {
            rect { x, y, width, height, rx, ry: rx }
        },
        Shape::Polyline(points) => rsx! {
            polyline { points }
        },
        Shape::Polygon(points) => rsx! {
            polygon { points }
        },
    }
}
