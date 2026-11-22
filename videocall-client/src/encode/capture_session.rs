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
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! One encoder start()'s capture, and the slot through which stop() ends it.

use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;

use web_sys::ReadableStreamDefaultReader;

use super::{read_frame, FrameRead};

/// The capture of one start() of an encoder. It exists before the frame
/// reader does, so stop() can end a start() that is still setting up.
pub(super) struct CaptureSession {
    reader: OnceCell<ReadableStreamDefaultReader>,
    cancelled: Cell<bool>,
}

impl CaptureSession {
    fn new() -> Self {
        Self {
            reader: OnceCell::new(),
            cancelled: Cell::new(false),
        }
    }

    /// Hands the session its frame reader, or cancels the reader at once if
    /// the session was already cancelled.
    pub fn attach(&self, reader: ReadableStreamDefaultReader) {
        if self.is_cancelled() {
            let _ = reader.cancel();
            return;
        }
        // One reader per session; a second one would never be read.
        if let Err(extra) = self.reader.set(reader) {
            let _ = extra.cancel();
        }
    }

    /// Reads the next frame. A session without a reader has ended.
    pub async fn read(&self) -> FrameRead {
        match self.reader.get() {
            Some(reader) => read_frame(reader).await,
            None => FrameRead::Ended,
        }
    }

    /// Ends the capture: a pending read() resolves at once. Idempotent.
    pub fn cancel(&self) {
        if !self.cancelled.replace(true) {
            if let Some(reader) = self.reader.get() {
                // The promise only reports whether the stream was already closed.
                let _ = reader.cancel();
            }
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.get()
    }

    /// Whether the stream ending now means the source died, not that we ended it.
    pub fn end_is_unexpected(&self) -> bool {
        !self.is_cancelled()
    }
}

/// The encoder's current capture session, shared with its running loop.
#[derive(Clone, Default)]
pub(super) struct ActiveCapture(Rc<RefCell<Option<Rc<CaptureSession>>>>);

impl ActiveCapture {
    /// Starts a new session and cancels the previous one, so only one loop runs.
    pub fn begin(&self) -> Rc<CaptureSession> {
        let session = Rc::new(CaptureSession::new());
        let previous = self.0.borrow_mut().replace(session.clone());
        if let Some(previous) = previous {
            previous.cancel();
        }
        session
    }

    /// Cancels the current session, if any.
    pub fn cancel(&self) {
        let current = self.0.borrow_mut().take();
        if let Some(session) = current {
            session.cancel();
        }
    }

    /// Empties the slot if it still holds `session`, leaving a newer one alone.
    pub fn clear_if(&self, session: &Rc<CaptureSession>) {
        let mut slot = self.0.borrow_mut();
        if slot.as_ref().is_some_and(|held| Rc::ptr_eq(held, session)) {
            slot.take();
        }
    }
}
