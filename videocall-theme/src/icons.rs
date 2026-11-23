// SPDX-License-Identifier: MIT OR Apache-2.0

//! The icon set. One list of glyphs, drawn by [`crate::Icon`], shared by
//! dioxus-ui and the gallery so the two can't show different pictures in
//! the same control.
//!
//! The glyphs are the ones dioxus-ui shipped before they moved here (most
//! are Feather icons on a 24-unit grid, stroked at 2 in the current text
//! color). Each shape keeps the SVG element it had, a `line` stays a
//! `line`. The move was not pixel-identical, though: strokes now get round
//! line caps and joins, so the glyphs in `meeting_ended_overlay.rs` and
//! `pages/meeting.rs` (which drew butt caps and miter joins before) look
//! slightly softer at the ends and corners.
//!
//! Everything here is `&'static` data. An app only links the glyphs it
//! names, and all of them share the one `Icon` component.
//!
//! Brand marks (Google, Okta, YouTube, Discord, DigitalOcean) and charts
//! are not icons and stay in the app.

/// How a glyph is painted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Paint {
    /// Outlined: `fill="none"`, `stroke="currentColor"`, round caps and
    /// joins. The stroke width defaults to 2.
    Stroke,
    /// Solid: `fill="currentColor"`, no stroke.
    Fill,
}

