// SPDX-License-Identifier: MIT OR Apache-2.0

pub mod about_modal;
mod action_bar_gesture;
pub mod action_bar_layout;
mod action_bar_shrink;
pub mod animation_frame;
pub mod announcement_overrides;
pub mod appearance_settings_panel;
pub mod attendants;
pub mod attendants_layout;
pub mod avatar;
pub mod avatar_settings;
pub mod browser_compatibility;
pub mod call_timer;
pub mod capability_check;
pub mod captions;
pub mod captions_overlay;
pub mod changelog;
pub mod co_host_picker;
pub mod co_hosts;
pub mod color_picker;
pub mod config_error;
pub mod connection_quality_indicator;
pub mod connection_status;
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
pub mod display_name_edit;
pub mod freshness_inject;
pub mod google_sign_in_button;
pub mod grid_overflow_badge;
pub mod handler_cell;
pub mod hero_orbs;
pub mod host;
pub mod host_action_notice;
pub mod host_controls;
pub mod icons;
pub mod invalid_meeting_id;
pub mod login;
pub mod media_metrics_overlay;
pub mod meeting_ended_overlay;
pub mod meeting_footer;
pub mod meeting_format;
pub mod meeting_options_controls;
pub mod meeting_restarted_overlay;
pub mod wt_receive_inject;
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
pub mod overflow_participants;
pub mod peer_list_item;
pub mod peer_list_sort;
pub mod performance_settings;
pub mod pin_order;
pub mod preferences_settings_panel;
// Issue 2135: the raised-hand roster (ordering + copy) and its persistent
// banner. Pure helpers are host-testable; the banner is a thin driver.
pub mod raised_hands;
// Issue #1884: pure reaction UI logic (enum→glyph table + overlay coalesce/cap),
// host-testable.
pub mod emoji_picker;
pub mod reactions;
pub mod reactions_overlay;
pub mod recent_actions;
pub mod recording_status_bar;
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
// wasm-only: it touches `web_sys::window`, `window.open`, and `captureStream`.
#[cfg(target_arch = "wasm32")]
pub mod screen_share_detach;
pub mod search_modal;
pub mod self_view;
pub mod self_view_hidden_pill;
pub mod share_actions;
pub mod share_view;
pub mod signal_quality;
pub(crate) mod tile_menu_anchor;
#[doc(hidden)]
#[allow(unused_imports)]
pub use tile_menu_anchor::live_tile_menu_dismiss_listeners_for_test;
pub mod toggle_switch;
pub mod transport_fallback;

pub mod top_bar;
pub mod update_display_name_modal;
pub mod video_control_buttons;
pub mod waiting_room;
mod window_event_listener;

pub mod canvas_generator;
pub mod peer_list;
pub mod peer_tile;
pub mod pre_join_preview;
pub mod pre_join_settings_card;
// Issue 2702: keeps the manual pre-join lobby's presence lease alive while
// admitted (REST `/join`) but not yet transport-connected. Pure scheduling
// state is host-testable; the component is a thin driver, directly mountable
// on its own so a wasm test does not need the whole `AttendantsComponent`.
pub mod presence_keepalive;
