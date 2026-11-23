// SPDX-License-Identifier: MIT OR Apache-2.0

//! The iOS-style toggle switch now lives in the design system as
//! [`videocall_theme::Switch`]. This module keeps the old name, so call sites
//! (`toggle_switch::ToggleSwitch`) did not have to change.
//!
//! Two props changed. `width` and `height` are now `Option<u32>`: left
//! out, the size comes from the `.switch` class, and given, they set the
//! `--switch-width`/`--switch-height` variables. The default track is now
//! 51x31 px, the iOS size, where it used to be 44x24. The rest (`enabled`,
//! `on_toggle`, `disabled`, `test_id`, `labelled_by`) is unchanged, plus
//! `class` and the usual pass-through of global attributes.

pub use videocall_theme::Switch as ToggleSwitch;
