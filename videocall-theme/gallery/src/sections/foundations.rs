// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tokens: colors, type, spacing and sizes, radii, elevation, motion.
//! Every list here comes from tokens.css through `token_groups()`.

use dioxus::prelude::*;
use videocall_theme::tokens::{self, Token};
use videocall_theme::*;

use super::{Section, Subhead};
use crate::{browser, token_groups, Gallery};

fn tokens_with_prefix(prefix: &str) -> Vec<&'static Token> {
    token_groups()
        .iter()
        .flat_map(|group| group.tokens.iter())
        .filter(|token| token.name.starts_with(prefix))
        .collect()
}

/// "1rem · 16px" from the resolved value and the file's trailing note.
fn value_label(gallery: &Gallery, token: &Token) -> String {
    let value = gallery.value(token.name);
    match token.note {
        Some(note) => format!("{value} · {note}"),
        None => value,
    }
}

fn parse_px(value: &str) -> Option<f64> {
    value.trim().strip_suffix("px")?.trim().parse().ok()
}

#[component]
pub fn Colors() -> Element {
    let gallery = use_context::<Gallery>();
    rsx! {
        Section {
            id: "colors",
            title: "Colors",
            lede: "Every color token, grouped as in tokens.css. The value under each name is read from the page, so it follows the appearance you pick below.",
            for group in token_groups().iter().filter(|group| group.tokens.iter().any(|t| tokens::is_color(t.name))) {
                Subhead { key: "{group.title}", "{group.title}" }
                div { class: "gallery-swatches",
                    for token in group.tokens.iter().filter(|t| tokens::is_color(t.name)) {
                        div { key: "{token.name}", class: "gallery-swatch",
                            div { class: "gallery-swatch-chip", style: "--sample: var({token.name})" }
                            code { class: "gallery-token-name", "{token.name}" }
                            span { class: "gallery-token-value", {gallery.value(token.name)} }
                        }
                    }
                }
            }
        }
    }
}

#[component]
pub fn Type() -> Element {
    let styles: Vec<&Token> = tokens_with_prefix("--text-")
        .into_iter()
        .filter(|token| !token.name.ends_with("-emphasized"))
        .collect();
    rsx! {
        Section {
            id: "type",
            title: "Type",
            lede: "The 11 iOS text styles at the default Large size, each next to its emphasized variant. Size and leading are in points, from the HIG table.",
            div { class: "gallery-type card",
                div { class: "gallery-type-row gallery-type-head text-footnote text-secondary",
                    span { "Style" }
                    span { "Regular" }
                    span { "Emphasized" }
                }
                for token in styles {
                    div { key: "{token.name}", class: "gallery-type-row",
                        div { class: "gallery-type-name",
                            code { class: "gallery-token-name", "{token.name}" }
                            span { class: "gallery-token-value", {token.note.unwrap_or("")} }
                        }
                        span { style: "font: var({token.name})", "Join the meeting" }
                        span { style: "font: var({token.name}-emphasized)", "Join the meeting" }
                    }
                }
            }
        }
    }
}

#[component]
pub fn Spacing() -> Element {
    let gallery = use_context::<Gallery>();
    rsx! {
        Section {
            id: "spacing",
            title: "Spacing and sizes",
            lede: "A 4px grid in rem, so it follows the browser's text size. 44px is Apple's minimum hit region and stays in px.",
            Subhead { "Spacing" }
            div { class: "card gallery-scale",
                for token in tokens_with_prefix("--space-") {
                    div { key: "{token.name}", class: "gallery-scale-row",
                        code { class: "gallery-token-name", "{token.name}" }
                        div { class: "gallery-bar", style: "--sample: var({token.name})" }
                        span { class: "gallery-token-value", {value_label(&gallery, token)} }
                    }
                }
            }
            Subhead { "Sizes" }
            div { class: "gallery-tiles",
                for token in tokens_with_prefix("--size-") {
                    div { key: "{token.name}", class: "gallery-tile",
                        div { class: "gallery-size-box", style: "--sample: var({token.name})" }
                        code { class: "gallery-token-name", "{token.name}" }
                        span { class: "gallery-token-value", {gallery.value(token.name)} }
                    }
                }
            }
        }
    }
}

