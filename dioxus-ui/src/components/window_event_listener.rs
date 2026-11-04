// SPDX-License-Identifier: MIT OR Apache-2.0

use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

/// RAII guard for an event listener on `window` or `document`: the handler is registered on
/// construction and **removed on `Drop`**.
///
/// Generalised from the resize-only guard of issue 2053 (issue 2100): the event
/// name is now a field, so the SAME guard covers the dock auto-hide
/// `mousemove` / `touchstart` listeners and the permission-recheck `focus`
/// listener, all of which had the identical `.forget()`-plus-signal-write
/// defect.
///
/// Store one in a `use_hook` (wrapped in `Rc`, since `use_hook` requires
/// `Clone` and hands back a clone each render) to bind the listener's lifetime
/// to the component INSTANCE: Dioxus drops a scope's hook values when the scope
/// is torn down, which is the very mechanism `use_drop` is built on
/// (`dioxus_core::use_drop` stores an `Rc<LifeCycle>` in a `use_hook` and runs
/// its callback from that value's `Drop`). So a `Drop`-implementing hook value
/// is cleaned up on unmount just as reliably as a `use_drop` closure.
///
/// This type exists because `Closure::forget()` keeps a callback alive for the
/// lifetime of the PAGE, not the component — so a SINGLE unmount is already a
/// bug; no remount is needed. The reachable trigger is an in-page router
/// unmount: `MeetingPage` is a client-side route
/// (`#[route("/meeting/:id", MeetingPage)]` in `routing.rs`), so navigating away
/// from it — browser Back being the easy one — unmounts `AttendantsComponent`
/// while the wasm instance keeps running. A forgotten closure stays registered
/// and still fires on the next resize, writing a signal whose scope is gone. In
/// Dioxus 0.7 `Signal::write`/`with_mut` is `try_write().unwrap()`
/// (dioxus-signals `write.rs:271`), so that write panics the client
/// (issue 2053).
///
/// NOT the trigger, despite the issue body saying so: the transport reconnect
/// (#1311) does not unmount this component. `MeetingPage`'s `on_connection_lost`
/// only `log::warn!`s and never touches `meeting_status` (`pages/meeting.rs`),
/// and the host-refresh path re-renders IN PLACE deliberately without a `key`
/// (`context.rs`, `pages/meeting.rs`). Nor is it an `Admitted` round trip: once
/// `Admitted` is reached the only later setter re-sets `Admitted`, and both
/// leave paths hard-navigate via `location().set_href("/")`
/// (`meeting_ended_overlay.rs`, and the hangup handler in `attendants.rs`), which resets the
/// page instead of remounting within it.
pub(crate) struct WindowEventListener {
    target: Option<web_sys::EventTarget>,
    /// The event this guard registered for, kept so `Drop` deregisters the
    /// SAME event it registered — passing a different name to
    /// `removeEventListener` is a silent no-op, i.e. the leak all over again.
    event: &'static str,
    capture: bool,
    /// `Some` while registered; taken by `Drop` after the listener is removed.
    /// Dropping the `Closure` frees the wasm callback slot — the leak that
    /// `Closure::forget()` made permanent.
    cb: Option<Closure<dyn FnMut(web_sys::Event)>>,
}

impl WindowEventListener {
    /// Registers `handler` for `window`'s `event` and returns the guard that
    /// owns it. The handler fires until the guard is dropped.
    pub(crate) fn new(event: &'static str, mut handler: impl FnMut() + 'static) -> Self {
        Self::with_event(event, false, move |_| handler())
    }

    pub(crate) fn with_event(
        event: &'static str,
        capture: bool,
        handler: impl FnMut(web_sys::Event) + 'static,
    ) -> Self {
        Self::on_target(web_sys::window().map(Into::into), event, capture, handler)
    }

    pub(crate) fn on_document(
        event: &'static str,
        handler: impl FnMut(web_sys::Event) + 'static,
    ) -> Self {
        let document = web_sys::window().and_then(|w| w.document());
        Self::on_target(document.map(Into::into), event, true, handler)
    }

    fn on_target(
        target: Option<web_sys::EventTarget>,
        event: &'static str,
        capture: bool,
        handler: impl FnMut(web_sys::Event) + 'static,
    ) -> Self {
        let cb = Closure::<dyn FnMut(web_sys::Event)>::new(handler);
        if let Some(target) = &target {
            let _ = target.add_event_listener_with_callback_and_bool(
                event,
                cb.as_ref().unchecked_ref(),
                capture,
            );
        }
        Self {
            target,
            event,
            capture,
            cb: Some(cb),
        }
    }
}

impl Drop for WindowEventListener {
    fn drop(&mut self) {
        if let Some(cb) = self.cb.take() {
            // Deregister BEFORE the `Closure` is dropped at the end of this
            // scope: a still-registered closure whose backing storage has been
            // freed throws "closure invoked after being dropped" when JS
            // dispatches into it.
            if let Some(target) = &self.target {
                let _ = target.remove_event_listener_with_callback_and_bool(
                    self.event,
                    cb.as_ref().unchecked_ref(),
                    self.capture,
                );
            }
        }
    }
}
