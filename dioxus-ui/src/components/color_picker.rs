/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

//! Custom HSV color picker used inside the "Choose Custom Color" modal.
//!
//! This replaces the native `<input type="color">` with a saturation/value
//! square, a vertical hue slider on the right, and a rounded preview-row card
//! containing a swatch, a hex-text input, and a copy-to-clipboard button.
//! The hex string and validity flag are owned by the parent (so the parent
//! drives the Add button), while the picker owns its internal HSV state and
//! keeps it in sync with the hex string in both directions.

use crate::util::color_math::{hsv_to_rgb, parse_hex, rgb_to_hex, rgb_to_hsv};
use dioxus::prelude::*;
use dioxus::web::WebEventExt;
use gloo_timers::future::TimeoutFuture;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Element as WebElement, PointerEvent as WebPointerEvent};

#[derive(Props, Clone, PartialEq)]
pub struct HsvColorPickerProps {
    /// Initial RGB used to seed the picker the first time it mounts.
    pub initial_rgb: (u8, u8, u8),
    /// Two-way bound hex text. The picker writes valid `#RRGGBB` strings here
    /// when the user drags the SV square or hue slider, and reads from it when
    /// the user types directly into the hex input.
    pub hex_input: Signal<String>,
    /// Set to `true` whenever the current hex string is malformed.
    pub input_error: Signal<bool>,
}