#[component]
pub fn Radii() -> Element {
    let gallery = use_context::<Gallery>();
    let mut inset = use_signal(|| 20u32);
    let outer = parse_px(&gallery.value("--radius-xl")).unwrap_or(28.0);
    let fallback = parse_px(&gallery.value("--radius-fallback")).unwrap_or(10.0);
    let inner = (outer - f64::from(inset())).max(fallback);
    rsx! {
        Section {
            id: "radii",
            title: "Radii",
            lede: "Fixed, capsule and concentric. Apple publishes no radius, so these numbers are approximations.",
            div { class: "gallery-tiles",
                for token in tokens_with_prefix("--radius-") {
                    div { key: "{token.name}", class: "gallery-tile",
                        div {
                            class: if token.name == "--radius-capsule" { "gallery-radius-tile gallery-radius-capsule" } else { "gallery-radius-tile" },
                            style: "--sample: var({token.name})",
                        }
                        code { class: "gallery-token-name", "{token.name}" }
                        span { class: "gallery-token-value", {gallery.value(token.name)} }
                    }
                }
            }
            Subhead { "Concentric" }
            p { class: "text-subheadline text-secondary",
                "The parent sets --concentric-outer and --concentric-inset; the child takes .radius-concentric. "
                "Drag the padding and the inner corner follows, down to --radius-fallback."
            }
            div { class: "gallery-concentric-demo",
                div {
                    class: "gallery-concentric",
                    style: "--concentric-outer: var(--radius-xl); --concentric-inset: {inset}px;",
                    div { class: "radius-concentric gallery-concentric-inner" }
                }
                div { class: "stack gap-2 grow",
                    label { r#for: "gallery-inset", class: "field-label", "Padding: {inset}px" }
                    input {
                        id: "gallery-inset",
                        class: "gallery-range",
                        r#type: "range",
                        min: "0",
                        max: "40",
                        value: "{inset}",
                        oninput: move |evt| inset.set(evt.value().parse().unwrap_or(20)),
                    }
                    code { class: "gallery-token-value",
                        "max({outer}px − {inset}px, {fallback}px) = {inner}px"
                    }
                }
            }
        }
    }
}

#[component]
pub fn Elevation() -> Element {
    let gallery = use_context::<Gallery>();
    let mut layers: Vec<(&Token, i64)> = tokens_with_prefix("--z-")
        .into_iter()
        .map(|token| (token, token.value.trim().parse::<i64>().unwrap_or(0)))
        .collect();
    layers.sort_by_key(|(_, value)| *value);
    rsx! {
        Section {
            id: "elevation",
            title: "Shadows and layers",
            lede: "Shadow 1 is for controls and cards, 2 for menus and toasts, 3 for sheets and alerts. The z-index scale runs bottom to top; the gaps leave room for calc(var(--z-x) + 1).",
            div { class: "gallery-elevation",
                for token in tokens_with_prefix("--shadow-") {
                    div { key: "{token.name}", class: "gallery-shadow-tile", style: "--sample: var({token.name})",
                        code { class: "gallery-token-name", "{token.name}" }
                    }
                }
            }
            Subhead { "Stacking order" }
            div { class: "gallery-z",
                for (index, (token, value)) in layers.iter().enumerate() {
                    div {
                        key: "{token.name}",
                        class: "gallery-z-tile",
                        style: "--i: {index}; z-index: var({token.name});",
                        code { class: "gallery-token-name", "{token.name}" }
                        span { class: "gallery-token-value", "{value}" }
                    }
                }
            }
            p { class: "text-footnote text-secondary", "Each tile sits on its own z-index, so a later tile covers the one before it. " {gallery.value("--z-lock")} " is the baby-mode lock, above everything." }
        }
    }
}

#[component]
pub fn Motion() -> Element {
    let gallery = use_context::<Gallery>();
    let mut playing = use_signal(|| false);
    // Subscribe to OS changes so the note below stays true.
    let _ = (gallery.os_tick)();
    let reduced = gallery.preview.read().reduce_motion
        || browser::media_matches("(prefers-reduced-motion: reduce)");
    let durations = tokens_with_prefix("--duration-");
    let easings = tokens_with_prefix("--ease-");
    rsx! {
        Section {
            id: "motion",
            title: "Motion",
            lede: "Three transition durations, four longer ones named for what they time, and three curves, all approximations. Only transform and opacity move. Under Reduce Motion every transition and animation jumps to its end (base.css) but keeps its delay, so timed things still happen on time. The spinner slows down instead.",
            div { class: "row gap-4 gallery-wrap",
                Button {
                    variant: ButtonVariant::Tinted,
                    pressed: playing(),
                    onclick: move |_| playing.toggle(),
                    if playing() { "Move back" } else { "Play" }
                }
                if reduced {
                    Badge { tone: BadgeTone::Warning, "Reduce Motion is on: no movement" }
                } else {
                    span { class: "text-footnote text-secondary", "Turn on Reduce Motion from the bar below to compare." }
                }
            }
            div { class: "card gallery-motion", "data-on": if playing() { "true" } else { "false" },
                for token in durations.iter() {
                    div { key: "{token.name}", class: "gallery-motion-row",
                        div { class: "gallery-motion-label",
                            code { class: "gallery-token-name", "{token.name}" }
                            span { class: "gallery-token-value",
                                "{token.value} · --ease-standard"
                                if let Some(note) = token.note { " · {note}" }
                            }
                        }
                        div { class: "gallery-rail",
                            style: "--sample-duration: var({token.name}); --sample-ease: var(--ease-standard);",
                            div { class: "gallery-rail-runner", div { class: "gallery-rail-dot" } }
                        }
                    }
                }
                for token in easings.iter() {
                    div { key: "{token.name}", class: "gallery-motion-row",
                        div { class: "gallery-motion-label",
                            code { class: "gallery-token-name", "{token.name}" }
                            span { class: "gallery-token-value", "{token.value}" }
                        }
                        div { class: "gallery-rail",
                            style: "--sample-duration: var(--duration-slow); --sample-ease: var({token.name});",
                            div { class: "gallery-rail-runner", div { class: "gallery-rail-dot" } }
                        }
                    }
                }
            }
        }
    }
}
