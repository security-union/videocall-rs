// SPDX-License-Identifier: MIT OR Apache-2.0

//! The theme gallery: every token and primitive of videocall-theme on one
//! page, built only from the crate's own stylesheets and components.
//!
//! Serve it with `make theme-gallery` (trunk, port 3002).
//!
//! The three theme sheets are not linked by Trunk. They are injected as
//! `<style>` elements from the crate's constants, run through
//! [`videocall_theme::preview::apply`] first, so the floating toolbar can
//! simulate Increase Contrast, Reduce Transparency and Reduce Motion without
//! touching the files. Light and Dark are the real pins from tokens.css
//! (`.theme-light`, `.theme-dark`), set on `<html>`.

mod browser;
mod icons;
mod sections;

use std::collections::HashMap;
use std::sync::OnceLock;

use dioxus::core::Runtime;
use dioxus::prelude::*;
use videocall_theme::preview::{apply, Preview, THEME_DARK, THEME_LIGHT};
use videocall_theme::tokens::{self, TokenGroup};
use videocall_theme::*;

/// Layout for the gallery itself, in the `gallery` layer.
const GALLERY_CSS: &str = include_str!("../gallery.css");

/// The OS settings whose changes should refresh the resolved token values.
const WATCHED_QUERIES: [&str; 4] = [
    "(prefers-color-scheme: dark)",
    "(prefers-contrast: more)",
    "(prefers-reduced-transparency: reduce)",
    "(prefers-reduced-motion: reduce)",
];

/// `(anchor id, label)` for the section index.
pub const SECTIONS: [(&str, &str); 14] = [
    ("colors", "Colors"),
    ("type", "Type"),
    ("spacing", "Spacing and sizes"),
    ("radii", "Radii"),
    ("elevation", "Shadows and layers"),
    ("motion", "Motion"),
    ("materials", "Materials"),
    ("buttons", "Buttons"),
    ("forms", "Forms"),
    ("lists", "Lists and cards"),
    ("overlays", "Overlays"),
    ("navigation", "Navigation"),
    ("status", "Status"),
    ("call", "In-call"),
];

/// Which appearance the page shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Appearance {
    /// Follow the OS. No pin.
    #[default]
    System,
    /// `.theme-light` on `<html>`.
    Light,
    /// `.theme-dark` on `<html>`.
    Dark,
}

/// Shared gallery state, provided as context. All fields are signals, so it
/// is `Copy`.
#[derive(Clone, Copy)]
pub struct Gallery {
    /// The appearance pin.
    pub appearance: Signal<Appearance>,
    /// The accessibility settings the toolbar simulates.
    pub preview: Signal<Preview>,
    /// Bumped whenever one of [`WATCHED_QUERIES`] changes in the OS.
    pub os_tick: Signal<u32>,
    /// Every token's computed value on the root element, read after each
    /// appearance change.
    pub resolved: Signal<HashMap<&'static str, String>>,
    /// Open toasts, `(id, text)`.
    pub toasts: Signal<Vec<(u32, String)>>,
    next_toast: Signal<u32>,
}

impl Gallery {
    /// The computed value of `name`, or an empty string before the first read.
    pub fn value(&self, name: &str) -> String {
        self.resolved.read().get(name).cloned().unwrap_or_default()
    }

    pub fn show_toast(&mut self, text: impl Into<String>) {
        let id = *self.next_toast.peek();
        self.next_toast.set(id + 1);
        let mut toasts = self.toasts.write();
        toasts.push((id, text.into()));
        // Three at most, newest last, like a stack that cannot grow forever.
        let excess = toasts.len().saturating_sub(3);
        toasts.drain(..excess);
    }

    pub fn dismiss_toast(&mut self, id: u32) {
        self.toasts.write().retain(|(toast, _)| *toast != id);
    }
}

/// tokens.css, grouped. Parsed once.
pub fn token_groups() -> &'static [TokenGroup] {
    static GROUPS: OnceLock<Vec<TokenGroup>> = OnceLock::new();
    GROUPS.get_or_init(|| tokens::groups(TOKENS_CSS))
}

fn token_names() -> &'static [&'static str] {
    static NAMES: OnceLock<Vec<&'static str>> = OnceLock::new();
    NAMES.get_or_init(|| {
        token_groups()
            .iter()
            .flat_map(|group| group.tokens.iter().map(|token| token.name))
            .collect()
    })
}

