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

mod media_device_access;
mod media_device_list;

// Crate-internal: the encoders classify their own `getUserMedia` rejections
// through the SAME logic as the pre-flight permission probe, so device-in-use /
// no-device / permission-denied are labeled identically wherever they surface.
pub(crate) use media_device_access::classify_get_user_media_error;
pub use media_device_access::MediaAccessKind;
pub use media_device_access::MediaDeviceAccess;
pub use media_device_access::MediaPermission;
pub use media_device_access::MediaPermissionsErrorState;
pub use media_device_access::PermissionState;
pub use media_device_list::{MediaDeviceList, SelectableDevices};
