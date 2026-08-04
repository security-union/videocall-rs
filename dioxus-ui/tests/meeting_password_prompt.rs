// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Browser tests for the meeting-password prompt (issue 1613).
//
// These exist because the property they pin is not observable from a host
// `#[test]`: whether a screen reader is TOLD about a rejection depends on
// whether the `role="alert"` node is re-created in the DOM, and that is a fact
// about Dioxus's diffing, not about any value the pure helpers return.
//
// The bug they were written for: `.focus()` on an already-focused element fires
// no focus event (the spec skips the focusing steps), and the prompt's submit
// handler deliberately parks focus on the field before every submit — so the
// `aria-describedby` re-read never happens. Meanwhile re-setting identical
// `textContent` produces zero DOM mutations, so an unkeyed live region is
// silent from the second consecutive rejection onward. Rejection 1 announced;
// 2, 3, 4... announced nothing, while the field cleared itself under the user.
//
// The fix is the `key` on the alert node. A test that merely asserts the error
// is *present* passes on the broken code — so these assert the node is a
// DIFFERENT DOM node than the one before it.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

mod support;

use std::cell::RefCell;

use dioxus::prelude::*;
use dioxus_ui::components::meeting_password_prompt::{
    MeetingPasswordPrompt, PasswordPromptReason, PasswordPromptState,
};
use support::{cleanup, create_mount_point, render_into, wait_for_selector, yield_now};
use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;
use web_sys::{Event, EventInit, HtmlInputElement};

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const FIELD: &str = "#meeting-password";
const ALERT: &str = "#meeting-password-error";

thread_local! {
    /// Handle on the harness's state signal so a test can drive the prompt the
    /// way the joining page does — from outside the component.
    static PROMPT_STATE: RefCell<Option<Signal<PasswordPromptState>>> =
        const { RefCell::new(None) };
}

/// `render_into` takes a plain `fn() -> Element`, so the harness cannot capture
/// anything; it publishes its signal through the thread-local above instead.
fn harness() -> Element {
    let state = use_signal(|| PasswordPromptState::opened(PasswordPromptReason::Required));
    use_hook(move || PROMPT_STATE.with(|slot| *slot.borrow_mut() = Some(state)));
    rsx! {
        MeetingPasswordPrompt {
            state,
            cancel_label: "Cancel",
            on_submit: move |_value: String| {},
            on_cancel: move |_| {},
        }
    }
}

/// Apply a transition to the harness's state, as the page's join handler would.
fn drive(next: impl FnOnce(PasswordPromptState) -> PasswordPromptState) {
    let mut signal = PROMPT_STATE
        .with(|slot| *slot.borrow())
        .expect("harness must be mounted before driving it");
    let current = *signal.peek();
    signal.set(next(current));
}

/// Two frames: one for Dioxus to process the signal write, one for the renderer
/// to flush the resulting mutations.
async fn settle() {
    yield_now().await;
    yield_now().await;
}

/// Type into the password field the way a user does.
///
/// The field is a CONTROLLED input bound to the component's own signal, so
/// setting `.value` alone is overwritten on the next render — the value has to
/// arrive through Dioxus's delegated `oninput` handler, which needs a bubbling
/// event. (Learned the hard way: the direct-write version of this helper made
/// the throttle test fail against correct production code.)
async fn type_into_field(mount: &web_sys::Element, text: &str) {
    let field: HtmlInputElement = query(mount, FIELD).unchecked_into();
    field.set_value(text);
    let init = EventInit::new();
    init.set_bubbles(true);
    let event = Event::new_with_event_init_dict("input", &init).unwrap();
    field.dispatch_event(&event).unwrap();
    settle().await;
}

fn query(mount: &web_sys::Element, selector: &str) -> web_sys::Element {
    mount
        .query_selector(selector)
        .ok()
        .flatten()
        .unwrap_or_else(|| panic!("expected {selector} to be present"))
}

/// The regression this file exists for. Reverting the `key` on the alert node
/// makes the final assertion fail while every other assertion here still
/// passes — which is exactly how the bug shipped.
#[wasm_bindgen_test]
async fn a_second_identical_rejection_recreates_the_live_region() {
    let mount = create_mount_point();
    render_into(&mount, harness);
    assert!(
        wait_for_selector(&mount, FIELD, 2_000).await,
        "prompt did not mount"
    );

    drive(|s| s.rejected(PasswordPromptReason::Invalid));
    assert!(
        wait_for_selector(&mount, ALERT, 2_000).await,
        "first rejection did not render an alert"
    );
    let first = query(&mount, ALERT);
    let first_text = first.text_content();

    // Same reason again: identical copy, so nothing about the rendered text
    // changes. This is the case that used to go silent.
    drive(|s| s.rejected(PasswordPromptReason::Invalid));
    settle().await;
    let second = query(&mount, ALERT);

    assert_eq!(
        second.text_content(),
        first_text,
        "precondition: the two rejections must render identical text, otherwise \
         this test would pass for the wrong reason"
    );
    assert!(
        !first.is_same_node(Some(&second)),
        "the alert node was diffed in place on the second rejection — zero DOM \
         mutations, so the live region announces nothing and the user is told \
         only that the field emptied itself"
    );

    cleanup(&mount);
}

/// The same guarantee for the throttled path, plus the property that makes a
/// throttle different from a verdict: the server refused before hashing, so
/// what the user typed must survive.
#[wasm_bindgen_test]
async fn a_second_throttle_announces_and_keeps_the_typed_value() {
    let mount = create_mount_point();
    render_into(&mount, harness);
    assert!(
        wait_for_selector(&mount, FIELD, 2_000).await,
        "prompt did not mount"
    );

    type_into_field(&mount, "probably-correct").await;

    drive(|s| s.rejected(PasswordPromptReason::Throttled));
    assert!(
        wait_for_selector(&mount, ALERT, 2_000).await,
        "first throttle did not render an alert"
    );
    let first = query(&mount, ALERT);

    drive(|s| s.rejected(PasswordPromptReason::Throttled));
    settle().await;
    let second = query(&mount, ALERT);

    assert!(
        !first.is_same_node(Some(&second)),
        "a repeated throttle must re-create the live region, or the second one \
         is announced to nobody"
    );
    let field: HtmlInputElement = query(&mount, FIELD).unchecked_into();
    assert_eq!(
        field.value(),
        "probably-correct",
        "a throttle refused the attempt WITHOUT verifying it, so discarding the \
         value would make the user retype a password nobody read"
    );
    assert_eq!(
        query(&mount, FIELD)
            .get_attribute("aria-invalid")
            .as_deref(),
        Some("false"),
        "a throttled attempt was never judged, so the field must not be \
         announced as invalid"
    );

    cleanup(&mount);
}

/// The opening prompt has nothing to report, so there must be no live region at
/// all — an alert present from the start has nothing to announce and would make
/// the first real rejection a text change rather than an insertion.
#[wasm_bindgen_test]
async fn the_opening_prompt_renders_no_live_region() {
    let mount = create_mount_point();
    render_into(&mount, harness);
    assert!(
        wait_for_selector(&mount, FIELD, 2_000).await,
        "prompt did not mount"
    );
    settle().await;

    assert!(
        mount.query_selector(ALERT).ok().flatten().is_none(),
        "`Required` reports no rejection, so it must render no alert node"
    );

    cleanup(&mount);
}
