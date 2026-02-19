use super::*;
use yew::prelude::Callback;

#[allow(clippy::new_without_default)]
impl MediaDeviceAccess {
    /// Constructor for the device access struct.
    ///
    /// After construction, set the callbacks, then call the [`request()`](Self::request) method to request
    /// access, e.g.:
    ///
    /// ```no_run
    /// # use videocall_client::MediaDeviceAccess;
    /// # use wasm_bindgen::JsValue;
    /// # use yew::Callback;
    /// let mut media_device_access = MediaDeviceAccess::new();
    /// media_device_access.on_granted = Callback::from(|_| {
    ///     // Handle granted permission
    /// });
    /// media_device_access.on_denied = Callback::from(|_err: JsValue| {
    ///     // Handle denied permission
    /// });
    /// media_device_access.request();
    /// ```
    pub fn new() -> Self {
        Self {
            granted: Arc::new(AtomicBool::new(false)),
            on_granted: Callback::noop(),
            on_denied: Callback::noop(),
        }
    }

    /// Causes the browser to request the user's permission to access the microphone and camera.
    ///
    /// This function returns immediately.  Eventually, either the [`on_granted`](Self::on_granted)
    /// or [`on_denied`](Self::on_denied) callback will be called.
    pub fn request(&self) {
        let on_granted = self.on_granted.clone();
        let on_denied = self.on_denied.clone();
        let wrapped_granted: Rc<dyn Fn()> = Rc::new(move || on_granted.emit(()));
        let wrapped_denied: Rc<dyn Fn(JsValue)> = Rc::new(move |e| on_denied.emit(e));
        run_request(Arc::clone(&self.granted), wrapped_granted, wrapped_denied);
    }
}
