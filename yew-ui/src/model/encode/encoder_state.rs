use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;

//
// EncoderState struct contains state variables that are common among the encoders, and the logic
// for working with them.
//

#[derive(Clone)]
pub struct EncoderState {
    pub(super) destroy: Arc<AtomicBool>,
    pub(super) enabled: Arc<AtomicBool>,
    pub(super) selected: Option<String>,
    pub(super) switching: Arc<AtomicBool>,
}

impl EncoderState {
    pub fn new() -> Self {
        Self {
            destroy: Arc::new(AtomicBool::new(false)),
            enabled: Arc::new(AtomicBool::new(false)),
            selected: None,
            switching: Arc::new(AtomicBool::new(false)),
        }
    }

    // Sets the enabled bit to a given value, returning true if it was a change.
    pub fn set_enabled(&mut self, value: bool) -> bool {
        if value != self.enabled.load(Ordering::Acquire) {
            self.enabled.store(value, Ordering::Release);
            true
        } else {
            false
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub fn select(&mut self, device: String) -> bool {
        self.selected = Some(device);
        if self.is_enabled() {
            self.switching.store(true, Ordering::Release);
            true
        } else {
            false
        }
    }

    pub fn stop(&mut self) {
        self.destroy.store(true, Ordering::Release);
    }
}
