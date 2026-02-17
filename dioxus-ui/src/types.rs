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

use serde::{Deserialize, Serialize};

/// Represents device information including both ID and human-readable name
/// for better debugging and logging across components
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// The unique device identifier used by browser APIs
    pub device_id: String,
    /// The human-readable device name shown to users
    pub device_name: String,
}

impl DeviceInfo {
    /// Create a new DeviceInfo from ID and name
    pub fn new(device_id: String, device_name: String) -> Self {
        Self {
            device_id,
            device_name,
        }
    }

    /// Create a DeviceInfo from a MediaDeviceInfo
    pub fn from_media_device_info(device: &web_sys::MediaDeviceInfo) -> Self {
        Self {
            device_id: device.device_id(),
            device_name: device.label(),
        }
    }
}

impl std::fmt::Display for DeviceInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.device_name, self.device_id)
    }
}
