/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 */

//! "About" modal — surfaces client + server build information on the homepage.
//!
//! Renders a glass-backdrop overlay with a card listing the running
//! `videocall-ui` build (compiled-in via `env!("CARGO_PKG_VERSION")`,
//! `GIT_SHA`, `BUILD_TIMESTAMP`) and the aggregated server-side build
//! info returned by `GET /api/v1/versions` (one row per registered
//! service: meeting-api, websocket, webtransport, ...).
//!
//! Lifecycle: the server fetch fires only when the modal opens, so
//! visitors who never tap "About" do not pay for the request.

use crate::components::changelog::WhatsNew;
use crate::components::meeting_footer::trap_tab_in_dialog;
use dioxus::prelude::*;
use serde::Deserialize;
use wasm_bindgen::JsCast;

/// The home-page control that opens About; focus returns to it on close.
pub(crate) const ABOUT_TRIGGER_ID: &str = "about-footer-link";
const ABOUT_DIALOG_ID: &str = "about-modal-dialog";

fn close_about(mut open: Signal<bool>) {
    open.set(false);
    if let Some(trigger) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(ABOUT_TRIGGER_ID))
        .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok())
    {
        let _ = trigger.focus();
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct BuildInfo {
    #[serde(default)]
    service: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    git_sha: String,
    #[serde(default)]
    git_branch: String,
    #[serde(default)]
    build_timestamp: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ServerVersionsResponse {
    #[serde(default)]
    components: Vec<BuildInfo>,
}

#[derive(Clone, PartialEq, Eq)]
enum FetchState {
    Loading,
    Ready(Vec<BuildInfo>),
    Error(String),
}

fn dash_if_empty(value: &str) -> &str {
    if value.is_empty() {
        "-"
    } else {
        value
    }
}

#[component]
pub fn AboutModal(mut open: Signal<bool>) -> Element {
    let mut state = use_signal(|| FetchState::Loading);

    use_effect(move || {
        if !open() {
            return;
        }
        state.set(FetchState::Loading);
        spawn(async move {
            let base_url = match crate::constants::meeting_api_base_url() {
                Ok(url) => url,
                Err(e) => {
                    state.set(FetchState::Error(format!("Config error: {e}")));
                    return;
                }
            };
            let url = format!("{base_url}/api/v1/versions");
            let resp = match reqwest::get(&url).await {
                Ok(r) => r,
                Err(e) => {
                    state.set(FetchState::Error(format!("Network error: {e}")));
                    return;
                }
            };
            if !resp.status().is_success() {
                state.set(FetchState::Error(format!(
                    "Server returned HTTP {}",
                    resp.status().as_u16()
                )));
                return;
            }
            match resp.json::<ServerVersionsResponse>().await {
                Ok(body) => state.set(FetchState::Ready(body.components)),
                Err(e) => state.set(FetchState::Error(format!("Invalid response: {e}"))),
            }
        });
    });

    if !open() {
        return rsx! {};
    }

    // Issue #1480: github info (commit + branch) is gated; version + built are not.
    let show_git = crate::constants::show_build_git_info();
    let client_version = env!("CARGO_PKG_VERSION");
    let client_sha = crate::constants::short_sha(env!("GIT_SHA"));
    let client_branch = env!("GIT_BRANCH");
    let client_ts = env!("BUILD_TIMESTAMP");
    // Issue #1789: render the Built value as date + full time (to the second) +
    // short zone label, converted from UTC into the viewer's local timezone
    // (matches the diagnostics build-info table). Falls back to the raw ts only if
    // `build_datetime_local` returns None (sentinel/empty).
    let client_built =
        crate::constants::build_datetime_local(client_ts).unwrap_or_else(|| client_ts.to_string());

    let server_section = match state() {
        FetchState::Loading => rsx! {
            div { class: "about-modal-status", "Loading server versions..." }
        },
        FetchState::Error(msg) => rsx! {
            div { class: "about-modal-status about-modal-status--error",
                "Couldn't reach the server: {msg}"
            }
        },
        FetchState::Ready(components) if components.is_empty() => rsx! {
            div { class: "about-modal-status", "No server components reported." }
        },
        FetchState::Ready(components) => {
            // Issue #1480: drop the Commit column entirely in production (github
            // info hidden) so the server table is Service/Version/Built (3 cols);
            // the --server-nogit modifier collapses the grid to match the spans.
            let row_class = if show_git {
                "about-modal-row"
            } else {
                "about-modal-row about-modal-row--server-nogit"
            };
            let header_class = if show_git {
                "about-modal-row about-modal-row--header"
            } else {
                "about-modal-row about-modal-row--header about-modal-row--server-nogit"
            };
            rsx! {
                div { class: "about-modal-table",
                    div { class: "{header_class}",
                        span { class: "about-modal-label", "Service" }
                        span { class: "about-modal-value", "Version" }
                        if show_git {
                            span { class: "about-modal-value", "Commit" }
                        }
                        span { class: "about-modal-value", "Built" }
                    }
                    for comp in components.iter() {
                        div { class: "{row_class}",
                            span { class: "about-modal-value about-modal-value--strong",
                                "{comp.service}"
                            }
                            span { class: "about-modal-value about-modal-value--mono",
                                "{dash_if_empty(&comp.version)}"
                            }
                            if show_git {
                                span { class: "about-modal-value about-modal-value--mono",
                                    "{crate::constants::short_sha(&comp.git_sha)}"
                                }
                            }
                            // Issue 1789: the Built value is now a proportional
                            // locale string (e.g. "Jun 19, 2026, 6:48:11 AM PDT"),
                            // so it drops the --mono class (Version/Commit keep it);
                            // matches the diagnostics build-info rendering.
                            span { class: "about-modal-value",
                                "{crate::constants::build_datetime_local(&comp.build_timestamp).unwrap_or_else(|| dash_if_empty(&comp.build_timestamp).to_string())}"
                            }
                        }
                    }
                }
            }
        }
    };

    rsx! {
        div {
            class: "glass-backdrop",
            "data-testid": "about-modal",
            // Click-outside dismiss.  The inner `.card-apple` stops
            // propagation so this fires only for backdrop clicks.
            onclick: move |_| close_about(open),

            div {
                id: ABOUT_DIALOG_ID,
                class: "card-apple about-modal-card",
                role: "dialog",
                "aria-modal": "true",
                "aria-labelledby": "about-modal-heading",
                tabindex: "0",
                "data-testid": "about-modal-dialog",
                onclick: move |e| e.stop_propagation(),
                onkeydown: move |e: Event<KeyboardData>| match e.key() {
                    Key::Escape => close_about(open),
                    Key::Tab if trap_tab_in_dialog(ABOUT_DIALOG_ID, e.modifiers().shift()) => {
                        e.prevent_default();
                    }
                    _ => {}
                },
                // Autofocus the dialog when it first mounts so keyboard
                // users can press Escape (or Tab) immediately without
                // clicking inside first.  Mirrors `search_modal.rs` and
                // `device_settings_modal.rs` accessibility patterns.
                onmounted: move |element| {
                    let element = element.data();
                    spawn(async move {
                        let _ = element.set_focus(true).await;
                    });
                },

                div { class: "about-modal-header",
                    h3 {
                        id: "about-modal-heading",
                        class: "about-modal-title",
                        "About"
                    }
                    button {
                        r#type: "button",
                        class: "btn-apple btn-secondary btn-sm about-modal-close",
                        "aria-label": "Close About dialog",
                        "data-testid": "about-modal-close",
                        onclick: move |_| close_about(open),
                        "Close"
                    }
                }

                section {
                    class: "about-modal-section",
                    "aria-labelledby": "about-client-heading",

                    h4 {
                        id: "about-client-heading",
                        class: "about-modal-section-title",
                        "Client"
                    }
                    div { class: "about-modal-table",
                        div { class: "about-modal-row",
                            span { class: "about-modal-label", "Component" }
                            span { class: "about-modal-value about-modal-value--strong",
                                "videocall-ui"
                            }
                        }
                        div { class: "about-modal-row",
                            span { class: "about-modal-label", "Version" }
                            span { class: "about-modal-value about-modal-value--strong",
                                "v{client_version}"
                            }
                        }
                        if show_git {
                            div { class: "about-modal-row",
                                span { class: "about-modal-label", "Commit" }
                                span { class: "about-modal-value about-modal-value--mono",
                                    "{client_sha}"
                                }
                            }
                            div { class: "about-modal-row",
                                span { class: "about-modal-label", "Branch" }
                                span { class: "about-modal-value about-modal-value--mono",
                                    "{client_branch}"
                                }
                            }
                        }
                        div { class: "about-modal-row",
                            span { class: "about-modal-label", "Built" }
                            // Issue 1789: proportional locale Built value → drop
                            // --mono (matches the server rows + diagnostics).
                            span { class: "about-modal-value",
                                "{client_built}"
                            }
                        }
                    }
                    WhatsNew { id_prefix: "about" }
                }

                section {
                    class: "about-modal-section",
                    "aria-labelledby": "about-server-heading",

                    h4 {
                        id: "about-server-heading",
                        class: "about-modal-section-title",
                        "Server"
                    }
                    {server_section}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::dash_if_empty;

    #[test]
    fn dash_if_empty_returns_dash_for_empty() {
        assert_eq!(dash_if_empty(""), "-");
    }

    #[test]
    fn dash_if_empty_passes_through_non_empty() {
        assert_eq!(dash_if_empty("1.2.3"), "1.2.3");
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod dom_tests {
    use super::*;
    use gloo_timers::future::TimeoutFuture;
    use wasm_bindgen_test::wasm_bindgen_test;

    #[allow(non_snake_case)]
    fn OpenAbout() -> Element {
        let open = use_signal(|| true);
        rsx! { AboutModal { open } }
    }

    async fn wait_for(mount: &web_sys::Element, selector: &str) -> Option<web_sys::Element> {
        for _ in 0..100 {
            if let Some(el) = mount.query_selector(selector).unwrap() {
                return Some(el);
            }
            TimeoutFuture::new(20).await;
        }
        None
    }

    #[wasm_bindgen_test]
    async fn the_client_section_expands_whats_new_inline() {
        let doc = gloo_utils::document();
        let mount = doc.create_element("div").unwrap();
        doc.body().unwrap().append_child(&mount).unwrap();
        dioxus::web::launch::launch_virtual_dom(
            VirtualDom::new(OpenAbout),
            dioxus::web::Config::new().rootelement(mount.clone()),
        );

        let toggle = wait_for(
            &mount,
            "section[aria-labelledby='about-client-heading'] [data-testid='changelog-toggle']",
        )
        .await
        .expect("What's new sits in the Client section");
        toggle.unchecked_ref::<web_sys::HtmlElement>().click();

        let panel = wait_for(
            &mount,
            "[data-testid='about-modal-dialog'] [data-testid='changelog-panel']",
        )
        .await
        .expect("the log opens inside the About dialog");
        assert_eq!(panel.id(), "about-changelog-panel");
        assert_eq!(toggle.get_attribute("aria-controls"), Some(panel.id()));
        assert_eq!(
            toggle.get_attribute("aria-expanded").as_deref(),
            Some("true")
        );

        js_sys::Function::new_with_args(
            "el",
            "el.dispatchEvent(new KeyboardEvent('keydown', \
             { key: 'Escape', bubbles: true, cancelable: true }));",
        )
        .call1(&wasm_bindgen::JsValue::NULL, &toggle)
        .unwrap();
        let mut closed = false;
        for _ in 0..100 {
            closed = mount
                .query_selector("[data-testid='about-modal']")
                .unwrap()
                .is_none();
            if closed {
                break;
            }
            TimeoutFuture::new(20).await;
        }
        assert!(closed, "Escape still closes About with the log expanded");

        mount.remove();
    }

    fn rect(el: &web_sys::Element) -> web_sys::DomRect {
        el.get_bounding_client_rect()
    }

    #[wasm_bindgen_test]
    async fn expanding_in_a_short_about_card_scrolls_the_log_into_view() {
        crate::components::changelog::reset_changelog_cache_for_test();
        let doc = gloo_utils::document();
        let style = doc.create_element("style").unwrap();
        style.set_text_content(Some(&format!(
            "{}{}.about-modal-card.card-apple {{ max-height: 150px; }}",
            include_str!("../../static/style.css"),
            include_str!("../../static/global.css"),
        )));
        doc.head().unwrap().append_child(&style).unwrap();
        js_sys::eval(
            r#"window.__original_fetch = window.__original_fetch || window.fetch;
            window.fetch = function (input) {
              var url = typeof input === 'string' ? input : input.url;
              if (!url.endsWith('/assets/changelog.json')) { return window.__original_fetch(input); }
              var build = function (d) { return { built: '2026-09-' + d + 'T12:00:00Z', version: '1.1.' + d,
                changes: ['First change ' + d, 'Second change ' + d, 'Third change ' + d] }; };
              var resp = new Response(JSON.stringify({ builds: [build(29), build(28), build(27)] }),
                { status: 200, headers: { 'Content-Type': 'application/json' } });
              Object.defineProperty(resp, 'url', { value: url });
              return Promise.resolve(resp);
            };"#,
        )
        .unwrap();
        let mount = doc.create_element("div").unwrap();
        doc.body().unwrap().append_child(&mount).unwrap();
        dioxus::web::launch::launch_virtual_dom(
            VirtualDom::new(OpenAbout),
            dioxus::web::Config::new().rootelement(mount.clone()),
        );

        let card = wait_for(&mount, "[data-testid='about-modal-dialog']")
            .await
            .unwrap();
        let toggle = wait_for(&mount, "[data-testid='changelog-toggle']")
            .await
            .unwrap();
        assert!(
            rect(&toggle).top() >= rect(&card).bottom(),
            "premise: What's new starts below the visible part of the card"
        );

        toggle.unchecked_ref::<web_sys::HtmlElement>().click();
        wait_for(&mount, "[data-testid='changelog-build']")
            .await
            .expect("the log loads");
        let mut in_view = false;
        for _ in 0..100 {
            let first_build = mount
                .query_selector("[data-testid='changelog-build']")
                .unwrap()
                .unwrap();
            in_view = rect(&toggle).top() >= rect(&card).top() - 1.0
                && rect(&toggle).bottom() <= rect(&card).bottom()
                && rect(&first_build).top() < rect(&card).bottom();
            if in_view {
                break;
            }
            TimeoutFuture::new(20).await;
        }
        let scroll_top = card.scroll_top();
        js_sys::eval(
            "if (window.__original_fetch) { window.fetch = window.__original_fetch; delete window.__original_fetch; }",
        )
        .unwrap();
        style.remove();
        mount.remove();
        assert!(
            in_view,
            "the toggle and the first build must scroll into the card (scrollTop {scroll_top})"
        );
    }

    #[allow(non_snake_case)]
    fn AboutWithTrigger() -> Element {
        let mut open = use_signal(|| true);
        rsx! {
            button { id: ABOUT_TRIGGER_ID, onclick: move |_| open.set(true), "About" }
            AboutModal { open }
        }
    }

    fn mount_about_with_trigger() -> web_sys::Element {
        let doc = gloo_utils::document();
        let mount = doc.create_element("div").unwrap();
        doc.body().unwrap().append_child(&mount).unwrap();
        dioxus::web::launch::launch_virtual_dom(
            VirtualDom::new(AboutWithTrigger),
            dioxus::web::Config::new().rootelement(mount.clone()),
        );
        mount
    }

    fn keydown(target: &web_sys::Element, key: &str, shift: bool) {
        js_sys::Function::new_with_args(
            "el, key, shift",
            "el.dispatchEvent(new KeyboardEvent('keydown', \
             { key: key, shiftKey: shift, bubbles: true, cancelable: true }));",
        )
        .call3(
            &wasm_bindgen::JsValue::NULL,
            target,
            &key.into(),
            &shift.into(),
        )
        .unwrap();
    }

    fn click(el: &web_sys::Element) {
        el.unchecked_ref::<web_sys::HtmlElement>().click();
    }

    fn active_id() -> String {
        gloo_utils::document()
            .active_element()
            .map(|el| el.id())
            .unwrap_or_default()
    }

    async fn wait_until(mut done: impl FnMut() -> bool) -> bool {
        for _ in 0..100 {
            if done() {
                return true;
            }
            TimeoutFuture::new(20).await;
        }
        done()
    }

    #[wasm_bindgen_test]
    async fn every_way_of_closing_about_returns_focus_to_its_trigger() {
        let mount = mount_about_with_trigger();
        let closers: [(&str, fn(&web_sys::Element)); 3] = [
            ("Escape", |card| keydown(card, "Escape", false)),
            ("Close", |card| {
                click(
                    &card
                        .query_selector("[data-testid='about-modal-close']")
                        .unwrap()
                        .unwrap(),
                )
            }),
            ("backdrop", |card| click(&card.parent_element().unwrap())),
        ];
        for (how, close) in closers {
            let card = wait_for(&mount, "[data-testid='about-modal-dialog']")
                .await
                .unwrap_or_else(|| panic!("About is open before {how}"));
            close(&card);
            let closed = wait_until(|| {
                mount
                    .query_selector("[data-testid='about-modal']")
                    .unwrap()
                    .is_none()
            })
            .await;
            let focused = active_id();
            if !(closed && focused == ABOUT_TRIGGER_ID) {
                mount.remove();
                panic!("{how}: closed {closed}, focus on {focused:?}");
            }
            click(
                &gloo_utils::document()
                    .get_element_by_id(ABOUT_TRIGGER_ID)
                    .unwrap(),
            );
        }
        mount.remove();
    }

    #[wasm_bindgen_test]
    async fn tab_wraps_inside_about() {
        let mount = mount_about_with_trigger();
        let card = wait_for(&mount, "[data-testid='about-modal-dialog']")
            .await
            .unwrap();
        let toggle = wait_for(&mount, "[data-testid='changelog-toggle']")
            .await
            .unwrap();
        let close = card
            .query_selector("[data-testid='about-modal-close']")
            .unwrap()
            .unwrap();
        let is_active = |el: &web_sys::Element| {
            gloo_utils::document()
                .active_element()
                .is_some_and(|active| active.is_same_node(Some(el)))
        };

        toggle
            .unchecked_ref::<web_sys::HtmlElement>()
            .focus()
            .unwrap();
        keydown(&toggle, "Tab", false);
        let tab_wraps = is_active(&close);
        keydown(&close, "Tab", true);
        let shift_tab_wraps = is_active(&toggle);
        card.unchecked_ref::<web_sys::HtmlElement>()
            .focus()
            .unwrap();
        keydown(&card, "Tab", false);
        let card_tab_enters = is_active(&close);
        mount.remove();

        assert!(tab_wraps, "Tab on the last button wraps to Close");
        assert!(
            shift_tab_wraps,
            "Shift+Tab on Close wraps to the last button"
        );
        assert!(card_tab_enters, "Tab on the card moves to the first button");
    }
}
