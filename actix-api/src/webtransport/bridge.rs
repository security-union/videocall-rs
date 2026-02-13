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

//! WebTransport Actor Bridge
//!
//! Bridges the gap between WebTransport (quinn async I/O) and Actix actors.
//!
//! Quinn uses pure tokio async, while actors use Actix's LocalSet runtime.
//! This bridge spawns I/O tasks that communicate with the actor via messages
//! and channels.
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────┐
//! │                    WebTransportBridge                                │
//! ├─────────────────────────────────────────────────────────────────────┤
//! │  ┌──────────────────┐              ┌──────────────────┐             │
//! │  │ UniStream Reader │              │ Datagram Reader  │             │
//! │  │ session.accept_  │              │ session.read_    │             │
//! │  │ uni().await      │              │ datagram().await │             │
//! │  └────────┬─────────┘              └────────┬─────────┘             │
//! │           │                                 │                       │
//! │           │ WtInbound(UniStream)            │ WtInbound(Datagram)   │
//! │           └────────────┬────────────────────┘                       │
//! │                        ▼                                            │
//! │           ┌────────────────────────┐                                │
//! │           │      Actor (external)  │                                │
//! │           └────────────┬───────────┘                                │
//! │                        │ outbound channel                           │
//! │                        ▼                                            │
//! │           ┌────────────────────────┐                                │
//! │           │      Writer Task       │                                │
//! │           │  UniStream / Datagram  │                                │
//! │           └────────────────────────┘                                │
//! └─────────────────────────────────────────────────────────────────────┘
//! ```

use crate::actors::transports::wt_chat_session::{WtInbound, WtInboundSource, WtOutbound};
use actix::Addr;
use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing::{error, info};
use web_transport_quinn::Session;

/// Callback for tracking packets sent to clients (used in tests)
pub type PacketSentCallback = Box<dyn Fn() + Send + Sync>;

/// Bridge between WebTransport session and an Actix actor.
///
/// Spawns I/O tasks that:
/// - Read from WebTransport streams/datagrams → send `WtInbound` to actor
/// - Receive from outbound channel → write to WebTransport streams/datagrams
pub struct WebTransportBridge {
    join_set: JoinSet<()>,
}

impl WebTransportBridge {
    /// Create a new bridge and start I/O tasks.
    ///
    /// # Arguments
    /// * `session` - The WebTransport session (quinn)
    /// * `actor_addr` - Address of the actor to receive inbound messages
    /// * `outbound_rx` - Channel receiver for outbound messages from actor
    #[allow(dead_code)] // Useful API even if currently only new_with_callback is used
    pub fn new<A>(
        session: Session,
        actor_addr: Addr<A>,
        outbound_rx: mpsc::Receiver<WtOutbound>,
    ) -> Self
    where
        A: actix::Actor<Context = actix::Context<A>> + actix::Handler<WtInbound>,
    {
        Self::new_with_callback(session, actor_addr, outbound_rx, None)
    }

    /// Create a new bridge with optional callback for packet tracking.
    pub fn new_with_callback<A>(
        session: Session,
        actor_addr: Addr<A>,
        outbound_rx: mpsc::Receiver<WtOutbound>,
        on_packet_sent: Option<PacketSentCallback>,
    ) -> Self
    where
        A: actix::Actor<Context = actix::Context<A>> + actix::Handler<WtInbound>,
    {
        let mut join_set = JoinSet::new();

        Self::spawn_unistream_reader(&mut join_set, session.clone(), actor_addr.clone());
        Self::spawn_datagram_reader(&mut join_set, session.clone(), actor_addr);
        Self::spawn_writer(&mut join_set, session, outbound_rx, on_packet_sent);

        Self { join_set }
    }

    /// Wait for any I/O task to complete (indicates session end).
    pub async fn wait_for_disconnect(&mut self) {
        self.join_set.join_next().await;
    }

    /// Shutdown all I/O tasks.
    pub async fn shutdown(mut self) {
        self.join_set.shutdown().await;
    }

    /// Spawn UniStream reader task.
    fn spawn_unistream_reader<A>(join_set: &mut JoinSet<()>, session: Session, actor_addr: Addr<A>)
    where
        A: actix::Actor<Context = actix::Context<A>> + actix::Handler<WtInbound>,
    {
        join_set.spawn(async move {
            while let Ok(mut uni_stream) = session.accept_uni().await {
                let actor_addr = actor_addr.clone();
                tokio::spawn(async move {
                    match uni_stream.read_to_end(usize::MAX).await {
                        Ok(buf) => {
                            let _ = actor_addr.try_send(WtInbound {
                                data: Bytes::from(buf),
                                source: WtInboundSource::UniStream,
                            });
                        }
                        Err(e) => {
                            error!("Error reading from UniStream: {}", e);
                        }
                    }
                });
            }
            info!("WebTransport UniStream reader ended");
        });
    }

    /// Spawn Datagram reader task.
    fn spawn_datagram_reader<A>(join_set: &mut JoinSet<()>, session: Session, actor_addr: Addr<A>)
    where
        A: actix::Actor<Context = actix::Context<A>> + actix::Handler<WtInbound>,
    {
        join_set.spawn(async move {
            while let Ok(buf) = session.read_datagram().await {
                let _ = actor_addr.try_send(WtInbound {
                    data: buf,
                    source: WtInboundSource::Datagram,
                });
            }
            info!("WebTransport Datagram reader ended");
        });
    }

    /// Spawn Writer task.
    fn spawn_writer(
        join_set: &mut JoinSet<()>,
        session: Session,
        mut outbound_rx: mpsc::Receiver<WtOutbound>,
        on_packet_sent: Option<PacketSentCallback>,
    ) {
        join_set.spawn(async move {
            while let Some(msg) = outbound_rx.recv().await {
                match msg {
                    WtOutbound::UniStream(data) => match session.open_uni().await {
                        Ok(mut stream) => {
                            if let Err(e) = stream.write_all(&data).await {
                                error!("Error writing to UniStream: {}", e);
                                break;
                            }
                            // Call packet sent callback if provided (for test instrumentation)
                            if let Some(ref callback) = on_packet_sent {
                                callback();
                            }
                        }
                        Err(e) => {
                            error!("Error opening UniStream: {}", e);
                            break;
                        }
                    },
                    WtOutbound::Datagram(data) => {
                        if let Err(e) = session.send_datagram(data) {
                            error!("Error sending datagram: {}", e);
                            // Don't break on datagram errors - they're unreliable
                        }
                    }
                }
            }
            info!("WebTransport Writer ended");
        });
    }
}
