// SPDX-License-Identifier: MIT OR Apache-2.0

//! Unmount-safe indirection between a long-lived `videocall_client::Callback`
//! and a short-lived Dioxus `EventHandler` prop (issue #2282).
//!
//! [`use_handler_cell`] registers a `use_drop` that empties the cell on unmount,
//! which is what makes [`HandlerCell::callback`]'s `Option` check load-bearing.

use dioxus::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use videocall_client::Callback as VcCallback;

/// Holds the current `EventHandler` prop, and bridges a detached
/// `videocall_client::Callback` into it.
pub struct HandlerCell<T: 'static> {
    inner: Rc<RefCell<Option<EventHandler<T>>>>,
}

impl<T: 'static> Clone for HandlerCell<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T: 'static> Default for HandlerCell<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: 'static> HandlerCell<T> {
    pub fn new() -> Self {
        Self {
            inner: Rc::new(RefCell::new(None)),
        }
    }

    /// Point the cell at the handler for the current render.
    pub fn set(&self, handler: EventHandler<T>) {
        *self.inner.borrow_mut() = Some(handler);
    }

    /// Detach the handler, making every later [`HandlerCell::callback`]
    /// invocation a no-op.
    pub fn clear(&self) {
        *self.inner.borrow_mut() = None;
    }

    /// Build the `videocall_client::Callback` to hand to an encoder. It holds
    /// only an `Rc` to the cell, never the `EventHandler`.
    pub fn callback(&self) -> VcCallback<T> {
        let inner = self.inner.clone();
        VcCallback::from(move |value: T| {
            let handler = inner.borrow().as_ref().copied();
            if let Some(handler) = handler {
                handler.call(value);
            }
        })
    }
}

/// [`HandlerCell`] that empties itself when the calling component unmounts.
///
/// Consumes two hook slots (the cell and the `use_drop`), so it must be called
/// unconditionally and in a stable order like any other hook.
pub fn use_handler_cell<T: 'static>() -> HandlerCell<T> {
    let cell = use_hook(HandlerCell::<T>::new);
    use_drop({
        let cell = cell.clone();
        move || cell.clear()
    });
    cell
}

#[cfg(test)]
mod tests {
    /// Pins `host.rs`'s shape; behaviour is in `tests/handler_cell_teardown.rs`.
    #[test]
    fn host_routes_every_encoder_callback_through_this_hook() {
        let source = include_str!("host.rs");
        assert_eq!(
            source.matches("use_handler_cell::<").count(),
            8,
            "all 8 of Host's encoder handler cells must come from use_handler_cell"
        );
        assert!(
            !source.contains(concat!("RefCell<Option<Event", "Handler")),
            "a hand-rolled handler cell in host.rs bypasses the unmount clear (#2282)"
        );
    }
}