#[component]
pub fn HsvColorPicker(props: HsvColorPickerProps) -> Element {
    let HsvColorPickerProps {
        initial_rgb,
        mut hex_input,
        mut input_error,
    } = props;

    // Seed HSV from the initial RGB exactly once.
    let (init_h, init_s, init_v) = use_hook(move || {
        let (r, g, b) = initial_rgb;
        rgb_to_hsv(r, g, b)
    });

    let mut hue = use_signal(|| init_h);
    let mut sat = use_signal(|| init_s);
    let mut val = use_signal(|| init_v);
    // Preserve a usable hue while the user drags saturation/value to zero.
    let mut last_nonzero_hue = use_signal(|| if init_s > 0.0 { init_h } else { 0.0 });

    let mut dragging_sv = use_signal(|| false);
    let mut dragging_hue = use_signal(|| false);

    // Cache of the last hex string the picker itself wrote into `hex_input`,
    // so the reconcile effect can short-circuit without relying on a bit-exact
    // `hsv → rgb → hsv → rgb` round-trip. Defends against any future drift in
    // color_math turning into an infinite update loop.
    let mut last_pushed_hex = use_signal(String::new);

    // Reconcile the hex text into HSV state whenever it changes externally
    // (e.g. the user typed into the hex input).
    use_effect(move || {
        let text = hex_input.read().clone();
        if text == *last_pushed_hex.read() {
            return;
        }
        let Some((r, g, b)) = parse_hex(&text) else {
            return;
        };
        let (cr, cg, cb) = hsv_to_rgb(hue(), sat(), val());
        if (cr, cg, cb) == (r, g, b) {
            return;
        }
        let (h, s, v) = rgb_to_hsv(r, g, b);
        if sat() != s {
            sat.set(s);
        }
        if val() != v {
            val.set(v);
        }
        if s > 0.0 && v > 0.0 {
            if hue() != h {
                hue.set(h);
            }
            if last_nonzero_hue() != h {
                last_nonzero_hue.set(h);
            }
        }
    });

    // Push HSV -> hex string + clear error flag.
    let mut push_hex = move || {
        let (r, g, b) = hsv_to_rgb(hue(), sat(), val());
        let new_hex = rgb_to_hex(r, g, b);
        if *hex_input.read() != new_hex {
            last_pushed_hex.set(new_hex.clone());
            hex_input.set(new_hex);
        }
        if input_error() {
            input_error.set(false);
        }
    };

    // Helper: walk up from the raw event target to the listener element using
    // a CSS selector. Necessary because Dioxus uses event delegation, so the
    // underlying web event's `current_target` points at the delegation root
    // (document body), not the SV square or hue track. We use this only to
    // resolve the real element for pointer capture; cursor coordinates are
    // taken from `evt.element_coordinates()`, which Dioxus already reports
    // relative to the listener element.
    fn closest_element(evt: &WebPointerEvent, selector: &str) -> Option<WebElement> {
        let raw = evt.target()?.dyn_into::<WebElement>().ok()?;
        raw.closest(selector).ok().flatten()
    }

    // SV square element width/height (px). Re-read on each pointer event from
    // the resolved element via `getBoundingClientRect()` because the modal can
    // be resized by the viewport.
    fn element_size(el: &WebElement) -> (f64, f64) {
        let rect = el.get_bounding_client_rect();
        (rect.width(), rect.height())
    }

    // ── SV square pointer math ──────────────────────────────────────────────
    let mut update_sv_from_event = move |evt: &PointerEvent| {
        let web_evt = match evt.try_as_web_event() {
            Some(e) => e,
            None => return,
        };
        let Some(el) = closest_element(&web_evt, ".color-picker-sv-square") else {
            return;
        };
        let (w, h) = element_size(&el);
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let coords = evt.element_coordinates();
        let x = coords.x.clamp(0.0, w);
        let y = coords.y.clamp(0.0, h);
        let new_sat = (x / w) as f32;
        let new_val = 1.0 - (y / h) as f32;
        sat.set(new_sat);
        val.set(new_val);
        if new_sat > 0.0 && new_val > 0.0 {
            last_nonzero_hue.set(hue());
        }
        push_hex();
    };

    let on_sv_pointer_down = move |evt: PointerEvent| {
        if let Some(web_evt) = evt.try_as_web_event() {
            if let Some(el) = closest_element(&web_evt, ".color-picker-sv-square") {
                let _ = el.set_pointer_capture(web_evt.pointer_id());
            }
        }
        update_sv_from_event(&evt);
        dragging_sv.set(true);
    };
    let on_sv_pointer_move = move |evt: PointerEvent| {
        if !dragging_sv() {
            return;
        }
        update_sv_from_event(&evt);
    };
    let on_sv_pointer_end = move |evt: PointerEvent| {
        if dragging_sv() {
            dragging_sv.set(false);
            if let Some(web_evt) = evt.try_as_web_event() {
                if let Some(el) = closest_element(&web_evt, ".color-picker-sv-square") {
                    let _ = el.release_pointer_capture(web_evt.pointer_id());
                }
            }
        }
    };

    let on_sv_keydown = move |evt: KeyboardEvent| {
        let shift = evt.modifiers().shift();
        let step_s = if shift { 0.10_f32 } else { 0.01_f32 };
        let step_v = step_s;
        let mut handled = true;
        let key = evt.key();
        match key {
            Key::ArrowLeft => sat.set((sat() - step_s).clamp(0.0, 1.0)),
            Key::ArrowRight => sat.set((sat() + step_s).clamp(0.0, 1.0)),
            Key::ArrowDown => val.set((val() - step_v).clamp(0.0, 1.0)),
            Key::ArrowUp => val.set((val() + step_v).clamp(0.0, 1.0)),
            Key::Home => sat.set(0.0),
            Key::End => sat.set(1.0),
            _ => handled = false,
        }
        if handled {
            evt.prevent_default();
            evt.stop_propagation();
            if sat() > 0.0 && val() > 0.0 {
                last_nonzero_hue.set(hue());
            }
            push_hex();
        }
    };

    // ── Hue slider pointer math (vertical) ──────────────────────────────────
    let mut update_hue_from_event = move |evt: &PointerEvent| {
        let web_evt = match evt.try_as_web_event() {
            Some(e) => e,
            None => return,
        };
        let Some(el) = closest_element(&web_evt, ".color-picker-hue-track") else {
            return;
        };
        let (_, h) = element_size(&el);
        if h <= 0.0 {
            return;
        }
        let coords = evt.element_coordinates();
        let y = coords.y.clamp(0.0, h);
        let new_hue = ((y / h) as f32 * 360.0).clamp(0.0, 359.999);
        hue.set(new_hue);
        last_nonzero_hue.set(new_hue);
        push_hex();
    };

    let on_hue_pointer_down = move |evt: PointerEvent| {
        if let Some(web_evt) = evt.try_as_web_event() {
            if let Some(el) = closest_element(&web_evt, ".color-picker-hue-track") {
                let _ = el.set_pointer_capture(web_evt.pointer_id());
            }
        }
        update_hue_from_event(&evt);
        dragging_hue.set(true);
    };
    let on_hue_pointer_move = move |evt: PointerEvent| {
        if !dragging_hue() {
            return;
        }
        update_hue_from_event(&evt);
    };
    let on_hue_pointer_end = move |evt: PointerEvent| {
        if dragging_hue() {
            dragging_hue.set(false);
            if let Some(web_evt) = evt.try_as_web_event() {
                if let Some(el) = closest_element(&web_evt, ".color-picker-hue-track") {
                    let _ = el.release_pointer_capture(web_evt.pointer_id());
                }
            }
        }
    };

    let on_hue_keydown = move |evt: KeyboardEvent| {
        let shift = evt.modifiers().shift();
        let mut handled = true;
        let mut new_hue = hue();
        match evt.key() {
            // Vertical orientation: Down increases hue (further down the
            // gradient), Up decreases. Left/Right are intentionally ignored.
            Key::ArrowUp => new_hue -= if shift { 10.0 } else { 1.0 },
            Key::ArrowDown => new_hue += if shift { 10.0 } else { 1.0 },
            Key::Home => new_hue = 0.0,
            Key::End => new_hue = 359.0,
            Key::PageUp => new_hue -= 15.0,
            Key::PageDown => new_hue += 15.0,
            _ => handled = false,
        }
        if handled {
            evt.prevent_default();
            evt.stop_propagation();
            new_hue = ((new_hue % 360.0) + 360.0) % 360.0;
            hue.set(new_hue);
            last_nonzero_hue.set(new_hue);
            push_hex();
        }
    };

    // ── Hex input handlers ─────────────────────────────────────────────────
    let on_hex_input = move |evt: Event<FormData>| {
        let raw = evt.value();
        hex_input.set(raw.clone());
        // Validate but keep the raw text in the field; the use_effect above
        // handles re-syncing HSV when the value parses cleanly.
        input_error.set(parse_hex(&raw).is_none() && !raw.is_empty());
    };
    let on_hex_blur = move |_| {
        let mut text = hex_input.read().clone();
        let trimmed = text.trim().to_string();
        if trimmed.is_empty() {
            // Empty field is not an error; the Add button is already disabled.
            input_error.set(false);
            return;
        }
        if !trimmed.starts_with('#') {
            text = format!("#{trimmed}");
            hex_input.set(text.clone());
        }
        input_error.set(parse_hex(hex_input.read().as_str()).is_none());
    };

    // ── Derived display values ─────────────────────────────────────────────
    let display_hue = if sat() > 0.0 {
        hue()
    } else {
        last_nonzero_hue()
    };
    let (cr, cg, cb) = hsv_to_rgb(display_hue, sat(), val());
    let current_hex = rgb_to_hex(cr, cg, cb);
    let pure_hue_rgb = hsv_to_rgb(display_hue, 1.0, 1.0);
    let pure_hue_hex = rgb_to_hex(pure_hue_rgb.0, pure_hue_rgb.1, pure_hue_rgb.2);

    let sv_bg = format!(
        "background: linear-gradient(to top, #000, transparent), linear-gradient(to right, #fff, hsl({display_hue:.2}, 100%, 50%));" // @token-exempt: SV gradient pure black/white + dynamic hue
    );
    let sv_marker_style = format!(
        "left: {:.2}%; top: {:.2}%;",
        (sat() * 100.0).clamp(0.0, 100.0),
        ((1.0 - val()) * 100.0).clamp(0.0, 100.0)
    );
    let hue_thumb_style = format!(
        "top: {:.2}%; --hue-pure-color: {};",
        (display_hue / 360.0 * 100.0).clamp(0.0, 100.0),
        pure_hue_hex
    );
    let preview_style = format!("background-color: {current_hex};");

    let hue_int = display_hue.round() as i32;
    let sat_pct = (sat() * 100.0).round() as i32;
    let val_pct = (val() * 100.0).round() as i32;
    let live_text = format!("Saturation {sat_pct}%, brightness {val_pct}%, hue {hue_int}°");

    // Copy-to-clipboard state. Set to true when the user successfully copies
    // the hex value; auto-resets after ~1.2s via gloo_timers.
    let mut copied = use_signal(|| false);
    let on_copy = move |evt: Event<MouseData>| {
        evt.stop_propagation();
        let text = hex_input.read().clone();
        if let Some(window) = web_sys::window() {
            let clipboard = window.navigator().clipboard();
            let promise = clipboard.write_text(&text);
            spawn(async move {
                if JsFuture::from(promise).await.is_ok() {
                    copied.set(true);
                    TimeoutFuture::new(1200).await;
                    copied.set(false);
                }
            });
        }
    };

    rsx! {
        div { class: "color-picker",
            div { class: "color-picker-top-row",
                div {
                    class: "color-picker-sv-square",
                    style: "{sv_bg}",
                    role: "application",
                    tabindex: "0",
                    "aria-label": "Saturation and brightness, use arrow keys to adjust",
                    "aria-roledescription": "2D color area",
                    onpointerdown: on_sv_pointer_down,
                    onpointermove: on_sv_pointer_move,
                    onpointerup: on_sv_pointer_end,
                    onpointercancel: on_sv_pointer_end,
                    onkeydown: on_sv_keydown,
                    div {
                        class: "color-picker-sv-marker",
                        style: "{sv_marker_style}",
                    }
                }

                div { class: "color-picker-hue-wrap color-picker-hue-wrap--vertical",
                    div {
                        class: "color-picker-hue-track color-picker-hue-track--vertical",
                        role: "slider",
                        tabindex: "0",
                        "aria-label": "Hue",
                        "aria-orientation": "vertical",
                        "aria-valuemin": "0",
                        "aria-valuemax": "360",
                        "aria-valuenow": "{hue_int}",
                        "aria-valuetext": "{hue_int}°",
                        onpointerdown: on_hue_pointer_down,
                        onpointermove: on_hue_pointer_move,
                        onpointerup: on_hue_pointer_end,
                        onpointercancel: on_hue_pointer_end,
                        onkeydown: on_hue_keydown,
                        div {
                            class: "color-picker-hue-thumb",
                            style: "{hue_thumb_style}",
                        }
                    }
                }
            }

            div { class: "color-picker-preview-row",
                div {
                    class: "color-picker-preview-swatch",
                    style: "{preview_style}",
                    "aria-hidden": "true",
                }
                input {
                    id: "color-picker-hex-input",
                    class: if input_error() { "custom-color-input error" } else { "custom-color-input" },
                    r#type: "text",
                    placeholder: "#RRGGBB",
                    maxlength: "7",
                    spellcheck: "false",
                    autocomplete: "off",
                    "aria-label": "Hex color value",
                    "aria-invalid": if input_error() { "true" } else { "false" },
                    "aria-describedby": "color-picker-hex-error",
                    value: "{hex_input}",
                    oninput: on_hex_input,
                    onblur: on_hex_blur,
                }
                button {
                    r#type: "button",
                    class: "color-picker-copy-btn",
                    "aria-label": "Copy hex value",
                    title: if copied() { "Copied" } else { "Copy hex value" },
                    onclick: on_copy,
                    if copied() {
                        // Checkmark icon shown briefly after a successful copy.
                        svg {
                            xmlns: "http://www.w3.org/2000/svg",
                            width: "16",
                            height: "16",
                            view_box: "0 0 24 24",
                            fill: "none",
                            stroke: "currentColor",
                            stroke_width: "2.5",
                            stroke_linecap: "round",
                            stroke_linejoin: "round",
                            "aria-hidden": "true",
                            path { d: "M20 6L9 17l-5-5" }
                        }
                    } else {
                        // Two overlapping squares = standard copy glyph.
                        svg {
                            xmlns: "http://www.w3.org/2000/svg",
                            width: "16",
                            height: "16",
                            view_box: "0 0 24 24",
                            fill: "none",
                            stroke: "currentColor",
                            stroke_width: "2",
                            stroke_linecap: "round",
                            stroke_linejoin: "round",
                            "aria-hidden": "true",
                            rect { x: "9", y: "9", width: "13", height: "13", rx: "2", ry: "2" }
                            path { d: "M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1" }
                        }
                    }
                }
            }

            // Visually-hidden live region announces SV/hue changes for AT users.
            div {
                class: "visually-hidden",
                "aria-live": "polite",
                "{live_text}"
            }
        }
    }
}
