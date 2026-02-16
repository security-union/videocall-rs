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

pub mod attendants;
pub mod browser_compatibility;
pub mod call_timer;
pub mod config_error;
pub mod device_selector;
pub mod device_settings_modal;
pub mod diagnostics;
pub mod google_sign_in_button;
pub mod host;
pub mod host_controls;
pub mod icons;
pub mod login;
pub mod meeting_ended_overlay;
pub mod meeting_info;
pub mod meetings_list;
pub mod neteq_chart;
pub mod okta_sign_in_button;
pub mod top_bar;
pub mod video_control_buttons;
pub mod waiting_room;

mod canvas_generator;
mod peer_list;
mod peer_list_item;
mod peer_tile;
