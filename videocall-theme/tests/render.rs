// SPDX-License-Identifier: MIT OR Apache-2.0

//! Server-side renders of the primitives, run on the host with
//! `cargo test -p videocall-theme`. They check the part the class tests
//! cannot: that `class`, `id`, `data-testid` and `aria-*` given to a
//! component actually land on its element, which is what lets dioxus-ui
//! adopt these without breaking its test hooks.

use dioxus::prelude::*;
use videocall_theme::*;

fn html(element: Element) -> String {
    dioxus_ssr::render_element(element)
}

/// Renders a root component in a real `VirtualDom`. Needed when the markup
/// holds event handlers, which can only be created inside a runtime.
fn html_app(app: fn() -> Element) -> String {
    let mut dom = VirtualDom::new(app);
    dom.rebuild_in_place();
    dioxus_ssr::render(&dom)
}

#[test]
fn button_passes_class_id_testid_and_aria_through() {
    let out = html(rsx! {
        Button {
            variant: ButtonVariant::Prominent,
            size: ButtonSize::Small,
            class: "retry-btn",
            id: "retry",
            "data-testid": "retry-button",
            aria_label: "Retry now",
            "Retry"
        }
    });
    assert!(out.starts_with("<button"), "{out}");
    assert!(
        out.contains(r#"class="btn btn-prominent btn-sm retry-btn""#),
        "{out}"
    );
    assert!(out.contains(r#"id="retry""#), "{out}");
    assert!(out.contains(r#"data-testid="retry-button""#), "{out}");
    assert!(out.contains(r#"aria-label="Retry now""#), "{out}");
    assert!(out.contains(r#"type="button""#), "{out}");
    assert!(out.contains(">Retry</button>"), "{out}");
    assert!(
        !out.contains("aria-pressed"),
        "plain buttons are not toggles: {out}"
    );
    assert!(!out.contains("disabled"), "{out}");
}

#[test]
fn button_pressed_and_disabled() {
    let out = html(rsx! {
        Button { pressed: true, disabled: true, "Mute" }
    });
    assert!(out.contains(r#"aria-pressed="true""#), "{out}");
    assert!(out.contains("disabled"), "{out}");
}

#[test]
fn switch_matches_the_toggle_switch_it_replaces() {
    let out = html_app(|| {
        rsx! {
            Switch {
                enabled: true,
                on_toggle: |_| {},
                test_id: "noise-toggle",
                labelled_by: "noise-label",
            }
        }
    });
    assert!(out.contains(r#"class="switch""#), "{out}");
    assert!(out.contains(r#"role="switch""#), "{out}");
    assert!(out.contains(r#"aria-checked="true""#), "{out}");
    assert!(out.contains(r#"aria-labelledby="noise-label""#), "{out}");
    assert!(out.contains(r#"data-testid="noise-toggle""#), "{out}");
    assert!(
        !out.contains("style="),
        "no size override by default: {out}"
    );

    let sized = html_app(|| {
        rsx! {
            Switch { enabled: false, on_toggle: |_| {}, width: 60, height: 30 }
        }
    });
    assert!(sized.contains(r#"aria-checked="false""#), "{sized}");
    assert!(
        sized.contains("--switch-width: 60px; --switch-height: 30px;"),
        "{sized}"
    );
}

#[test]
fn control_button_keeps_name_and_disables() {
    let out = html(rsx! {
        ControlButton {
            label: "Share screen",
            state: ControlState::On,
            disabled: true,
            pressed: true,
            "data-testid": "share-button",
            Tooltip { "Stop Screen Share" }
        }
    });
    assert!(
        out.contains(r#"class="video-control-button active disabled""#),
        "{out}"
    );
    // Same as the app's control buttons: the real attribute, not aria-disabled.
    assert!(out.contains(r#" disabled=true"#), "{out}");
    assert!(!out.contains("aria-disabled"), "{out}");
    assert!(out.contains(r#"aria-label="Share screen""#), "{out}");
    assert!(out.contains(r#"aria-pressed="true""#), "{out}");
    assert!(out.contains(r#"data-testid="share-button""#), "{out}");
    assert!(!out.contains("device-warning"), "{out}");
    assert!(
        out.contains(r#"<span class="tooltip" role="tooltip">Stop Screen Share</span>"#),
        "{out}"
    );
}

#[test]
fn list_row_picks_its_element() {
    let plain = html(rsx! { ListRow { "a" } });
    assert!(plain.starts_with(r#"<div class="list-row""#), "{plain}");
    let link = html(rsx! { ListRow { href: "/settings", "a" } });
    assert!(
        link.starts_with(r#"<a class="list-row" href="/settings""#),
        "{link}"
    );
    let button = html_app(|| rsx! { ListRow { onclick: |_| {}, "a" } });
    assert!(button.starts_with("<button"), "{button}");
    assert!(button.contains(r#"class="list-row""#), "{button}");
}

#[test]
fn search_field_puts_attributes_on_the_input() {
    let out = html(rsx! {
        SearchField {
            label: "Search meetings",
            class: "meetings-search",
            placeholder: "Search",
            "data-testid": "meeting-search",
        }
    });
    assert!(
        out.starts_with(r#"<div class="search-field meetings-search">"#),
        "{out}"
    );
    let input = &out[out.find("<input").expect("an input")..];
    assert!(input.contains(r#"type="search""#), "{out}");
    assert!(input.contains(r#"aria-label="Search meetings""#), "{out}");
    assert!(input.contains(r#"placeholder="Search""#), "{out}");
    assert!(input.contains(r#"data-testid="meeting-search""#), "{out}");
}

#[test]
fn sheet_alert_and_menu_roles() {
    let sheet = html(rsx! {
        Sheet { wide: true, grabber: true, aria_labelledby: "t",
            SheetHeader { SheetTitle { id: "t", "Settings" } }
        }
    });
    assert!(sheet.contains(r#"class="sheet sheet-wide""#), "{sheet}");
    assert!(sheet.contains(r#"role="dialog""#), "{sheet}");
    assert!(sheet.contains(r#"aria-modal="true""#), "{sheet}");
    assert!(sheet.contains(r#"aria-labelledby="t""#), "{sheet}");
    assert!(sheet.contains(r#"class="sheet-grabber""#), "{sheet}");
    assert!(
        sheet.contains(r#"<h2 class="sheet-title" id="t">Settings</h2>"#),
        "{sheet}"
    );

    let alert = html(rsx! { Alert { AlertTitle { "Leave?" } } });
    assert!(alert.contains(r#"role="alertdialog""#), "{alert}");

    let item = html(rsx! { MenuItem { destructive: true, checked: false, "Sign out" } });
    assert!(
        item.contains(r#"class="menu-item menu-item-destructive""#),
        "{item}"
    );
    assert!(item.contains(r#"role="menuitemcheckbox""#), "{item}");
    assert!(item.contains(r#"aria-checked="false""#), "{item}");
}

#[test]
fn segment_and_tabs_mark_selection_the_way_the_sheet_reads_it() {
    let segment = html(rsx! { Segment { selected: true, "Grid" } });
    assert!(segment.contains(r#"aria-pressed="true""#), "{segment}");

    let tab = html(rsx! { Tab { selected: true, "General" } });
    assert!(tab.contains(r#"role="tab""#), "{tab}");
    assert!(tab.contains(r#"aria-selected="true""#), "{tab}");

    let link = html(
        rsx! { Tab { selected: true, href: "#colors", class: "settings-nav-button", "Colors" } },
    );
    assert!(
        link.contains(r#"class="tab active settings-nav-button""#),
        "{link}"
    );
    assert!(link.contains(r#"aria-current="page""#), "{link}");
}

#[test]
fn status_primitives() {
    let badge = html(rsx! { Badge { tone: BadgeTone::Count, "3" } });
    assert_eq!(badge, r#"<span class="badge badge-count">3</span>"#);

    let hidden_dot = html(rsx! { StatusDot { tone: StatusTone::Success } });
    assert!(hidden_dot.contains(r#"aria-hidden="true""#), "{hidden_dot}");
    let labelled_dot = html(rsx! { StatusDot { tone: StatusTone::Danger, label: "Live" } });
    assert!(labelled_dot.contains(r#"role="img""#), "{labelled_dot}");
    assert!(
        labelled_dot.contains(r#"aria-label="Live""#),
        "{labelled_dot}"
    );

    let spinner = html(rsx! { Spinner { large: true } });
    assert!(
        spinner.contains(r#"class="spinner spinner-lg""#),
        "{spinner}"
    );
    assert!(spinner.contains(r#"aria-label="Loading""#), "{spinner}");
}

#[test]
fn field_shows_error_instead_of_hint() {
    let out = html(rsx! {
        Field { label: "Name", label_for: "name", hint: "Shown to others", error: "Required",
            TextField { id: "name", invalid: true, value: "" }
        }
    });
    assert!(
        out.contains(r#"<label class="field-label" for="name">Name</label>"#),
        "{out}"
    );
    assert!(out.contains(r#"class="input""#), "{out}");
    assert!(out.contains(r#"aria-invalid="true""#), "{out}");
    assert!(out.contains(r#"class="field-error""#), "{out}");
    assert!(!out.contains("field-hint"), "{out}");
}

#[test]
fn toast_solid_and_tooltip_alignment() {
    let glass = html(rsx! { Toast { "Saved" } });
    assert!(glass.starts_with(r#"<div class="toast">"#), "{glass}");
    let solid = html(rsx! { Toast { solid: true, class: "peer-toast", "Dana joined" } });
    assert!(
        solid.starts_with(r#"<div class="toast toast-solid peer-toast">"#),
        "{solid}"
    );
    let start = html(rsx! { Tooltip { align: TooltipAlign::Start, "Mute" } });
    assert!(
        start.contains(r#"class="tooltip tooltip-start""#),
        "{start}"
    );
    let end = html(rsx! { Tooltip { align: TooltipAlign::End, open: true, "Leave" } });
    assert!(
        end.contains(r#"class="tooltip tooltip-end is-open""#),
        "{end}"
    );
}

#[test]
fn a_role_the_caller_passes_replaces_the_components_own() {
    // dioxus-ui's dialogs and menus carry no role today; `role: None` keeps
    // it that way, and drops `aria-modal` with it.
    let alert = html(rsx! { Alert { role: None::<&str>, class: "meeting-ended-alert", "x" } });
    assert_eq!(alert, r#"<div class="alert meeting-ended-alert">x</div>"#);
    let dialog = html(rsx! {
        Alert { role: "dialog", "aria-modal": "true", id: "baby-mode-unlock-dialog", "x" }
    });
    assert!(dialog.contains(r#"role="dialog""#), "{dialog}");
    assert!(!dialog.contains("alertdialog"), "{dialog}");
    assert_eq!(dialog.matches("aria-modal").count(), 1, "{dialog}");

    let menu = html(rsx! {
        Menu { role: None::<&str>, class: "auth-dropdown-menu",
            MenuItem { role: None::<&str>, destructive: true, "Sign out" }
        }
    });
    assert_eq!(
        menu,
        r#"<div class="menu auth-dropdown-menu"><button type="button" class="menu-item menu-item-destructive">Sign out</button></div>"#
    );

    let sheet = html(rsx! { Sheet { role: None::<&str>, "x" } });
    assert_eq!(sheet, r#"<div class="sheet">x</div>"#);
}

// Markup dioxus-ui relied on before it adopted these components, taken from
// the pre-migration source. These cover states a headless run of the app
// cannot reach (a missing device, a disabled share, the hang up button).

#[test]
fn control_button_reproduces_the_apps_unavailable_mic() {
    // Was: class "video-control-button active error", disabled, aria-pressed,
    // and a trailing `span.device-warning` with "!" (now also aria-hidden).
    let out = html_app(|| {
        rsx! {
            ControlButton {
                label: "Microphone",
                state: ControlState::On,
                pressed: true,
                unavailable: true,
                onclick: |_| {},
                Tooltip { align: TooltipAlign::Start, "Mute" }
            }
        }
    });
    assert!(
        out.contains(r#"class="video-control-button active error""#),
        "{out}"
    );
    assert!(!out.contains("disabled\""), "no .disabled class: {out}");
    assert!(out.contains(" disabled=true"), "{out}");
    assert!(out.contains(r#"aria-label="Microphone""#), "{out}");
    assert!(out.contains(r#"aria-pressed="true""#), "{out}");
    assert!(
        out.ends_with(
            r#"<span class="tooltip tooltip-start" role="tooltip">Mute</span><span class="device-warning" aria-hidden="true">!</span></button>"#
        ),
        "{out}"
    );
}

#[test]
fn control_button_without_label_is_named_by_its_tooltip() {
    // Hang up had no aria-label: e2e finds it by its `.tooltip` text.
    let out = html_app(|| {
        rsx! {
            ControlButton {
                state: ControlState::Danger,
                onclick: |_| {},
                Tooltip { align: TooltipAlign::End, "Hang Up" }
            }
        }
    });
    assert!(out.starts_with("<button"), "{out}");
    assert!(
        out.contains(r#"class="video-control-button danger""#),
        "{out}"
    );
    assert!(!out.contains("aria-label"), "{out}");
    assert!(!out.contains("aria-pressed"), "{out}");
    assert!(!out.contains("disabled"), "{out}");
    assert!(
        out.contains(r#"<span class="tooltip tooltip-end" role="tooltip">Hang Up</span>"#),
        "{out}"
    );
}

#[test]
fn icon_button_named_by_title() {
    let out = html(rsx! {
        IconButton { size: IconButtonSize::Small, class: "change-name-fab", title: "Change name", "x" }
    });
    assert!(
        out.contains(r#"class="icon-btn icon-btn-sm change-name-fab""#),
        "{out}"
    );
    assert!(out.contains(r#"title="Change name""#), "{out}");
    assert!(!out.contains("aria-label"), "{out}");
}

#[test]
fn meeting_ended_overlay_markup() {
    // Was: div.glass-backdrop.meeting-ended-overlay.scrim >
    //   div.alert.meeting-ended-alert (no role) > ... p.alert-message,
    //   div.alert-actions > button.btn.btn-prominent.meeting-ended-home-btn
    let out = html_app(|| {
        rsx! {
            Scrim { class: "glass-backdrop meeting-ended-overlay",
                Alert { role: None::<&str>, class: "meeting-ended-alert",
                    AlertMessage { class: "meeting-ended-message", "The host ended it." }
                    AlertActions {
                        Button {
                            variant: ButtonVariant::Prominent,
                            class: "meeting-ended-home-btn",
                            onclick: |_| {},
                            "Return to Home"
                        }
                    }
                }
            }
        }
    });
    assert!(
        out.starts_with(r#"<div class="scrim glass-backdrop meeting-ended-overlay""#),
        "{out}"
    );
    assert!(
        out.contains(r#"<div class="alert meeting-ended-alert">"#),
        "{out}"
    );
    assert!(!out.contains("role="), "{out}");
    assert!(!out.contains("aria-modal"), "{out}");
    assert!(
        out.contains(r#"<p class="alert-message meeting-ended-message">The host ended it.</p>"#),
        "{out}"
    );
    assert!(
        out.contains(r#"class="btn btn-prominent meeting-ended-home-btn""#),
        "{out}"
    );
}

#[test]
fn host_admit_panel_chrome() {
    // Was: div.host-controls-container.glass-regular >
    //   button.host-controls-toggle.list-row[type=button][aria-expanded] >
    //   span.badge.badge-count + span.list-row-label
    let out = html_app(|| {
        rsx! {
            Glass { class: "host-controls-container",
                ListRow { class: "host-controls-toggle", "aria-expanded": "false", onclick: |_| {},
                    Badge { tone: BadgeTone::Count, "2" }
                    ListRowLabel { "Waiting to join" }
                }
            }
        }
    });
    assert!(
        out.starts_with(r#"<div class="glass-regular host-controls-container">"#),
        "{out}"
    );
    assert!(out.contains(r#"type="button""#), "{out}");
    assert!(
        out.contains(r#"class="list-row host-controls-toggle""#),
        "{out}"
    );
    assert!(out.contains(r#"aria-expanded="false""#), "{out}");
    assert!(
        out.contains(r#"<span class="badge badge-count">2</span><span class="list-row-label">Waiting to join</span>"#),
        "{out}"
    );
}

#[test]
fn toast_and_dim_reproduce_the_call_screen() {
    let toast = html(rsx! {
        Toast { solid: true, class: "copy-toast", role: "alert", "aria-live": "assertive", "Link copied to clipboard" }
    });
    assert_eq!(
        toast,
        r#"<div class="toast toast-solid copy-toast" role="alert" aria-live="assertive">Link copied to clipboard</div>"#
    );
    let dim = html(rsx! { MediaDim { edge: DimEdge::Bottom, class: "call-controls-dim" } });
    assert_eq!(
        dim,
        r#"<div class="media-dim-bottom call-controls-dim"></div>"#
    );
}

#[test]
fn list_semantics_tags_and_landmarks() {
    let list = html(rsx! {
        ListGroup { list: true, class: "meetings-list",
            ListRow { list_item: true, class: "meeting-item", "a" }
        }
    });
    assert_eq!(
        list,
        r#"<ul class="list-group meetings-list"><li class="list-row meeting-item">a</li></ul>"#
    );
    let bar = html(rsx! { Bar { clear: true, nav: true, class: "call-controls", "x" } });
    assert_eq!(bar, r#"<nav class="bar bar-clear call-controls">x</nav>"#);
    let header = html(rsx! { Toolbar { header: true, class: "settings-toolbar", "x" } });
    assert_eq!(
        header,
        r#"<header class="toolbar settings-toolbar">x</header>"#
    );
}

#[test]
fn spinner_without_label_and_optional_invalid() {
    assert_eq!(
        html(rsx! { Spinner { large: true, label: None } }),
        r#"<span class="spinner spinner-lg"></span>"#
    );
    let ok = html(rsx! { TextField { invalid: false } });
    assert!(ok.contains(r#"aria-invalid="false""#), "{ok}");
    let unset = html(rsx! { TextField {} });
    assert!(!unset.contains("aria-invalid"), "{unset}");
}
