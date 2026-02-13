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

//! Transport adapters for chat sessions.
//!
//! Each transport adapter is a thin actor that handles transport-specific I/O
//! and delegates all business logic to `SessionLogic`.
//!
//! To add a new transport:
//! 1. Create a new file (e.g., `quic_chat_session.rs`)
//! 2. Create an actor that owns a `SessionLogic`
//! 3. Implement transport-specific send/receive
//! 4. Delegate to `SessionLogic` for all business logic

pub mod ws_chat_session;
pub mod wt_chat_session;

pub use ws_chat_session::WsChatSession;
pub use wt_chat_session::{StopSession, WtChatSession, WtInbound, WtInboundSource, WtOutbound};
