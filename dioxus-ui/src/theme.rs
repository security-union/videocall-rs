// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared v0 style tokens for Rust-rendered UI snippets.
//! Keep this intentionally small and aligned to the current dark UI.

pub mod color {
    pub const BG: &str = "#000000";

    pub const TEXT_PRIMARY: &str = "#fff";
    pub const TEXT_MUTED: &str = "#aaa";
    pub const TEXT_SUBTLE: &str = "#888";

    /// Error foreground text — WCAG AA compliant on glass surfaces (≥4.5:1).
    /// Matches --error-text in global.css.
    pub const ERROR_TEXT: &str = "#FF7A6E";

    pub const AXIS: &str = "#666";

    pub const SIGNAL_AUDIO: &str = "#4FC3F7";
    pub const SIGNAL_VIDEO: &str = "#81C784";
    pub const SIGNAL_SCREEN: &str = "#CE93D8";
    pub const SIGNAL_LATENCY: &str = "#FF8A65";
    pub const SIGNAL_LATENCY_DIM: &str = "rgba(255,138,101,0.4)";
    pub const SIGNAL_GRID_MAJOR: &str = "rgba(255,255,255,0.1)";
    pub const SIGNAL_GRID_MINOR: &str = "rgba(255,255,255,0.07)";
    pub const TOOLTIP_DIVIDER: &str = "rgba(255,255,255,0.15)";

    /// Error foreground for input validation messages — matches --error-text (WCAG AA).
    pub const INPUT_ERROR: &str = ERROR_TEXT;
    pub const PREVIEW_AVATAR_BG: &str = "#3a3a3a";
    pub const PREVIEW_AVATAR_RING: &str = "rgba(0,0,0,0.62)";

    pub const NETEQ_BUFFER: &str = "#8ef";
    pub const NETEQ_JITTER: &str = "#ff8";
    pub const NETEQ_BLUE: &str = "#007bff";
    pub const NETEQ_GREEN: &str = "#28a745";
    pub const NETEQ_RED: &str = "#dc3545";
    pub const NETEQ_ORANGE: &str = "#fd7e14";
    pub const NETEQ_PURPLE: &str = "#6f42c1";
    pub const NETEQ_TEAL: &str = "#17a2b8";
    pub const NETEQ_AMBER: &str = "#ffc107";
}