/// One SVG element of a glyph. Coordinates are kept as the strings the
/// markup uses, so nothing is reformatted on the way to the DOM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// `<path d>`.
    Path(&'static str),
    /// `<path d fill-rule="evenodd">`.
    PathEvenOdd(&'static str),
    /// `<line x1 y1 x2 y2>`.
    Line(&'static str, &'static str, &'static str, &'static str),
    /// `<circle cx cy r>`.
    Circle(&'static str, &'static str, &'static str),
    /// `<rect x y width height rx ry>`, with `rx` used for both radii.
    Rect {
        x: &'static str,
        y: &'static str,
        width: &'static str,
        height: &'static str,
        rx: &'static str,
    },
    /// `<polyline points>`.
    Polyline(&'static str),
    /// `<polygon points>`.
    Polygon(&'static str),
}

/// A named icon: its view box, how it is painted, and its shapes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Glyph {
    /// The constant's name in lower case, e.g. `"mic-off"`. The gallery
    /// lists icons by it and tests compare it.
    pub name: &'static str,
    pub view_box: &'static str,
    pub paint: Paint,
    pub shapes: &'static [Shape],
}

const GRID: &str = "0 0 24 24";

const fn stroke(name: &'static str, shapes: &'static [Shape]) -> Glyph {
    Glyph {
        name,
        view_box: GRID,
        paint: Paint::Stroke,
        shapes,
    }
}

use Shape::{Circle, Line, Path, PathEvenOdd, Polygon, Polyline};

const fn rect(
    x: &'static str,
    y: &'static str,
    width: &'static str,
    height: &'static str,
    rx: &'static str,
) -> Shape {
    Shape::Rect {
        x,
        y,
        width,
        height,
        rx,
    }
}

// Shared parts, so a glyph and its crossed-out twin stay in step.
const PEOPLE_SHAPES: [Shape; 4] = [
    Path("M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2"),
    Circle("9", "7", "4"),
    Path("M23 21v-2a4 4 0 0 0-3-3.87"),
    Path("M16 3.13a4 4 0 0 1 0 7.75"),
];
const ACTIVITY_LINE: Shape = Path("M2 12h2l3.5-7L12 19l2.5-5H20");
const CIRCLE_24: Shape = Circle("12", "12", "10");
const USER_SHAPES: [Shape; 2] = [
    Path("M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"),
    Circle("9", "7", "4"),
];
const SLASH: Shape = Line("1", "1", "23", "23");
const MIC_STAND: Shape = Line("12", "19", "12", "22");

// ---------------------------------------------------------------------------
// Call controls
// ---------------------------------------------------------------------------

pub const MIC: Glyph = stroke(
    "mic",
    &[
        Path("M12 2a3 3 0 0 0-3 3v7a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3z"),
        Path("M19 10v2a7 7 0 0 1-14 0v-2"),
        MIC_STAND,
    ],
);
pub const MIC_OFF: Glyph = stroke(
    "mic-off",
    &[
        SLASH,
        Path("M9 9v3a3 3 0 0 0 5.12 2.12M15 9.34V5a3 3 0 0 0-5.94-.6"),
        Path("M17 16.95A7 7 0 0 1 5 12v-2m14 0v2a7 7 0 0 1-.11 1.23"),
        MIC_STAND,
    ],
);
pub const CAMERA: Glyph = stroke(
    "camera",
    &[
        Polygon("23 7 16 12 23 17 23 7"),
        rect("1", "5", "15", "14", "2"),
    ],
);
pub const CAMERA_OFF: Glyph = stroke(
    "camera-off",
    &[
        Path("M16 16v1a2 2 0 0 1-2 2H3a2 2 0 0 1-2-2V7a2 2 0 0 1 2-2h2m5.66 0H14a2 2 0 0 1 2 2v3.34l1 1L23 7v10"),
        SLASH,
    ],
);
/// Screen share, on.
pub const SCREEN: Glyph = stroke(
    "screen",
    &[
        rect("2", "3", "20", "14", "2"),
        Line("8", "21", "16", "21"),
        Line("12", "17", "12", "21"),
    ],
);
/// Screen share, off.
pub const SCREEN_OFF: Glyph = stroke(
    "screen-off",
    &[
        Path("M13 3H4a2 2 0 0 0-2 2v10a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-3"),
        Polyline("8 21 12 17 16 21"),
        Polyline("16 7 20 7 20 3"),
        SLASH,
    ],
);
pub const PEOPLE: Glyph = stroke("people", &PEOPLE_SHAPES);
pub const PEOPLE_OFF: Glyph = stroke(
    "people-off",
    &[
        PEOPLE_SHAPES[0],
        PEOPLE_SHAPES[1],
        PEOPLE_SHAPES[2],
        PEOPLE_SHAPES[3],
        SLASH,
    ],
);
pub const ACTIVITY: Glyph = stroke("activity", &[ACTIVITY_LINE]);
pub const ACTIVITY_OFF: Glyph =
    stroke("activity-off", &[ACTIVITY_LINE, Line("3", "3", "21", "21")]);
pub const SETTINGS: Glyph = stroke(
    "settings",
    &[
        Circle("12", "12", "3"),
        Path("M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1 0 2.83 2 2 0 0 1-2.83 0l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-2 2 2 2 0 0 1-2-2v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06-.06a2 2 0 0 1-2.83 0 2 2 0 0 1 0-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1-2-2 2 2 0 0 1 2-2h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 0-2.83 2 2 0 0 1 2.83 0l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1 1.51V3a2 2 0 0 1 2-2 2 2 0 0 1 2 2v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 0 2 2 0 0 1 0 2.83l-.06-.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 2 2 2 2 0 0 1-2 2h-.09a1.65 1.65 0 0 0-1.51 1z"),
    ],
);
/// Baby mode: a baby's face.
pub const BABY: Glyph = stroke(
    "baby",
    &[
        Path("M9 12h.01"),
        Path("M15 12h.01"),
        Path("M10 16c.5.3 1.2.5 2 .5s1.5-.2 2-.5"),
        Path("M19 6.3a9 9 0 0 1 1.8 3.9 2 2 0 0 1 0 3.6 9 9 0 0 1-17.6 0 2 2 0 0 1 0-3.6A9 9 0 0 1 12 3c2 0 3.5 1.1 3.5 2.5s-.9 2.5-2 2.5c-.8 0-1.5-.4-1.5-1"),
    ],
);
/// The More button: three dots in a row.
pub const MORE: Glyph = stroke(
    "more",
    &[
        Circle("12", "12", "1"),
        Circle("19", "12", "1"),
        Circle("5", "12", "1"),
    ],
);
pub const HANG_UP: Glyph = Glyph {
    name: "hang-up",
    view_box: GRID,
    paint: Paint::Fill,
    shapes: &[Path("M12.017 6.995c-2.306 0-4.534.408-6.215 1.507-1.737 1.135-2.788 2.944-2.797 5.451a4.8 4.8 0 0 0 .01.62c.015.193.047.512.138.763a2.557 2.557 0 0 0 2.579 1.677H7.31a2.685 2.685 0 0 0 2.685-2.684v-.645a.684.684 0 0 1 .684-.684h2.647a.686.686 0 0 1 .686.687v.645c0 .712.284 1.395.787 1.898.478.478 1.101.787 1.847.787h1.647a2.555 2.555 0 0 0 2.575-1.674c.09-.25.123-.57.137-.763.015-.2.022-.433.01-.617-.002-2.508-1.049-4.32-2.785-5.458-1.68-1.1-3.907-1.51-6.213-1.51Z")],
};

// ---------------------------------------------------------------------------
// Video tiles and the peer list
// ---------------------------------------------------------------------------

pub const CROP: Glyph = stroke(
    "crop",
    &[
        Path("M6 3v12a3 3 0 003 3h12"),
        Path("M18 21V9a3 3 0 00-3-3H3"),
    ],
);
pub const PUSH_PIN: Glyph = Glyph {
    name: "push-pin",
    view_box: GRID,
    paint: Paint::Fill,
    shapes: &[PathEvenOdd("M16,9V4l1,0c0.55,0,1-0.45,1-1v0c0-0.55-0.45-1-1-1H7C6.45,2,6,2.45,6,3v0 c0,0.55,0.45,1,1,1l1,0v5c0,1.66-1.34,3-3,3h0v2h5.97v7l1,1l1-1v-7H19v-2h0C17.34,12,16,10.66,16,9z")],
};
/// The generic person avatar. A 512-unit grid, solid.
pub const AVATAR: Glyph = Glyph {
    name: "avatar",
    view_box: "0 0 512 512",
    paint: Paint::Fill,
    shapes: &[
        Path("M458.159,404.216c-18.93-33.65-49.934-71.764-100.409-93.431c-28.868,20.196-63.938,32.087-101.745,32.087 c-37.828,0-72.898-11.89-101.767-32.087c-50.474,21.667-81.479,59.782-100.398,93.431C28.731,448.848,48.417,512,91.842,512 c43.426,0,164.164,0,164.164,0s120.726,0,164.153,0C463.583,512,483.269,448.848,458.159,404.216z"),
        Path("M256.005,300.641c74.144,0,134.231-60.108,134.231-134.242v-32.158C390.236,60.108,330.149,0,256.005,0 c-74.155,0-134.252,60.108-134.252,134.242V166.4C121.753,240.533,181.851,300.641,256.005,300.641z"),
    ],
};
/// Vertical dots: a row's own menu.
pub const MORE_VERTICAL: Glyph = stroke(
    "more-vertical",
    &[
        Circle("12", "12", "1"),
        Circle("12", "5", "1"),
        Circle("12", "19", "1"),
    ],
);
pub const SEARCH: Glyph = stroke(
    "search",
    &[Circle("11", "11", "7"), Line("21", "21", "16.65", "16.65")],
);
pub const USER_PLUS: Glyph = stroke(
    "user-plus",
    &[
        USER_SHAPES[0],
        USER_SHAPES[1],
        Line("19", "8", "19", "14"),
        Line("22", "11", "16", "11"),
    ],
);
pub const USER_MINUS: Glyph = stroke(
    "user-minus",
    &[USER_SHAPES[0], USER_SHAPES[1], Line("22", "11", "16", "11")],
);

// ---------------------------------------------------------------------------
// General
// ---------------------------------------------------------------------------

pub const CLOSE: Glyph = stroke(
    "close",
    &[Line("18", "6", "6", "18"), Line("6", "6", "18", "18")],
);
pub const CHECK: Glyph = stroke("check", &[Polyline("20 6 9 17 4 12")]);
pub const PLUS: Glyph = stroke(
    "plus",
    &[Line("12", "5", "12", "19"), Line("5", "12", "19", "12")],
);
pub const CHEVRON_DOWN: Glyph = stroke("chevron-down", &[Polyline("6 9 12 15 18 9")]);
pub const CHEVRON_LEFT: Glyph = stroke("chevron-left", &[Polyline("15 18 9 12 15 6")]);
pub const CHEVRON_RIGHT: Glyph = stroke("chevron-right", &[Polyline("9 18 15 12 9 6")]);
pub const PENCIL: Glyph = stroke(
    "pencil",
    &[
        Path("M12 20h9"),
        Path("M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4Z"),
    ],
);
pub const TRASH: Glyph = stroke(
    "trash",
    &[
        Polyline("3 6 5 6 21 6"),
        Path("M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"),
        Line("10", "11", "10", "17"),
        Line("14", "11", "14", "17"),
    ],
);
pub const LOCK: Glyph = stroke(
    "lock",
    &[
        rect("3", "11", "18", "11", "2"),
        Path("M7 11V7a5 5 0 0 1 10 0v4"),
    ],
);
pub const CLOCK: Glyph = stroke("clock", &[CIRCLE_24, Polyline("12 6 12 12 16 14")]);
pub const CALENDAR: Glyph = stroke(
    "calendar",
    &[
        rect("3", "4", "18", "18", "2"),
        Line("16", "2", "16", "6"),
        Line("8", "2", "8", "6"),
        Line("3", "10", "21", "10"),
    ],
);
/// An exclamation mark in a circle.
pub const ALERT: Glyph = stroke(
    "alert",
    &[
        CIRCLE_24,
        Line("12", "8", "12", "12"),
        Line("12", "16", "12.01", "16"),
    ],
);
/// An "i" in a circle.
pub const INFO: Glyph = stroke(
    "info",
    &[
        CIRCLE_24,
        Line("12", "16", "12", "12"),
        Line("12", "8", "12.01", "8"),
    ],
);
/// An X in a circle.
pub const ERROR: Glyph = stroke(
    "error",
    &[
        CIRCLE_24,
        Line("15", "9", "9", "15"),
        Line("9", "9", "15", "15"),
    ],
);
/// An arrow turning right into a wall: join.
pub const ENTER: Glyph = stroke(
    "enter",
    &[Path("M15 10l5 5-5 5"), Path("M20 15H8"), Path("M4 4v16")],
);
pub const PLAY: Glyph = stroke("play", &[Polygon("5 3 19 12 5 21 5 3")]);
pub const STOP: Glyph = stroke("stop", &[rect("3", "3", "18", "18", "2")]);
pub const SHARE: Glyph = stroke(
    "share",
    &[
        Path("M12 15V4"),
        Path("M8 8l4-4 4 4"),
        Path("M5 13v6h14v-6"),
    ],
);
pub const CHAT: Glyph = stroke("chat", &[Path("M4 5h16v11H9l-5 4z")]);
pub const ACCESSIBILITY: Glyph = stroke(
    "accessibility",
    &[
        Path("M12 6.5a1.5 1.5 0 1 0 0-3 1.5 1.5 0 0 0 0 3z"),
        Path("M5 9l7 1 7-1"),
        Path("M12 10v4"),
        Path("M9 20l3-6 3 6"),
    ],
);

/// Every glyph, in the order the gallery lists them.
pub const ALL: &[&Glyph] = &[
    &MIC,
    &MIC_OFF,
    &CAMERA,
    &CAMERA_OFF,
    &SCREEN,
    &SCREEN_OFF,
    &PEOPLE,
    &PEOPLE_OFF,
    &ACTIVITY,
    &ACTIVITY_OFF,
    &SETTINGS,
    &BABY,
    &MORE,
    &HANG_UP,
    &CROP,
    &PUSH_PIN,
    &AVATAR,
    &MORE_VERTICAL,
    &SEARCH,
    &USER_PLUS,
    &USER_MINUS,
    &CLOSE,
    &CHECK,
    &PLUS,
    &CHEVRON_DOWN,
    &CHEVRON_LEFT,
    &CHEVRON_RIGHT,
    &PENCIL,
    &TRASH,
    &LOCK,
    &CLOCK,
    &CALENDAR,
    &ALERT,
    &INFO,
    &ERROR,
    &ENTER,
    &PLAY,
    &STOP,
    &SHARE,
    &CHAT,
    &ACCESSIBILITY,
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn names_are_unique_and_kebab_case() {
        let mut seen = HashSet::new();
        for glyph in ALL {
            assert!(seen.insert(glyph.name), "duplicate name {}", glyph.name);
            assert!(
                glyph
                    .name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '-'),
                "{}",
                glyph.name
            );
            assert!(!glyph.shapes.is_empty(), "{} has no shapes", glyph.name);
        }
    }

    #[test]
    fn no_two_glyphs_draw_the_same_picture() {
        for (index, glyph) in ALL.iter().enumerate() {
            for other in &ALL[index + 1..] {
                assert!(
                    glyph.shapes != other.shapes || glyph.view_box != other.view_box,
                    "{} and {} are the same glyph",
                    glyph.name,
                    other.name
                );
            }
        }
    }
}
