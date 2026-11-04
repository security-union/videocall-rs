//! #3277: many peers created in one task boot through the production `ensure_peer` path while
//! the tab's wasm memory budget is nearly full. Stub workers take as many wasm memories as the
//! real ones (decoder 1, NetEq 2) and fail to start the same way when one is refused.

use super::super::worker_boot_queue::MAX_CONCURRENT_WORKER_BOOTS;
use super::*;
use crate::decode::WorkerBootState;
use std::collections::{BTreeMap, BTreeSet};
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::wasm_bindgen_test;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen(inline_js = r#"
// No maximum, as the app's worker wasm declares.
const MEMORY_WASM = "[0,97,115,109,1,0,0,0,5,3,1,0,18,7,7,1,3,109,101,109,2,0]";

// `pauseMs`: after the first memory, wait up to this long (random) before the rest.
// `readyJson` null: the worker never reports ready.
// `readyDelayMs`: wait this long after the last memory before reporting ready.
export function install_stub_worker(linkId, memories, readyJson, pauseMs, readyDelayMs) {
  let link = document.getElementById(linkId);
  if (!link) {
    link = document.createElement("link");
    link.id = linkId;
    document.head.appendChild(link);
  }
  const previous = link.getAttribute("href");
  const src = `
const bytes = new Uint8Array(${MEMORY_WASM});
const held = [];
(async () => {
  for (let i = 0; i < ${memories}; i++) {
    held.push((await WebAssembly.instantiate(bytes)).instance);
    if (i === 0 && ${pauseMs} > 0) await new Promise((r) => setTimeout(r, Math.random() * ${pauseMs}));
  }
  self.onmessage = () => {};
  if (${readyDelayMs} > 0) await new Promise((r) => setTimeout(r, ${readyDelayMs}));
  if (${readyJson} !== null) self.postMessage(${readyJson});
})().catch((e) => setTimeout(() => { throw e; }));`;
  link.setAttribute("href", URL.createObjectURL(new Blob([src], { type: "text/javascript" })));
  return previous;
}

export function restore_worker_link(linkId, previous) {
  const link = document.getElementById(linkId);
  if (!link) return;
  if (previous === undefined || previous === null) link.remove();
  else link.setAttribute("href", previous);
}

// A wasm panic aborts without unwinding, so a failed test cannot release its filler; the next
// fill releases it instead.
export function release_wasm_memories() {
  if (globalThis.__bootBudgetFiller) globalThis.__bootBudgetFiller.terminate();
  globalThis.__bootBudgetFiller = undefined;
}

// Fill the tab's budget in a probe worker, terminate it, then hold all but `leave` memories.
export async function hold_all_wasm_memories_but(leave) {
  release_wasm_memories();
  const src = `
const bytes = new Uint8Array(${MEMORY_WASM});
const held = [];
self.onmessage = async (e) => {
  while (e.data < 0 || held.length < e.data) {
    try { held.push((await WebAssembly.instantiate(bytes)).instance); } catch { break; }
  }
  self.postMessage(held.length);
};`;
  const url = URL.createObjectURL(new Blob([src], { type: "text/javascript" }));
  const run = (want) => new Promise((resolve) => {
    const w = new Worker(url);
    w.onmessage = (e) => resolve([w, e.data]);
    w.postMessage(want);
  });
  const [probe, total] = await run(-1);
  probe.terminate();
  const [filler, held] = await run(total - leave);
  globalThis.__bootBudgetFiller = filler;
  if (held !== total - leave) {
    release_wasm_memories();
    throw new Error(`held ${held} of ${total - leave} wasm memories`);
  }
}

export function block_main_thread(ms) {
  const end = performance.now() + ms;
  while (performance.now() < end) {}
}

export function track_live_workers() {
  const Original = globalThis.Worker;
  let live = 0;
  globalThis.Worker = class extends Original {
    constructor(...args) {
      super(...args);
      live += 1;
      let ended = false;
      const terminate = this.terminate.bind(this);
      this.terminate = () => { if (!ended) { ended = true; live -= 1; } return terminate(); };
    }
  };
  return { live: () => live, restore: () => { globalThis.Worker = Original; } };
}

export function count_workers_created() {
  const Original = globalThis.Worker;
  let created = 0;
  globalThis.Worker = class extends Original {
    constructor(...args) {
      super(...args);
      created += 1;
    }
  };
  return () => {
    globalThis.Worker = Original;
    return created;
  };
}
"#)]
extern "C" {
    fn install_stub_worker(
        link_id: &str,
        memories: u32,
        ready_json: &str,
        pause_ms: u32,
        ready_delay_ms: u32,
    ) -> Option<String>;
    fn restore_worker_link(link_id: &str, previous: Option<String>);
    fn hold_all_wasm_memories_but(leave: u32) -> js_sys::Promise;
    fn release_wasm_memories();
    fn count_workers_created() -> js_sys::Function;
    fn track_live_workers() -> JsValue;
    fn block_main_thread(ms: u32);
}

const DECODER_MEMORIES: u32 = 1;
const NETEQ_MEMORIES: u32 = 2;
/// Created in this order; the talkers are created LAST so arrival order alone cannot favour them.
const PEERS: std::ops::RangeInclusive<u64> = 1001..=1008;
const TALKERS: [u64; 2] = [1007, 1008];
/// Room for exactly four NetEq workers and one decoder.
const NETEQ_THAT_FIT: u32 = 4;
const DECODERS_THAT_FIT: u32 = 1;
const FREE_MEMORIES: u32 = NETEQ_THAT_FIT * NETEQ_MEMORIES + DECODERS_THAT_FIT * DECODER_MEMORIES;

struct StubWorkers {
    previous: [(&'static str, Option<String>); 2],
}

const DECODER_READY: &str = r#"{ kind: "worker_ready" }"#;
const NETEQ_READY: &str = r#"{ type: "workerReady", mute_state: true }"#;

impl StubWorkers {
    fn install() -> Self {
        Self::with(DECODER_READY, NETEQ_READY, 0)
    }

    fn with(decoder_ready: &str, neteq_ready: &str, neteq_pause_ms: u32) -> Self {
        Self::with_ready_delay(decoder_ready, neteq_ready, neteq_pause_ms, 0)
    }

    fn with_ready_delay(
        decoder_ready: &str,
        neteq_ready: &str,
        neteq_pause_ms: u32,
        neteq_ready_delay_ms: u32,
    ) -> Self {
        Self {
            previous: [
                (
                    "codecs-worker",
                    install_stub_worker("codecs-worker", DECODER_MEMORIES, decoder_ready, 0, 0),
                ),
                (
                    "neteq-worker",
                    install_stub_worker(
                        "neteq-worker",
                        NETEQ_MEMORIES,
                        neteq_ready,
                        neteq_pause_ms,
                        neteq_ready_delay_ms,
                    ),
                ),
            ],
        }
    }
}

impl Drop for StubWorkers {
    fn drop(&mut self) {
        for (id, previous) in &mut self.previous {
            restore_worker_link(id, previous.take());
        }
    }
}

fn audio_packet(session_id: u64) -> PacketWrapper {
    let media = MediaPacket {
        media_type: MediaType::AUDIO.into(),
        user_id: b"peer@test.com".to_vec(),
        data: vec![0u8; 10],
        ..Default::default()
    };
    PacketWrapper {
        data: media.write_to_bytes().expect("serialize MediaPacket"),
        user_id: "peer@test.com".into(),
        packet_type: PacketType::MEDIA.into(),
        session_id,
        ..Default::default()
    }
}

/// Audio and camera; a screen decoder exists only once its peer shares (#3210).
type States = BTreeMap<u64, [WorkerBootState; 2]>;

fn states(manager: &PeerDecodeManager) -> States {
    manager
        .connected_peers
        .ordered_keys()
        .iter()
        .map(|sid| {
            let peer = manager.connected_peers.get(sid).expect("listed peer");
            (
                *sid,
                [
                    peer.audio.boot_state_for_test(),
                    peer.video.boot_state_for_test(),
                ],
            )
        })
        .collect()
}

fn settled(state: WorkerBootState) -> bool {
    matches!(state, WorkerBootState::Ready | WorkerBootState::Failed)
}

struct Settled {
    states: States,
    most_booting_at_once: usize,
    live_screen_decoders: usize,
}

async fn wait_until_audio_and_video_settle(manager: &PeerDecodeManager) -> Settled {
    let mut most_booting_at_once = 0;
    for _ in 0..3000 {
        let now = states(manager);
        let booting = now
            .values()
            .flatten()
            .filter(|s| **s == WorkerBootState::Booting)
            .count();
        most_booting_at_once = most_booting_at_once.max(booting);
        if now.values().all(|[a, v]| settled(*a) && settled(*v)) {
            let live_screen_decoders = now
                .keys()
                .filter(|sid| manager.screen_decoder_live_for_test(**sid))
                .count();
            return Settled {
                states: now,
                most_booting_at_once,
                live_screen_decoders,
            };
        }
        gloo_timers::future::TimeoutFuture::new(5).await;
    }
    panic!(
        "boots never settled at {}: {:?} {}",
        js_sys::Date::now(),
        states(manager),
        manager.boot_queue.debug_state()
    );
}

fn with(states: &States, slot: usize, wanted: WorkerBootState) -> BTreeSet<u64> {
    states
        .iter()
        .filter(|(_, s)| s[slot] == wanted)
        .map(|(sid, _)| *sid)
        .collect()
}

struct BudgetFiller;

impl Drop for BudgetFiller {
    fn drop(&mut self) {
        release_wasm_memories();
    }
}

/// Every peer of [`PEERS`] is created, and the talkers' first audio arrives, in ONE task.
async fn burst_into_a_full_tab(queue: Option<WorkerBootQueue>) -> Settled {
    let _stubs = StubWorkers::install();
    JsFuture::from(hold_all_wasm_memories_but(FREE_MEMORIES))
        .await
        .expect("fill the tab's wasm budget");
    let _filler = BudgetFiller;
    let mut manager = PeerDecodeManager::new();
    if let Some(queue) = queue {
        manager.boot_queue = queue;
    }
    for sid in PEERS {
        assert!(matches!(
            manager.ensure_peer(sid, "peer@test.com"),
            PeerStatus::Added(_)
        ));
    }
    for sid in TALKERS {
        manager
            .decode(audio_packet(sid), "local@test.com")
            .expect("talker audio decodes");
    }
    wait_until_audio_and_video_settle(&manager).await
}

#[wasm_bindgen_test]
async fn one_at_a_time_a_full_tab_keeps_talkers_and_fails_only_the_tail_of_the_queue() {
    let Settled {
        states,
        live_screen_decoders,
        ..
    } = burst_into_a_full_tab(Some(WorkerBootQueue::new(1))).await;

    assert_eq!(
        with(&states, 0, WorkerBootState::Ready),
        BTreeSet::from([1007, 1008, 1001, 1002]),
        "audio first, talkers ahead of arrival order, then the earliest arrivals: {states:?}"
    );
    assert_eq!(
        with(&states, 0, WorkerBootState::Failed),
        (1003..=1006).collect::<BTreeSet<_>>()
    );
    assert_eq!(
        with(&states, 1, WorkerBootState::Ready),
        BTreeSet::from([1001]),
        "video boots only after every NetEq: {states:?}"
    );
    let complete: BTreeSet<u64> = states
        .iter()
        .filter(|(_, [a, v])| *a == WorkerBootState::Ready && *v == WorkerBootState::Ready)
        .map(|(sid, _)| *sid)
        .collect();
    assert_eq!(complete, BTreeSet::from([1001]));
    assert_eq!(
        live_screen_decoders, 0,
        "no peer is sharing, so no screen worker starts"
    );
}

#[wasm_bindgen_test]
async fn at_the_production_bound_a_burst_boots_at_most_the_bound_and_keeps_talkers() {
    let Settled {
        states,
        most_booting_at_once,
        live_screen_decoders,
    } = burst_into_a_full_tab(None).await;

    assert!(most_booting_at_once >= 1);
    assert!(
        most_booting_at_once <= MAX_CONCURRENT_WORKER_BOOTS,
        "{most_booting_at_once} workers booted at once"
    );
    for talker in TALKERS {
        assert_eq!(states[&talker][0], WorkerBootState::Ready, "{states:?}");
    }
    assert_eq!(live_screen_decoders, 0);
}

#[wasm_bindgen_test]
async fn a_peer_removed_while_queued_never_starts_a_worker() {
    let _stubs = StubWorkers::install();
    let mut manager = PeerDecodeManager::new();
    manager.boot_queue = WorkerBootQueue::new(1);
    let created = count_workers_created();
    for sid in [2001, 2002, 2003] {
        manager.ensure_peer(sid, "peer@test.com");
    }
    manager.delete_peer(2003);
    assert_eq!(
        manager.boot_queue.counts(),
        (0, 4),
        "2003's two entries left the queue"
    );

    let states = wait_until_audio_and_video_settle(&manager).await.states;
    let created = created.call0(&JsValue::NULL).expect("count").as_f64();

    assert_eq!(states.keys().copied().collect::<Vec<_>>(), vec![2001, 2002]);
    assert_eq!(
        created,
        Some(4.0),
        "audio and video for the two remaining peers only: {states:?}"
    );
}

#[wasm_bindgen_test]
async fn clearing_every_peer_while_queued_starts_nothing() {
    let _stubs = StubWorkers::install();
    let mut manager = PeerDecodeManager::new();
    let created = count_workers_created();
    for sid in 3001..=3008 {
        manager.ensure_peer(sid, "peer@test.com");
    }
    manager.clear_all_peers();
    gloo_timers::future::TimeoutFuture::new(50).await;
    let created = created.call0(&JsValue::NULL).expect("count").as_f64();

    assert_eq!(manager.boot_queue.counts(), (0, 0));
    assert_eq!(created, Some(0.0));
}

#[wasm_bindgen_test]
async fn cameras_wait_until_every_booting_neteq_has_settled() {
    let _stubs = StubWorkers::with(DECODER_READY, NETEQ_READY, 400);
    JsFuture::from(hold_all_wasm_memories_but(4 * NETEQ_MEMORIES))
        .await
        .expect("fill the tab's wasm budget");
    let _filler = BudgetFiller;
    let mut manager = PeerDecodeManager::new();
    for sid in 4001..=4004 {
        manager.ensure_peer(sid, "peer@test.com");
    }
    let states = wait_until_audio_and_video_settle(&manager).await.states;

    assert_eq!(
        with(&states, 0, WorkerBootState::Ready).len(),
        4,
        "every NetEq fits when no camera boots beside it: {states:?}"
    );
}

#[wasm_bindgen_test]
async fn an_expired_boot_is_terminated_so_live_workers_stay_within_the_bound() {
    let _stubs = StubWorkers::with("null", "null", 0);
    let tracker = track_live_workers();
    let live = js_sys::Reflect::get(&tracker, &"live".into())
        .expect("live")
        .unchecked_into::<js_sys::Function>();
    let manager = Rc::new(RefCell::new(PeerDecodeManager::new()));
    let queue = WorkerBootQueue::with_timeout(2, 60.0, 20.0);
    manager.borrow_mut().boot_queue = queue.clone();
    install_boot_retry_driver(
        &queue,
        Rc::downgrade(&manager),
        |m: &mut PeerDecodeManager| m,
    );
    // Four peers: camera boots time out with more camera boots queued behind them.
    for sid in 5001..=5004 {
        manager.borrow_mut().ensure_peer(sid, "peer@test.com");
    }
    let mut most_live = 0.0f64;
    for _ in 0..600 {
        let now = live
            .call0(&JsValue::NULL)
            .expect("live")
            .as_f64()
            .unwrap_or(-1.0);
        most_live = most_live.max(now);
        let done = states(&manager.borrow())
            .values()
            .all(|[a, v]| *a == WorkerBootState::Failed && *v == WorkerBootState::Failed);
        if done {
            break;
        }
        gloo_timers::future::TimeoutFuture::new(10).await;
    }
    let restore = js_sys::Reflect::get(&tracker, &"restore".into())
        .expect("restore")
        .unchecked_into::<js_sys::Function>();
    restore.call0(&JsValue::NULL).expect("restore Worker");
    let final_states = states(&manager.borrow());

    assert!(
        final_states
            .values()
            .all(|[a, v]| *a == WorkerBootState::Failed && *v == WorkerBootState::Failed),
        "every timed-out boot settles as failed: {final_states:?}"
    );
    assert!(most_live >= 1.0);
    assert!(most_live <= 2.0, "{most_live} workers alive at once");
}

/// A stalled event loop must not count as boot time: the NetEq worker reports ready 600 ms after
/// it starts, the main thread stalls for the first 550 ms, and the timeout is 450 ms.
#[wasm_bindgen_test]
async fn a_main_thread_stall_does_not_count_towards_the_boot_timeout() {
    let _stubs = StubWorkers::with_ready_delay(DECODER_READY, NETEQ_READY, 0, 600);
    let mut manager = PeerDecodeManager::new();
    manager.boot_queue = WorkerBootQueue::with_timeout(2, 450.0, 20.0);
    manager.ensure_peer(6001, "peer@test.com");
    for _ in 0..3 {
        gloo_timers::future::TimeoutFuture::new(0).await;
    }
    block_main_thread(550);
    let states = wait_until_audio_and_video_settle(&manager).await.states;

    assert_eq!(
        states[&6001][..2],
        [WorkerBootState::Ready, WorkerBootState::Ready],
        "{states:?}"
    );
}

#[wasm_bindgen_test]
async fn a_neteq_boot_that_timed_out_is_rebuilt_on_the_next_packet() {
    let hanging = StubWorkers::with(DECODER_READY, "null", 0);
    let mut manager = PeerDecodeManager::new();
    manager.boot_queue = WorkerBootQueue::with_timeout(4, 60.0, 20.0);
    manager.ensure_peer(8001, "peer@test.com");
    drop(hanging);
    let _healthy = StubWorkers::install();
    for _ in 0..300 {
        if states(&manager)[&8001][0] == WorkerBootState::Failed {
            break;
        }
        gloo_timers::future::TimeoutFuture::new(10).await;
    }
    assert_eq!(states(&manager)[&8001][0], WorkerBootState::Failed);

    manager
        .decode(audio_packet(8001), "local@test.com")
        .expect("audio decodes");
    for _ in 0..300 {
        if states(&manager)[&8001][0] == WorkerBootState::Ready {
            break;
        }
        gloo_timers::future::TimeoutFuture::new(10).await;
    }
    assert_eq!(states(&manager)[&8001][0], WorkerBootState::Ready);
}

/// The rebuild is bounded: a NetEq that never starts is retried until the peer's audio budget is
/// spent, and then the camera boots.
#[wasm_bindgen_test]
async fn a_neteq_that_never_starts_is_retried_until_its_audio_budget_is_spent() {
    let _stubs = StubWorkers::with(DECODER_READY, "null", 0);
    let created = count_workers_created();
    let mut manager = PeerDecodeManager::new();
    manager.boot_queue = WorkerBootQueue::with_timeout(4, 60.0, 20.0);
    manager.ensure_peer(8101, "peer@test.com");
    for _ in 0..300 {
        let _ = manager.decode(audio_packet(8101), "local@test.com");
        if states(&manager)[&8101][1] == WorkerBootState::Ready {
            break;
        }
        gloo_timers::future::TimeoutFuture::new(10).await;
    }
    for _ in 0..10 {
        let _ = manager.decode(audio_packet(8101), "local@test.com");
        gloo_timers::future::TimeoutFuture::new(10).await;
    }
    let created = created.call0(&JsValue::NULL).expect("count").as_f64();

    // One camera decoder, the NetEq boot and the one retry the budget leaves room for.
    assert_eq!(created, Some(3.0));
    assert_eq!(states(&manager)[&8101][0], WorkerBootState::Failed);
    assert_eq!(
        states(&manager)[&8101][1],
        WorkerBootState::Ready,
        "the camera boots once the audio budget is spent"
    );
}

#[wasm_bindgen_test]
async fn a_timed_out_neteq_is_retried_without_another_packet() {
    let hanging = StubWorkers::with(DECODER_READY, "null", 0);
    JsFuture::from(hold_all_wasm_memories_but(NETEQ_MEMORIES))
        .await
        .expect("fill budget");
    let _filler = BudgetFiller;
    let manager = Rc::new(RefCell::new(PeerDecodeManager::new()));
    let queue = WorkerBootQueue::with_timeout(4, 60.0, 20.0);
    manager.borrow_mut().boot_queue = queue.clone();
    install_boot_retry_driver(
        &queue,
        Rc::downgrade(&manager),
        |m: &mut PeerDecodeManager| m,
    );
    manager.borrow_mut().ensure_peer(8231, "peer@test.com");
    drop(hanging);
    let _healthy = StubWorkers::install();

    let mut now = states(&manager.borrow());
    for _ in 0..400 {
        if now[&8231][0] == WorkerBootState::Ready && settled(now[&8231][1]) {
            break;
        }
        gloo_timers::future::TimeoutFuture::new(10).await;
        now = states(&manager.borrow());
    }
    assert_eq!(now[&8231][0], WorkerBootState::Ready, "{now:?}");
}

#[wasm_bindgen_test]
#[allow(clippy::await_holding_refcell_ref)] // the held borrow is the busy client under test
async fn a_retry_the_ticker_could_not_queue_is_queued_on_a_later_tick() {
    let hanging = StubWorkers::with(DECODER_READY, "null", 0);
    let manager = Rc::new(RefCell::new(PeerDecodeManager::new()));
    let queue = WorkerBootQueue::with_timeout(4, 60.0, 20.0);
    manager.borrow_mut().boot_queue = queue.clone();
    install_boot_retry_driver(
        &queue,
        Rc::downgrade(&manager),
        |m: &mut PeerDecodeManager| m,
    );
    manager.borrow_mut().ensure_peer(8251, "peer@test.com");
    drop(hanging);
    let _healthy = StubWorkers::install();
    {
        let busy = manager.borrow();
        for _ in 0..300 {
            if states(&busy)[&8251][0] == WorkerBootState::Failed {
                break;
            }
            gloo_timers::future::TimeoutFuture::new(10).await;
        }
        gloo_timers::future::TimeoutFuture::new(100).await;
        assert_eq!(states(&busy)[&8251][0], WorkerBootState::Failed);
    }

    let mut now = states(&manager.borrow());
    for _ in 0..300 {
        if now[&8251][0] == WorkerBootState::Ready {
            break;
        }
        gloo_timers::future::TimeoutFuture::new(10).await;
        now = states(&manager.borrow());
    }
    assert_eq!(now[&8251][0], WorkerBootState::Ready, "{now:?}");
}

#[wasm_bindgen_test]
async fn a_departed_peers_audio_hold_does_not_block_other_cameras() {
    let hanging = StubWorkers::with(DECODER_READY, "null", 0);
    let mut manager = PeerDecodeManager::new();
    manager.boot_queue = WorkerBootQueue::with_timeout(4, 60.0, 20.0);
    manager.ensure_peer(8241, "gone@test.com");
    for _ in 0..300 {
        if states(&manager)[&8241][0] == WorkerBootState::Failed {
            break;
        }
        gloo_timers::future::TimeoutFuture::new(10).await;
    }
    assert_eq!(states(&manager)[&8241][0], WorkerBootState::Failed);
    drop(hanging);
    let _healthy = StubWorkers::install();

    manager.delete_peer(8241);
    manager.ensure_peer(8242, "talker@test.com");
    manager
        .decode(audio_packet(8242), "local@test.com")
        .expect("audio decodes");
    let settled = wait_until_audio_and_video_settle(&manager).await.states;
    assert_eq!(
        settled[&8242][..2],
        [WorkerBootState::Ready, WorkerBootState::Ready],
        "{settled:?}"
    );
}

#[wasm_bindgen_test]
async fn a_camera_does_not_take_the_memory_a_timed_out_neteq_retry_needs() {
    let hanging = StubWorkers::with(DECODER_READY, "null", 0);
    JsFuture::from(hold_all_wasm_memories_but(NETEQ_MEMORIES))
        .await
        .expect("fill budget");
    let _filler = BudgetFiller;
    let mut manager = PeerDecodeManager::new();
    manager.boot_queue = WorkerBootQueue::with_timeout(4, 300.0, 20.0);
    manager.boot_queue.set_aging_for_test(100.0);
    manager.ensure_peer(8201, "peer@test.com");
    for _ in 0..1000 {
        if states(&manager)[&8201][0] == WorkerBootState::Failed {
            break;
        }
        gloo_timers::future::TimeoutFuture::new(10).await;
    }
    assert_eq!(states(&manager)[&8201][0], WorkerBootState::Failed);
    drop(hanging);
    let _healthy = StubWorkers::install();

    manager
        .decode(audio_packet(8201), "local@test.com")
        .expect("audio decodes");
    let settled = wait_until_audio_and_video_settle(&manager).await.states;
    assert_eq!(settled[&8201][0], WorkerBootState::Ready, "{settled:?}");
}

#[wasm_bindgen_test]
async fn a_timed_out_neteq_is_retried_on_another_peers_packet() {
    let hanging = StubWorkers::with(DECODER_READY, "null", 0);
    let mut manager = PeerDecodeManager::new();
    manager.boot_queue = WorkerBootQueue::with_timeout(4, 60.0, 20.0);
    manager.ensure_peer(8221, "quiet@test.com");
    for _ in 0..300 {
        if states(&manager)[&8221][0] == WorkerBootState::Failed {
            break;
        }
        gloo_timers::future::TimeoutFuture::new(10).await;
    }
    assert_eq!(states(&manager)[&8221][0], WorkerBootState::Failed);
    drop(hanging);
    let _healthy = StubWorkers::install();

    manager.ensure_peer(8222, "talker@test.com");
    manager
        .decode(audio_packet(8222), "local@test.com")
        .expect("audio decodes");
    let settled = wait_until_audio_and_video_settle(&manager).await.states;
    assert_eq!(settled[&8221][0], WorkerBootState::Ready, "{settled:?}");
}

#[wasm_bindgen_test]
async fn a_host_muted_peer_does_not_take_the_last_audio_memory_from_a_talker() {
    let _stubs = StubWorkers::install();
    JsFuture::from(hold_all_wasm_memories_but(NETEQ_MEMORIES))
        .await
        .expect("fill budget");
    let _filler = BudgetFiller;
    let mut manager = PeerDecodeManager::new();
    manager.boot_queue = WorkerBootQueue::new(1);
    for (sid, uid) in [(8211, "muted@test.com"), (8212, "talker@test.com")] {
        manager.ensure_peer(sid, uid);
        let heartbeat = MediaPacket {
            media_type: MediaType::HEARTBEAT.into(),
            user_id: uid.into(),
            heartbeat_metadata: Some(videocall_types::protos::media_packet::HeartbeatMetadata {
                audio_enabled: true,
                is_speaking: true,
                ..Default::default()
            })
            .into(),
            ..Default::default()
        };
        manager
            .decode(
                PacketWrapper {
                    session_id: sid,
                    user_id: uid.into(),
                    packet_type: PacketType::MEDIA.into(),
                    data: heartbeat.write_to_bytes().expect("heartbeat encodes"),
                    ..Default::default()
                },
                "local@test.com",
            )
            .expect("heartbeat decodes");
    }
    manager.force_peer_media_off("muted@test.com", true, false);

    let settled = wait_until_audio_and_video_settle(&manager).await.states;
    assert_eq!(settled[&8212][0], WorkerBootState::Ready, "{settled:?}");
}
