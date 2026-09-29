// SPDX-License-Identifier: MIT OR Apache-2.0

//! A `requestAnimationFrame` callback that cancels its pending frame when
//! dropped, so the browser never invokes a freed `Closure`.

use std::cell::Cell;
use std::rc::{Rc, Weak};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

pub struct AnimationFrame {
    closure: Closure<dyn FnMut()>,
    pending: Rc<Cell<Option<i32>>>,
}

impl AnimationFrame {
    /// Runs `f` on the next frame after each [`AnimationFrame::request`].
    pub fn new(mut f: impl FnMut() + 'static) -> Self {
        let pending = Rc::new(Cell::new(None));
        let closure = Closure::<dyn FnMut()>::new({
            let pending = pending.clone();
            move || {
                pending.set(None);
                f();
            }
        });
        Self { closure, pending }
    }

    /// Runs `tick` every frame from the first [`AnimationFrame::request`] until
    /// the last clone of the returned `Rc` is dropped.
    pub fn new_loop(mut tick: impl FnMut() + 'static) -> Rc<Self> {
        Rc::new_cyclic(|this: &Weak<Self>| {
            let this = this.clone();
            Self::new(move || {
                tick();
                if let Some(this) = this.upgrade() {
                    this.request();
                }
            })
        })
    }

    /// Queues one frame; a no-op while a frame is already pending.
    pub fn request(&self) {
        if self.pending.get().is_some() {
            return;
        }
        if let Some(win) = web_sys::window() {
            if let Ok(id) = win.request_animation_frame(self.closure.as_ref().unchecked_ref()) {
                self.pending.set(Some(id));
            }
        }
    }
}

impl Drop for AnimationFrame {
    fn drop(&mut self) {
        if let (Some(id), Some(win)) = (self.pending.take(), web_sys::window()) {
            let _ = win.cancel_animation_frame(id);
        }
    }
}
