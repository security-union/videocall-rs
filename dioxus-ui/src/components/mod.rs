// SPDX-License-Identifier: MIT OR Apache-2.0

pub mod about_modal;
pub mod action_bar_layout;
pub mod appearance_settings_panel;
pub mod attendants;
mod attendants_layout;
pub mod browser_compatibility;
pub mod call_timer;
pub mod capability_check;
pub mod color_picker;
pub mod config_error;
pub mod connection_quality_indicator;
// Issue #367: test-only diagnostics-bus injection hook that publishes synthetic
// `active_server_rtt` samples for the indicator above (gated on
// MOCK_PEERS_ENABLED). Registers window.__videocall_inject_server_rtt.
pub mod connection_quality_inject;
pub mod decode_budget;
pub mod decode_budget_banner;
pub mod decode_budget_inject;
pub mod decode_paused_pill;
pub mod density;
pub mod device_selector;
pub mod device_settings_modal;
pub mod diagnostics;
pub mod freshness_inject;
pub mod google_sign_in_button;
pub mod grid_overflow_badge;
pub mod handler_cell;
pub mod host;
pub mod host_controls;
pub mod icons;
pub mod login;
pub mod media_metrics_overlay;
pub mod meeting_ended_overlay;
pub mod meeting_format;
pub mod meeting_info;
pub mod meeting_options_controls;
// Issue 1613: the meeting-password prompt. The error-code -> prompt mapping and
// the prompt's attempt bookkeeping are pure and host-testable; the component is
// a thin driver over them.
pub mod meeting_password_prompt;
// Issue 2136: the host-set meeting countdown. Pure formatting / urgency /
// milestone helpers are host-testable; the components are thin drivers, and
// they are deliberately the ONLY readers of `MeetingTimerCtx` — nothing outside
// this module subscribes to it, which is what bounds a timer transition's
// re-render blast radius. See the module docs.
pub mod meeting_timer;
pub mod meetings_filter;
pub mod meetings_list;
pub mod neteq_chart;
pub mod okta_sign_in_button;
pub mod peer_list_item;
pub mod performance_settings;
pub mod preferences_settings_panel;
// Issue 2135: the raised-hand roster (ordering + copy) and its persistent
// banner. Pure helpers are host-testable; the banner is a thin driver.
pub mod raised_hands;
// Issue #1884: pure reaction UI logic (enum→glyph table + overlay coalesce/cap),
// host-testable.
pub mod emoji_picker;
pub mod reactions;
pub mod reactions_overlay;
// HCL #893: test-only injection hook for the SCREEN first-render ack (gated on
// MOCK_PEERS_ENABLED). Registers window.__videocall_inject_screen_first_render.
pub mod screen_first_render_inject;
// Issue 1175: pure zoom/pan math for received shared content (host-testable).
pub mod screen_share_zoom;
// Issue 1175: imperative glue to detach shared content into a separate window.
// Pure detached-window sizing math (host-testable), split out of the wasm-only
// detach module below (issue #1842). Its only non-test consumer
// (`screen_share_detach`) is `#[cfg(target_arch = "wasm32")]`-gated, so on native
// builds the items look unused (the host `#[test]`s still exercise them) — allow
// dead_code off-wasm so the native `-D warnings` clippy job stays clean.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) mod screen_share_detach_sizing;
// wasm-only: it touches `web_sys::window`, Document PiP / window.open, and
// `captureStream`.
#[cfg(target_arch = "wasm32")]
pub mod screen_share_detach;
pub mod search_modal;
pub mod signal_quality;
pub mod toggle_switch;

pub mod top_bar;
pub mod update_display_name_modal;
pub mod video_control_buttons;
pub mod waiting_room;

pub mod canvas_generator;
pub mod chat_sidebar;
mod peer_list;
pub mod peer_tile;
pub mod pre_join_preview;
pub mod pre_join_settings_card;
