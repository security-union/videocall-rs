// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub device_id: String,
    pub device_name: String,
}

impl DeviceInfo {
    #[allow(dead_code)]
    pub fn new(device_id: String, device_name: String) -> Self {
        Self {
            device_id,
            device_name,
        }
    }

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
