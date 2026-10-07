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
 */

//! #2727: per-publisher fan-out order survives the arbiter hop.
//!
//! Sharding moved each receiver's session actor onto its own arbiter, so a
//! publisher's packets now cross a thread boundary between the relay's
//! per-session NATS loop and the actor that drains it. Ordering per publisher is
//! the invariant that replaces pinning the loop to the owning arbiter, so it is
//! pinned here through the production path: `ChatServer` publishes to NATS, the
//! receiver's own subscription loop forwards, and the receiver's actor -- which
//! this test puts on a real `SessionShards` arbiter, asserting it is NOT the
//! test's thread -- records arrival order.
//!
//! ## Prerequisites
//!
//! A running NATS server (`NATS_URL`, default `nats://nats:4222`).

use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use actix::{Actor, Context, Handler};
use protobuf::Message as ProtoMessage;
use sec_api::actors::chat_server::ChatServer;
use sec_api::messages::server::{ActivateConnection, ClientMessage, Connect, JoinRoom, Packet};
use sec_api::messages::session::Message;
use sec_api::relay_shards::SessionShards;
use serial_test::serial;
use tokio::sync::oneshot;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

/// Enough packets that a reordering fan-out is overwhelmingly likely to be
/// caught.
const PACKETS: u32 = 64;

/// The recorder's mailbox, sized for the WHOLE burst.
///
/// The inbound fan-out hop `try_send`s (`chat_server.rs`, the #1145 attribution
/// block), so a full mailbox is a DROP, not a delay, and nothing re-delivers it.
/// actix's `DEFAULT_CAPACITY` is 16 (`mailbox.rs:11`), and this test publishes
/// 64 packets that the receiver's subscription loop forwards in one pass: on a
/// loaded machine the recorder's arbiter does not get scheduled inside that
/// pass, the mailbox fills, and the rest are shed as designed. Sized here so the
/// property under test is ORDER rather than the relay's backpressure.
const MAILBOX_SLOTS: usize = PACKETS as usize * 2;

/// Sequence number of a warm-up packet: outside `0..PACKETS`, so the recorder
/// counts it without ever putting it in the measured sequence.
const WARMUP_SEQ: u32 = u32::MAX;

/// How long the barrier below waits for the first warm-up packet to make the
/// whole round trip, and how many polls it leaves between warm-ups.
const WARMUP_BUDGET: Duration = Duration::from_secs(30);
const WARMUP_EVERY: usize = 8;

/// Global ceiling, and the per-packet bound that actually fires first: the
/// stall budget restarts on every packet that lands, so a slow-but-progressing
/// delivery is not killed by the wall clock while a wedged one fails promptly.
const SETTLE_BUDGET: Duration = Duration::from_secs(60);
const STALL_BUDGET: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(25);

const PUBLISHER_SESSION: u64 = 27_270_001;
const RECEIVER_SESSION: u64 = 27_270_002;

/// AUDIO on purpose: it is the one media class the relay never layer-filters
/// (every audio packet is stamped layer 0 and the relay's filter is gated on a
/// non-zero layer) and the #988 viewport filter is video-only, so an out-of-order
/// result cannot be a filtered packet in disguise.
fn audio_packet(seq: u32) -> Vec<u8> {
    let inner = MediaPacket {
        media_type: MediaType::AUDIO.into(),
        data: seq.to_be_bytes().to_vec(),
        ..Default::default()
    }
    .write_to_bytes()
    .expect("a MediaPacket must encode");

    PacketWrapper {
        packet_type: PacketType::MEDIA.into(),
        session_id: PUBLISHER_SESSION,
        user_id: b"publisher@example.com".to_vec(),
        data: inner,
        ..Default::default()
    }
    .write_to_bytes()
    .expect("a PacketWrapper must encode")
}

