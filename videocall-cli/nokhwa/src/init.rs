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

#[cfg(not(all(
    feature = "input-avfoundation",
    any(target_os = "macos", target_os = "ios")
)))]
fn init_avfoundation(callback: impl Fn(bool) + Send + 'static) {
    callback(true);
}

#[cfg(all(
    feature = "input-avfoundation",
    any(target_os = "macos", target_os = "ios")
))]
fn init_avfoundation(callback: impl Fn(bool) + Send + Sync + 'static) {
    use videocall_nokhwa_bindings_macos::request_permission_with_callback;

    request_permission_with_callback(callback);
}

#[cfg(not(all(
    feature = "input-avfoundation",
    any(target_os = "macos", target_os = "ios")
)))]
fn status_avfoundation() -> bool {
    true
}

#[cfg(all(
    feature = "input-avfoundation",
    any(target_os = "macos", target_os = "ios")
))]
fn status_avfoundation() -> bool {
    use videocall_nokhwa_bindings_macos::{current_authorization_status, AVAuthorizationStatus};

    matches!(
        current_authorization_status(),
        AVAuthorizationStatus::Authorized
    )
}

// todo: make this work on browser code
/// Initialize `nokhwa`
/// It is your responsibility to call this function before anything else, but only on `MacOS`.
///
/// The `on_complete` is called after initialization (a.k.a User granted permission). The callback's argument
/// is weather the initialization was successful or not
pub fn nokhwa_initialize(on_complete: impl Fn(bool) + Send + Sync + 'static) {
    init_avfoundation(on_complete);
}

/// Check the status if `nokhwa`
/// True if the initialization is successful (ready-to-use)
#[must_use]
pub fn nokhwa_check() -> bool {
    status_avfoundation()
}
