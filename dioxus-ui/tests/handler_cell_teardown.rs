// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// `Host` is not mountable here: it needs a live `VideoCallClientCtx` and
// constructs three real encoders. These cases exercise `use_handler_cell`
// directly; `handler_cell::tests` pins `host.rs` to that hook (#2282).

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

use std::cell::{Cell, RefCell};

use dioxus::core::NoOpMutations;
use dioxus::prelude::*;
use dioxus_ui::components::handler_cell::use_handler_cell;
use videocall_client::Callback as VcCallback;
use wasm_bindgen_test::*;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

thread_local! {
    /// Kept alive past unmount as a real encoder keeps its own.
    static PUBLISHED: RefCell<Option<VcCallback<String>>> = const { RefCell::new(None) };
    static DELIVERED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    static SHOW_CHILD: RefCell<Option<Signal<bool>>> = const { RefCell::new(None) };
    static CHILD_DROPPED: Cell<bool> = const { Cell::new(false) };
    static PARENT_HANDLER: RefCell<Option<EventHandler<String>>> = const { RefCell::new(None) };
}

/// Stands in for `Host`: takes an `EventHandler` prop owned by its parent's
/// scope and routes it through the production hook.
#[component]
fn EncoderOwner(on_settings: EventHandler<String>) -> Element {
    let cell = use_handler_cell::<String>();
    cell.set(on_settings);
    use_hook({
        let cell = cell.clone();
        move || PUBLISHED.with(|p| *p.borrow_mut() = Some(cell.callback()))
    });
    use_drop(|| CHILD_DROPPED.with(|c| c.set(true)));
    rsx! { div {} }
}

fn app() -> Element {
    rsx! {
        EncoderOwner {
            on_settings: move |settings: String| {
                DELIVERED.with(|d| d.borrow_mut().push(settings));
            },
        }
    }
}

fn mount_and_publish() -> (VirtualDom, VcCallback<String>) {
    PUBLISHED.with(|p| *p.borrow_mut() = None);
    DELIVERED.with(|d| d.borrow_mut().clear());

    let mut vdom = VirtualDom::new(app);
    vdom.rebuild_in_place();

    let published = PUBLISHED
        .with(|p| p.borrow().clone())
        .expect("subject should publish its encoder callback on mount");
    (vdom, published)
}

#[wasm_bindgen_test]
fn encoder_callback_reaches_handler_while_mounted() {
    let (vdom, published) = mount_and_publish();

    published.emit("bitrate=1200".to_string());

    assert_eq!(
        DELIVERED.with(|d| d.borrow().clone()),
        vec!["bitrate=1200".to_string()],
        "a live component must still receive encoder settings"
    );
    drop(vdom);
}

#[wasm_bindgen_test]
fn encoder_callback_after_unmount_is_swallowed_not_panicking() {
    let (vdom, published) = mount_and_publish();

    // Frees the generational box behind `on_settings`; reading it panicked (#2282).
    drop(vdom);

    published.emit("bitrate=1200".to_string());

    assert!(
        DELIVERED.with(|d| d.borrow().is_empty()),
        "a torn-down component must receive nothing"
    );
}

#[wasm_bindgen_test]
fn repeated_post_unmount_events_stay_inert() {
    let (vdom, published) = mount_and_publish();
    drop(vdom);

    published.emit("first".to_string());
    published.emit("second".to_string());
    published.emit("third".to_string());

    assert!(DELIVERED.with(|d| d.borrow().is_empty()));
}

/// Owns the handler in ITS scope and unmounts only the child, so the handler's
/// generational box stays valid across the child's teardown.
fn parent_outlives_child_app() -> Element {
    let show_child = use_signal(|| true);
    let handler = use_hook(|| {
        let handler = EventHandler::new(|settings: String| {
            DELIVERED.with(|d| d.borrow_mut().push(settings));
        });
        PARENT_HANDLER.with(|p| *p.borrow_mut() = Some(handler));
        SHOW_CHILD.with(|s| *s.borrow_mut() = Some(show_child));
        handler
    });

    if show_child() {
        rsx! { EncoderOwner { on_settings: handler } }
    } else {
        rsx! { div {} }
    }
}

#[wasm_bindgen_test]
fn child_unmount_clears_the_cell_and_leaves_the_parent_callable() {
    PUBLISHED.with(|p| *p.borrow_mut() = None);
    PARENT_HANDLER.with(|p| *p.borrow_mut() = None);
    SHOW_CHILD.with(|s| *s.borrow_mut() = None);
    DELIVERED.with(|d| d.borrow_mut().clear());
    CHILD_DROPPED.with(|c| c.set(false));

    let mut vdom = VirtualDom::new(parent_outlives_child_app);
    vdom.rebuild_in_place();

    let published = PUBLISHED
        .with(|p| p.borrow().clone())
        .expect("child should publish its encoder callback on mount");
    published.emit("while-mounted".to_string());
    assert_eq!(DELIVERED.with(|d| d.borrow().len()), 1);

    let mut show_child = SHOW_CHILD
        .with(|s| *s.borrow())
        .expect("parent should publish its toggle");
    vdom.in_runtime(|| show_child.set(false));
    vdom.render_immediate(&mut NoOpMutations);
    assert!(
        CHILD_DROPPED.with(|c| c.get()),
        "harness precondition: the child scope did not actually unmount"
    );

    published.emit("after-child-unmount".to_string());
    assert_eq!(
        DELIVERED.with(|d| d.borrow().len()),
        1,
        "the cell was not emptied on unmount: the parent handler is still alive, so the \
         event was delivered instead of dropped"
    );

    let parent = PARENT_HANDLER
        .with(|p| p.borrow().clone())
        .expect("parent handler should still be published");
    parent.call("direct-parent-call".to_string());
    assert_eq!(
        DELIVERED.with(|d| d.borrow().len()),
        2,
        "clearing the cell must drop only our clone, never release the parent's box"
    );

    drop(vdom);
}
