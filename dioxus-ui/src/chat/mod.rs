// SPDX-License-Identifier: MIT OR Apache-2.0

//! Chat adapter layer for external chat service integration.
//!
//! This module provides a service-agnostic abstraction over external chat
//! backends. The adapter is driven entirely by deploy-time configuration
//! (`window.__APP_CONFIG` chat fields). When chat is disabled, none of
//! this code is exercised at runtime.
//!
//! # Module layout
//!
//! - [`types`] — Data structures: `ChatMessage`, `ChatRoom`, `ChatError`,
//!   `ChatConfig`, `ChatAuthMode`.
//! - [`adapter`] — The `ChatServiceAdapter` trait definition.
//! - [`generic_adapter`] — Config-driven HTTP adapter implementing the trait.
//! - [`context`] — Dioxus context types and hooks for reactive chat state.

pub mod adapter;
pub mod context;
pub mod generic_adapter;
pub mod types;

// Re-export the most commonly used items for convenience.
pub use adapter::ChatServiceAdapter;
pub use context::{use_chat_state, ChatState};
pub use generic_adapter::GenericChatAdapter;
pub use types::{ChatConfig, ChatError, ChatMessage};