/// The sequence number a forwarded packet carries, or `None` for anything that
/// is not one of ours (lifecycle and presence packets share this channel).
fn sequence_of(bytes: &[u8]) -> Option<u32> {
    let wrapper = PacketWrapper::parse_from_bytes(bytes).ok()?;
    if wrapper.packet_type.enum_value().ok()? != PacketType::MEDIA {
        return None;
    }
    let inner = MediaPacket::parse_from_bytes(&wrapper.data).ok()?;
    let raw: [u8; 4] = inner.data.get(..4)?.try_into().ok()?;
    Some(u32::from_be_bytes(raw))
}

/// Every `drop_reason` the inbound fan-out hop books for one room.
const FANOUT_DROP_REASONS: [&str; 3] =
    ["mailbox_full", "priority_drop_audio", "priority_drop_video"];

fn fanout_drop_count(room: &str, reason: &str) -> f64 {
    sec_api::metrics::RELAY_PACKET_DROPS_TOTAL
        .with_label_values(&[room, "nats_delivery", reason])
        .get()
}

fn fanout_drop_total(room: &str) -> f64 {
    FANOUT_DROP_REASONS
        .iter()
        .map(|reason| fanout_drop_count(room, reason))
        .sum()
}

/// What the hop booked, for a failure message: a short count is explained here
/// or not at all.
fn fanout_drops(room: &str) -> String {
    FANOUT_DROP_REASONS
        .iter()
        .map(|reason| format!("{reason}={}", fanout_drop_count(room, reason)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Records the order in which forwarded packets reach a receiver's actor.
struct RecordingSession {
    seen: Arc<Mutex<Vec<u32>>>,
    warmups: Arc<std::sync::atomic::AtomicUsize>,
}

impl Actor for RecordingSession {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        ctx.set_mailbox_capacity(MAILBOX_SLOTS);
    }
}

impl Handler<Message> for RecordingSession {
    type Result = ();

    fn handle(&mut self, msg: Message, _ctx: &mut Self::Context) {
        match sequence_of(&msg.msg) {
            Some(WARMUP_SEQ) => {
                self.warmups
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Some(seq) => self
                .seen
                .lock()
                .expect("recorder must not be poisoned")
                .push(seq),
            None => {}
        }
    }
}

fn never_epoch() -> Arc<AtomicU64> {
    Arc::new(AtomicU64::new(0))
}

fn join(session: u64, room: &str, user: &str, transport: &str) -> JoinRoom {
    JoinRoom {
        session,
        room: room.to_string(),
        user_id: user.to_string(),
        display_name: user.to_string(),
        is_guest: false,
        observer: false,
        instance_id: None,
        is_host: false,
        transport: transport.to_string(),
        downlink_congested_epoch: never_epoch(),
        token_iat: None,
        closer: None,
    }
}

/// Runs on the relay's PRODUCTION runtime shape (#2727): a multi-threaded tokio
/// runtime with the actix `System` on its main-thread `LocalSet`, exactly what
/// `bin/webtransport_server.rs` builds past one arbiter. That is load-bearing,
/// not decoration -- under a current-thread runtime, spawned tasks poll in spawn
/// order, so a fan-out that dispatched each packet to its own task would still
/// arrive ordered and this test would pass against a broken relay.
#[test]
#[serial(wt_shard_fanout_ordering)]
fn fan_out_order_per_publisher_survives_the_arbiter_hop() {
    actix_rt::System::with_tokio_rt(|| {
        tokio::runtime::Builder::new_multi_thread()
            // Explicit, like the relay: without it tokio reads TOKIO_WORKER_THREADS
            // and panics on a hostile value a developer happens to be carrying.
            .worker_threads(sec_api::relay_shards::resolve_worker_thread_count(2))
            .enable_all()
            .build()
            .expect("the test needs the relay's multi-threaded runtime")
    })
    .block_on(fan_out_order_body());
}

async fn fan_out_order_body() {
    let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
    let nats_client = async_nats::connect(&nats_url)
        .await
        .expect("Failed to connect to NATS");

    // Two shards, so the receiver's actor is placed by the SAME production
    // `spawn_session` the WebTransport accept loop calls.
    let shards = SessionShards::new(2);
    let seen = Arc::new(Mutex::new(Vec::<u32>::new()));
    let warmups = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let (ready_tx, ready_rx) = oneshot::channel::<(actix::Recipient<Message>, ThreadId)>();
    let (release_tx, release_rx) = oneshot::channel::<()>();
    let seen_for_actor = seen.clone();
    let warmups_for_actor = warmups.clone();
    assert!(
        shards.spawn_session(move |lease| async move {
            let addr = RecordingSession {
                seen: seen_for_actor,
                warmups: warmups_for_actor,
            }
            .start();
            let _ = ready_tx.send((addr.clone().recipient(), std::thread::current().id()));
            // Hold the actor and its shard slot open for the whole test.
            let _ = release_rx.await;
            drop(addr);
            drop(lease);
        }),
        "the receiver session must be accepted by a live shard"
    );

    let (recipient, actor_thread) = ready_rx
        .await
        .expect("the receiver actor must report its recipient");
    assert_ne!(
        actor_thread,
        std::thread::current().id(),
        "the receiver actor must be on an arbiter, or this test would not cross a thread \
         boundary and would prove nothing about the hop"
    );

    let chat_server = ChatServer::new(nats_client.clone()).await.start();
    let room = "shard-fanout-ordering".to_string();

    // The receiver: Connect + JoinRoom spawns its per-session NATS subscription
    // loop, which is the production hop under test.
    chat_server
        .send(Connect {
            id: RECEIVER_SESSION,
            addr: recipient,
        })
        .await
        .expect("receiver Connect must succeed");
    chat_server
        .send(join(
            RECEIVER_SESSION,
            &room,
            "receiver@example.com",
            "webtransport",
        ))
        .await
        .expect("receiver JoinRoom must be delivered")
        .expect("receiver JoinRoom must be accepted");
    chat_server
        .send(ActivateConnection {
            session: RECEIVER_SESSION,
        })
        .await
        .expect("receiver ActivateConnection must succeed");

    // The publisher. Its own actor is irrelevant here, but it must be Active or
    // `ChatServer` will not publish its packets at all.
    struct SilentSession;
    impl Actor for SilentSession {
        type Context = Context<Self>;
    }
    impl Handler<Message> for SilentSession {
        type Result = ();
        fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
    }
    let publisher = SilentSession.start();
    chat_server
        .send(Connect {
            id: PUBLISHER_SESSION,
            addr: publisher.recipient(),
        })
        .await
        .expect("publisher Connect must succeed");
    chat_server
        .send(join(
            PUBLISHER_SESSION,
            &room,
            "publisher@example.com",
            "webtransport",
        ))
        .await
        .expect("publisher JoinRoom must be delivered")
        .expect("publisher JoinRoom must be accepted");
    chat_server
        .send(ActivateConnection {
            session: PUBLISHER_SESSION,
        })
        .await
        .expect("publisher ActivateConnection must succeed");

    // BARRIER -- this is what the test was missing (#2727 flake).
    //
    // `Handler<JoinRoom>` establishes the receiver's per-session NATS
    // subscription in a `tokio::spawn`ed task, and awaiting the `JoinRoom`
    // message only proves the handler RAN, not that `queue_subscribe` has taken
    // effect. Core NATS has no replay: a packet published before the
    // subscription is live reaches nobody and is not counted anywhere, which is
    // exactly the "N of 64 packets, no drops recorded" failure. Under load the
    // spawned task can lose the race by seconds.
    //
    // So: publish warm-up packets until one completes the whole round trip. The
    // recorder counts them without recording them, and the measured burst below
    // starts from a pipeline proven live end to end.
    let warmup_started = Instant::now();
    let mut polls = 0usize;
    loop {
        if warmups.load(std::sync::atomic::Ordering::Relaxed) > 0 {
            break;
        }
        assert!(
            warmup_started.elapsed() < WARMUP_BUDGET,
            "no warm-up packet completed the round trip in {WARMUP_BUDGET:?}: the \
             receiver's subscription never went live, so nothing this test \
             measures could work"
        );
        // One warm-up per WARMUP_EVERY polls, not per poll: once the
        // subscription goes live every warm-up still in flight lands in the
        // recorder's mailbox, and a 25 ms cadence could put enough of them
        // there to shed the measured burst behind them.
        if polls.is_multiple_of(WARMUP_EVERY) {
            chat_server
                .send(ClientMessage {
                    session: PUBLISHER_SESSION,
                    room: room.clone(),
                    msg: Packet {
                        data: Arc::new(audio_packet(WARMUP_SEQ)),
                        requires_host: false,
                    },
                    user: "publisher@example.com".to_string(),
                    requires_host: false,
                })
                .await
                .expect("publishing a warm-up packet must succeed");
        }
        polls += 1;
        actix_rt::time::sleep(POLL).await;
    }

    let drops_before = fanout_drop_total(&room);

    // Wire tap: a plain subscribe alongside the receiver's queue-group
    // subscription, so it observes the same publications without stealing any.
    // Its count separates "the publish side never got the packet onto NATS"
    // from "the fan-out hop had it and the receiver's actor never ran".
    let on_wire = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let wire_sub = nats_client
        .subscribe(format!("room.{room}.{PUBLISHER_SESSION}"))
        .await
        .expect("the wire tap must subscribe");
    let on_wire_for_task = on_wire.clone();
    let wire_task = actix_rt::spawn(async move {
        let mut sub = wire_sub;
        while let Some(msg) = futures::StreamExt::next(&mut sub).await {
            if sequence_of(&msg.payload).is_some() {
                on_wire_for_task.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    });

    for seq in 0..PACKETS {
        chat_server
            .send(ClientMessage {
                session: PUBLISHER_SESSION,
                room: room.clone(),
                msg: Packet {
                    data: Arc::new(audio_packet(seq)),
                    requires_host: false,
                },
                user: "publisher@example.com".to_string(),
                requires_host: false,
            })
            .await
            .expect("publishing must succeed");
    }

    // Collected rather than asserted, so the cleanup below ALWAYS runs. A panic
    // here used to leave the wire tap's `Subscriber` alive into runtime
    // teardown, and async-nats drops one by calling `tokio::spawn`, which then
    // panics with no runtime — burying the real failure under a second one.
    let outcome: Result<(), String> = async {
        let deadline = Instant::now() + SETTLE_BUDGET;
        let mut landed = 0usize;
        let mut last_progress = Instant::now();
        loop {
            let got = seen.lock().expect("recorder must not be poisoned").len();
            if got >= PACKETS as usize {
                break;
            }
            if got > landed {
                landed = got;
                last_progress = Instant::now();
            }
            if last_progress.elapsed() >= STALL_BUDGET || Instant::now() >= deadline {
                return Err(format!(
                    "delivery stalled at {got} of {PACKETS} packets: {} reached NATS, \
                     fan-out drops {}. All on the wire with no drops means the \
                     receiver's subscription loop or its arbiter was never scheduled; \
                     a drop count means the hop shed onto a full recorder mailbox",
                    on_wire.load(std::sync::atomic::Ordering::Relaxed),
                    fanout_drops(&room),
                ));
            }
            actix_rt::time::sleep(POLL).await;
        }

        // Drops would truncate the sequence, so the ordering check below would
        // compare a prefix and could pass while reordering.
        let shed = fanout_drop_total(&room) - drops_before;
        if shed != 0.0 {
            return Err(format!(
                "the fan-out hop shed {shed} packets onto the recorder's mailbox \
                 ({}), so this run measured backpressure, not order; MAILBOX_SLOTS \
                 is undersized",
                fanout_drops(&room),
            ));
        }

        let observed = seen.lock().expect("recorder must not be poisoned").clone();
        let expected: Vec<u32> = (0..PACKETS).collect();
        if observed != expected {
            return Err(format!(
                "one publisher's packets must reach the receiver's actor in publish \
                 order across the arbiter hop; NATS orders per subject, the session's \
                 loop is one sequential task, and the actor mailbox is FIFO\n  \
                 observed: {observed:?}"
            ));
        }
        Ok(())
    }
    .await;

    wire_task.abort();
    let _ = wire_task.await;
    let _ = release_tx.send(());
    shards.stop();

    if let Err(message) = outcome {
        panic!("{message}");
    }
}
