// SPDX-License-Identifier: MIT OR Apache-2.0

//! `.input`, `.select`, `.search-field` and `.field`.
//!
//! The controls forward global and element attributes, so `value`,
//! `placeholder`, `name`, `autocomplete`, `maxlength` and friends are set the
//! same way as on a plain `input`.

use dioxus::prelude::*;

use crate::classes::with_extra;

fn aria_invalid(invalid: Option<bool>) -> Option<&'static str> {
    invalid.map(|on| if on { "true" } else { "false" })
}

/// A single-line text input (`.input`).
///
/// `invalid` writes `aria-invalid` (`"true"` turns the border red). Leave it
/// out and the attribute is not rendered; pass `false` for a field that is
/// validated and currently fine, which renders `aria-invalid="false"`.
#[component]
pub fn TextField(
    #[props(default)] invalid: Option<bool>,
    #[props(default)] disabled: bool,
    #[props(default)] class: Option<String>,
    #[props(default)] oninput: Option<EventHandler<FormEvent>>,
    #[props(default)] onchange: Option<EventHandler<FormEvent>>,
    #[props(default)] onkeydown: Option<EventHandler<KeyboardEvent>>,
    #[props(extends = GlobalAttributes, extends = input)] attributes: Vec<Attribute>,
) -> Element {
    let class = with_extra("input", class.as_deref());
    rsx! {
        input {
            class,
            disabled,
            "aria-invalid": aria_invalid(invalid),
            oninput: move |evt| {
                if let Some(handler) = &oninput {
                    handler.call(evt);
                }
            },
            onchange: move |evt| {
                if let Some(handler) = &onchange {
                    handler.call(evt);
                }
            },
            onkeydown: move |evt| {
                if let Some(handler) = &onkeydown {
                    handler.call(evt);
                }
            },
            ..attributes,
        }
    }
}

/// A multi-line text input (`textarea.input`). `invalid` works as on
/// [`TextField`].
#[component]
pub fn TextArea(
    #[props(default)] invalid: Option<bool>,
    #[props(default)] disabled: bool,
    #[props(default)] class: Option<String>,
    #[props(default)] oninput: Option<EventHandler<FormEvent>>,
    #[props(default)] onchange: Option<EventHandler<FormEvent>>,
    #[props(extends = GlobalAttributes, extends = textarea)] attributes: Vec<Attribute>,
) -> Element {
    let class = with_extra("input", class.as_deref());
    rsx! {
        textarea {
            class,
            disabled,
            "aria-invalid": aria_invalid(invalid),
            oninput: move |evt| {
                if let Some(handler) = &oninput {
                    handler.call(evt);
                }
            },
            onchange: move |evt| {
                if let Some(handler) = &onchange {
                    handler.call(evt);
                }
            },
            ..attributes,
        }
    }
}

/// A native select (`.select`). Put `option` elements in the children.
/// `invalid` works as on [`TextField`].
#[component]
pub fn Select(
    #[props(default)] invalid: Option<bool>,
    #[props(default)] disabled: bool,
    #[props(default)] class: Option<String>,
    #[props(default)] onchange: Option<EventHandler<FormEvent>>,
    #[props(extends = GlobalAttributes, extends = select)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let class = with_extra("select", class.as_deref());
    rsx! {
        select {
            class,
            disabled,
            "aria-invalid": aria_invalid(invalid),
            onchange: move |evt| {
                if let Some(handler) = &onchange {
                    handler.call(evt);
                }
            },
            ..attributes,
            {children}
        }
    }
}

/// A capsule search box (`.search-field`): a magnifier and an
/// `input type="search"`.
///
/// `class` goes on the wrapper. Every other attribute (`id`, `data-testid`,
/// `value`, `placeholder`, ...) goes on the input, because that is what a
/// test types into. `label` is the input's `aria-label`.
#[component]
pub fn SearchField(
    /// Accessible name of the input.
    label: String,
    #[props(default)] class: Option<String>,
    #[props(default)] oninput: Option<EventHandler<FormEvent>>,
    #[props(default)] onkeydown: Option<EventHandler<KeyboardEvent>>,
    #[props(extends = GlobalAttributes, extends = input)] attributes: Vec<Attribute>,
) -> Element {
    let class = with_extra("search-field", class.as_deref());
    rsx! {
        div { class,
            svg {
                view_box: "0 0 24 24",
                fill: "none",
                stroke: "currentColor",
                stroke_width: "2",
                stroke_linecap: "round",
                "aria-hidden": "true",
                circle { cx: "11", cy: "11", r: "7" }
                path { d: "M20 20l-3.5-3.5" }
            }
            input {
                r#type: "search",
                "aria-label": label,
                oninput: move |evt| {
                    if let Some(handler) = &oninput {
                        handler.call(evt);
                    }
                },
                onkeydown: move |evt| {
                    if let Some(handler) = &onkeydown {
                        handler.call(evt);
                    }
                },
                ..attributes,
            }
        }
    }
}

/// Label, control and hint stacked (`.field`).
///
/// `label_for` is the control's id, so clicking the label focuses it. An
/// `error` replaces the hint and is drawn red. Wire `aria-describedby` on the
/// control yourself if it should read the hint.
#[component]
pub fn Field(
    #[props(default)] label: Option<String>,
    #[props(default)] label_for: Option<String>,
    #[props(default)] hint: Option<String>,
    #[props(default)] error: Option<String>,
    #[props(default)] class: Option<String>,
    #[props(extends = GlobalAttributes)] attributes: Vec<Attribute>,
    children: Element,
) -> Element {
    let class = with_extra("field", class.as_deref());
    rsx! {
        div { class, ..attributes,
            if let Some(label) = label {
                label { class: "field-label", r#for: label_for, "{label}" }
            }
            {children}
            if let Some(error) = error {
                p { class: "field-error", "{error}" }
            } else if let Some(hint) = hint {
                p { class: "field-hint", "{hint}" }
            }
        }
    }
}