fn main() {
    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    let appearance = use_signal(Appearance::default);
    let preview = use_signal(Preview::default);
    let os_tick = use_signal(|| 0u32);
    let mut resolved = use_signal(HashMap::new);
    let mut gallery = use_context_provider(|| Gallery {
        appearance,
        preview,
        os_tick,
        resolved,
        toasts: Signal::new(Vec::new()),
        next_toast: Signal::new(0),
    });

    // The listeners live as long as the page; the gallery root never
    // unmounts. wrap_closure runs the callback inside the Dioxus runtime,
    // which a signal write needs.
    use_hook(move || {
        let bump = Runtime::wrap_closure(move |()| {
            let mut tick = os_tick;
            tick += 1;
        });
        browser::watch_media(&WATCHED_QUERIES, move || bump(()));
    });

    let sheets = use_memo(move || {
        let preview = preview();
        STYLESHEETS
            .iter()
            .map(|(_, css)| apply(css, &preview).into_owned())
            .collect::<Vec<String>>()
    });

    // Effects run after the render that changed the <style> text, so the
    // computed values here are the new ones. The pin goes on first, in the
    // same effect, so the read sees it.
    use_effect(move || {
        let appearance = appearance();
        browser::set_root_class(THEME_LIGHT, appearance == Appearance::Light);
        browser::set_root_class(THEME_DARK, appearance == Appearance::Dark);
        let _ = preview();
        let _ = os_tick();
        resolved.set(browser::read_properties(token_names()));
    });

    let mut current = use_signal(browser::current_hash);
    use_hook(move || {
        let sync = Runtime::wrap_closure(move |()| {
            let mut current = current;
            current.set(browser::current_hash());
        });
        browser::watch_hash(move || sync(()));
    });
    let toasts = gallery.toasts.read().clone();

    rsx! {
        style { "{sheets.read()[0]}" }
        style { "{sheets.read()[1]}" }
        style { "{sheets.read()[2]}" }
        style { {GALLERY_CSS} }

        div { class: "gallery",
            nav { class: "gallery-nav glass-regular", aria_label: "Sections",
                div { class: "gallery-brand",
                    span { class: "text-headline", "videocall-theme" }
                    span { class: "text-footnote text-secondary", "Design system gallery" }
                }
                TabList { class: "gallery-nav-column", role: "none",
                    for (id, label) in SECTIONS {
                        Tab {
                            key: "{id}",
                            href: "#{id}",
                            selected: current() == id,
                            onclick: move |_| current.set(id.to_string()),
                            "{label}"
                        }
                    }
                }
                TabList { class: "gallery-nav-row", horizontal: true, role: "none",
                    for (id, label) in SECTIONS {
                        Tab {
                            key: "{id}",
                            href: "#{id}",
                            selected: current() == id,
                            onclick: move |_| current.set(id.to_string()),
                            "{label}"
                        }
                    }
                }
            }

            main { class: "gallery-main",
                header { class: "gallery-intro",
                    h1 { class: "text-large-title text-emphasized", "videocall-theme" }
                    p { class: "gallery-lede",
                        "Every token and primitive of the design system, drawn from the crate's own "
                        "stylesheets. Values are read from the page at runtime, so they always match "
                        "tokens.css. Use the bar at the bottom to switch appearance and to simulate "
                        "accessibility settings."
                    }
                }
                sections::foundations::Colors {}
                sections::foundations::Type {}
                sections::foundations::Spacing {}
                sections::foundations::Radii {}
                sections::foundations::Elevation {}
                sections::foundations::Motion {}
                sections::materials::Materials {}
                sections::controls::Buttons {}
                sections::controls::Forms {}
                sections::content::Lists {}
                sections::overlays::Overlays {}
                sections::navigation::Navigation {}
                sections::navigation::Status {}
                sections::call::Call {}
            }
        }

        PreviewControls {}

        ToastRegion { role: "status", aria_live: "polite",
            for (id, text) in toasts {
                Toast { key: "{id}",
                    span { class: "grow", "{text}" }
                    IconButton {
                        label: "Dismiss",
                        size: IconButtonSize::Small,
                        onclick: move |_| gallery.dismiss_toast(id),
                        icons::Icon { paths: icons::CLOSE }
                    }
                }
            }
        }
    }
}

/// The floating bar: appearance, plus a menu of simulated OS settings.
#[component]
fn PreviewControls() -> Element {
    let gallery = use_context::<Gallery>();
    let mut appearance = gallery.appearance;
    let mut preview = gallery.preview;
    let mut menu_open = use_signal(|| false);
    let current = preview();

    let appearances = [
        (Appearance::Light, "Light"),
        (Appearance::Dark, "Dark"),
        (Appearance::System, "Auto"),
    ];

    rsx! {
        div { class: "gallery-controls",
            if menu_open() {
                Menu { id: "gallery-a11y-menu", aria_label: "Simulated settings",
                    MenuHeader { "Simulate a system setting" }
                    MenuItem {
                        checked: current.increase_contrast,
                        onclick: move |_| preview.with_mut(|p| p.increase_contrast = !p.increase_contrast),
                        span { class: "grow", "Increase Contrast" }
                        CheckMark { on: current.increase_contrast }
                    }
                    MenuItem {
                        checked: current.reduce_transparency,
                        onclick: move |_| preview.with_mut(|p| p.reduce_transparency = !p.reduce_transparency),
                        span { class: "grow", "Reduce Transparency" }
                        CheckMark { on: current.reduce_transparency }
                    }
                    MenuItem {
                        checked: current.reduce_motion,
                        onclick: move |_| preview.with_mut(|p| p.reduce_motion = !p.reduce_motion),
                        span { class: "grow", "Reduce Motion" }
                        CheckMark { on: current.reduce_motion }
                    }
                    MenuSeparator {}
                    MenuItem {
                        onclick: move |_| menu_open.set(false),
                        "Done"
                    }
                }
            }
            Bar { aria_label: "Gallery controls",
                Segmented { aria_label: "Appearance",
                    for (option, label) in appearances {
                        Segment {
                            key: "{label}",
                            selected: appearance() == option,
                            onclick: move |_| appearance.set(option),
                            "{label}"
                        }
                    }
                }
                IconButton {
                    label: "Simulated settings",
                    pressed: menu_open(),
                    aria_expanded: if menu_open() { "true" } else { "false" },
                    aria_controls: "gallery-a11y-menu",
                    onclick: move |_| menu_open.toggle(),
                    icons::Icon { paths: icons::ACCESSIBILITY }
                }
            }
        }
    }
}

/// A trailing check for a checkable menu item. Hidden from screen readers:
/// the item's `aria-checked` already says it.
#[component]
fn CheckMark(on: bool) -> Element {
    rsx! {
        span { class: "gallery-check", "aria-hidden": "true",
            if on {
                icons::Icon { paths: icons::CHECK }
            }
        }
    }
}
