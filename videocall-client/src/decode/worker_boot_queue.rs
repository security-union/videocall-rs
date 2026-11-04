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

//! Per-peer decoder workers boot through one bounded, prioritised queue (#3277).
//!
//! At most [`MAX_CONCURRENT_WORKER_BOOTS`] workers boot at once; the next one starts when a
//! booting worker reports ready or failed, or when its decoder is dropped. A screen decoder
//! boots only while its peer is sharing.
// The pump runs only in the browser; the host build keeps the policy for its unit tests.
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use videocall_codecs::decoder::BootSettled;

pub(crate) const MAX_CONCURRENT_WORKER_BOOTS: usize = 4;

/// A decoder whose boot timed out is rebuilt through the queue at most this many times per kind
/// for one peer.
pub(crate) const MAX_BOOT_RETRIES: u8 = 2;

/// A boot still unsettled after this much responsive time is terminated as a failed boot, so a
/// worker that never answers neither wedges the queue nor keeps booting beside the bound.
pub(crate) const BOOT_SLOT_TIMEOUT_MS: f64 = 30_000.0;

/// While boots are in flight the expiry check ticks this often. Each tick credits a boot at most
/// two periods, so a stalled event loop barely counts towards the timeout.
pub(crate) const BOOT_EXPIRY_TICK_MS: f64 = 1_000.0;

/// An entry queued this long goes ahead of every entry that arrived in a later arrival bucket,
/// so later arrivals cannot starve it.
pub(crate) const BOOT_AGING_MS: f64 = 5_000.0;

/// Aged entries are served in arrival order at this granularity; inside one bucket the media
/// priority still holds, so one burst of peers keeps audio first even once it has aged.
pub(crate) const AGED_ARRIVAL_BUCKET_MS: f64 = 1_000.0;

/// A camera or screen entry at the head that waits for in-flight audio lets at most this many
/// later audio boots past it, then holds its place until the audio in flight settles.
pub(crate) const BLOCKED_HEAD_OVERTAKES: u32 = MAX_CONCURRENT_WORKER_BOOTS as u32;

/// Credited boot time that one peer's audio attempts, first and retries together, may use. Each
/// attempt is terminated at the slot timeout or the budget left and charged that limit.
pub(crate) const AUDIO_BOOT_BUDGET_MS: f64 = 40_000.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BootKind {
    Audio,
    Video,
    Screen,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PeerBootHints {
    pub speaking: bool,
    pub audio_on: bool,
    pub visible: bool,
    pub video_on: bool,
    pub screen_on: bool,
}

/// Lower boots first; `None` is not eligible yet.
pub(crate) fn boot_rank(kind: BootKind, hints: PeerBootHints) -> Option<[u8; 4]> {
    let later = |x: bool| u8::from(!x);
    match kind {
        BootKind::Audio => Some([
            0,
            later(hints.speaking),
            later(hints.audio_on),
            later(hints.visible),
        ]),
        BootKind::Screen => hints.screen_on.then(|| [1, 0, later(hints.visible), 0]),
        BootKind::Video => Some([
            1,
            1 + later(hints.visible),
            later(hints.video_on),
            later(hints.speaking),
        ]),
    }
}

pub(crate) type BootStarter = Box<dyn Fn() -> bool>;
pub(crate) type BootAbort = Box<dyn Fn()>;

/// `start` creates the worker (`false` once the decoder is gone); `abort` terminates a worker
/// that has not settled and settles it as failed.
pub(crate) struct BootHandle {
    pub(crate) start: BootStarter,
    pub(crate) abort: BootAbort,
}

struct QueuedBoot {
    id: u64,
    session_id: u64,
    kind: BootKind,
    hints: Rc<Cell<PeerBootHints>>,
    enqueued_ms: f64,
    /// Boot time the peer's earlier audio attempts used.
    spent_ms: f64,
    overtaken: u32,
    handle: BootHandle,
}

struct ActiveBoot {
    id: u64,
    session_id: u64,
    kind: BootKind,
    enqueued_ms: f64,
    spent_ms: f64,
    credited_ms: f64,
    last_credit_ms: f64,
    abort: BootAbort,
}

/// A peer whose audio boot timed out: camera and screen boots wait until its retry is queued or
/// the peer's owner ends the hold because no retry will come.
struct AudioHold {
    session_id: u64,
    /// When the timed-out boot was queued; its retry is queued as of this time.
    enqueued_ms: f64,
    spent_ms: f64,
}

/// The queue's state, free of browser APIs so the policy is host-testable.
pub(crate) struct BootQueueCore {
    limit: usize,
    timeout_ms: f64,
    tick_ms: f64,
    audio_budget_ms: f64,
    aging_ms: f64,
    queued: Vec<QueuedBoot>,
    active: Vec<ActiveBoot>,
    audio_holds: Vec<AudioHold>,
    closed: bool,
}

impl BootQueueCore {
    #[cfg(test)]
    pub(crate) fn new(limit: usize) -> Self {
        Self::with_timeout(limit, BOOT_SLOT_TIMEOUT_MS, BOOT_EXPIRY_TICK_MS)
    }

    pub(crate) fn with_timeout(limit: usize, timeout_ms: f64, tick_ms: f64) -> Self {
        Self {
            limit,
            timeout_ms,
            tick_ms,
            audio_budget_ms: timeout_ms * (AUDIO_BOOT_BUDGET_MS / BOOT_SLOT_TIMEOUT_MS),
            aging_ms: BOOT_AGING_MS,
            queued: Vec::new(),
            active: Vec::new(),
            audio_holds: Vec::new(),
            closed: false,
        }
    }

    pub(crate) fn push(
        &mut self,
        id: u64,
        session_id: u64,
        kind: BootKind,
        hints: Rc<Cell<PeerBootHints>>,
        now_ms: f64,
        handle: BootHandle,
    ) {
        if self.closed {
            return;
        }
        let (mut enqueued_ms, mut spent_ms) = (now_ms, 0.0);
        if kind == BootKind::Audio {
            if let Some(index) = self
                .audio_holds
                .iter()
                .position(|h| h.session_id == session_id)
            {
                let hold = self.audio_holds.swap_remove(index);
                (enqueued_ms, spent_ms) = (hold.enqueued_ms, hold.spent_ms);
            }
        }
        self.queued.push(QueuedBoot {
            id,
            session_id,
            kind,
            hints,
            enqueued_ms,
            spent_ms,
            overtaken: 0,
            handle,
        });
    }

    /// Credits each in-flight boot with the time since its last credit, capped at two ticks,
    /// and removes the boots that reached the timeout or, for audio, the peer's audio budget; the
    /// caller must run each abort before it admits another boot. A timed-out audio boot with
    /// budget left holds camera and screen boots back until its retry is queued.
    pub(crate) fn take_expired(&mut self, now_ms: f64) -> Vec<(u64, BootAbort)> {
        let cap = 2.0 * self.tick_ms;
        let mut expired = Vec::new();
        let mut index = 0;
        while index < self.active.len() {
            let boot = &mut self.active[index];
            boot.credited_ms += (now_ms - boot.last_credit_ms).clamp(0.0, cap);
            boot.last_credit_ms = now_ms;
            let audio = boot.kind == BootKind::Audio;
            let budget_left = if audio {
                self.audio_budget_ms - boot.spent_ms
            } else {
                f64::INFINITY
            };
            if boot.credited_ms >= self.timeout_ms.min(budget_left) {
                let boot = self.active.swap_remove(index);
                if audio && self.timeout_ms < budget_left {
                    self.audio_holds.push(AudioHold {
                        session_id: boot.session_id,
                        enqueued_ms: boot.enqueued_ms,
                        spent_ms: boot.spent_ms + self.timeout_ms,
                    });
                }
                expired.push((boot.id, boot.abort));
            } else {
                index += 1;
            }
        }
        expired
    }

    /// Marks the best eligible entry active and hands back its starter, if a slot is free. A
    /// camera or screen boot waits while an audio boot runs or a timed-out one awaits its retry.
    pub(crate) fn take_next(&mut self, now_ms: f64) -> Option<(u64, BootStarter)> {
        if self.closed || self.active.len() >= self.limit {
            return None;
        }
        let key = |boot: &QueuedBoot| {
            let rank = boot_rank(boot.kind, boot.hints.get())?;
            let order = if now_ms - boot.enqueued_ms >= self.aging_ms {
                (0, (boot.enqueued_ms / AGED_ARRIVAL_BUCKET_MS) as u64, rank)
            } else {
                (1, 0, rank)
            };
            Some((order, boot.id))
        };
        let best = |audio_only: bool| {
            self.queued
                .iter()
                .enumerate()
                .filter(|(_, boot)| !audio_only || boot.kind == BootKind::Audio)
                .filter_map(|(index, boot)| Some((key(boot)?, index)))
                .min()
                .map(|(_, index)| index)
        };
        let head = best(false)?;
        let audio_in_flight = self.active.iter().any(|boot| boot.kind == BootKind::Audio)
            || !self.audio_holds.is_empty();
        let index = if audio_in_flight && self.queued[head].kind != BootKind::Audio {
            if self.queued[head].overtaken >= BLOCKED_HEAD_OVERTAKES {
                return None;
            }
            let audio = best(true)?;
            self.queued[head].overtaken += 1;
            audio
        } else {
            head
        };
        let boot = self.queued.swap_remove(index);
        self.active.push(ActiveBoot {
            id: boot.id,
            session_id: boot.session_id,
            kind: boot.kind,
            enqueued_ms: boot.enqueued_ms,
            spent_ms: boot.spent_ms,
            credited_ms: 0.0,
            last_credit_ms: now_ms,
            abort: boot.handle.abort,
        });
        Some((boot.id, boot.handle.start))
    }

    /// Frees `id`'s slot or drops it from the queue; `false` if it held neither.
    pub(crate) fn release(&mut self, id: u64) -> bool {
        if let Some(index) = self.active.iter().position(|boot| boot.id == id) {
            self.active.swap_remove(index);
            return true;
        }
        if let Some(index) = self.queued.iter().position(|boot| boot.id == id) {
            self.queued.swap_remove(index);
            return true;
        }
        false
    }

    pub(crate) fn active_len(&self) -> usize {
        self.active.len()
    }

    pub(crate) fn queued_len(&self) -> usize {
        self.queued.len()
    }

    pub(crate) fn has_queued_for(&self, session_id: u64) -> bool {
        self.queued.iter().any(|boot| boot.session_id == session_id)
    }

    /// Peers whose timed-out audio boot still holds back camera and screen boots.
    pub(crate) fn audio_hold_sessions(&self) -> Vec<u64> {
        self.audio_holds
            .iter()
            .map(|hold| hold.session_id)
            .collect()
    }

    pub(crate) fn end_audio_hold(&mut self, session_id: u64) {
        self.audio_holds
            .retain(|hold| hold.session_id != session_id);
    }

    /// Drops every queued boot and starts no more; boots in flight still settle.
    fn close(&mut self) {
        self.closed = true;
        self.audio_holds.clear();
        self.queued.clear();
    }
}

struct Shared {
    core: RefCell<BootQueueCore>,
    next_id: Cell<u64>,
    pump_scheduled: Cell<bool>,
    ticking: Cell<bool>,
    retry_driver: RefCell<Option<Rc<dyn Fn()>>>,
}

/// A client's worker-boot queue, shared by every peer of one [`super::PeerDecodeManager`].
#[derive(Clone)]
pub(crate) struct WorkerBootQueue {
    shared: Rc<Shared>,
}

impl std::fmt::Debug for WorkerBootQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let core = self.shared.core.borrow();
        f.debug_struct("WorkerBootQueue")
            .field("active", &core.active_len())
            .field("queued", &core.queued_len())
            .finish()
    }
}

impl Default for WorkerBootQueue {
    fn default() -> Self {
        Self::new(MAX_CONCURRENT_WORKER_BOOTS)
    }
}

impl WorkerBootQueue {
    pub(crate) fn new(limit: usize) -> Self {
        Self::with_timeout(limit, BOOT_SLOT_TIMEOUT_MS, BOOT_EXPIRY_TICK_MS)
    }

    pub(crate) fn with_timeout(limit: usize, timeout_ms: f64, tick_ms: f64) -> Self {
        Self {
            shared: Rc::new(Shared {
                core: RefCell::new(BootQueueCore::with_timeout(limit, timeout_ms, tick_ms)),
                next_id: Cell::new(1),
                pump_scheduled: Cell::new(false),
                ticking: Cell::new(false),
                retry_driver: RefCell::default(),
            }),
        }
    }

    pub(crate) fn reserve(
        &self,
        session_id: u64,
        kind: BootKind,
        hints: &Rc<Cell<PeerBootHints>>,
    ) -> BootReservation {
        let id = self.shared.next_id.get();
        self.shared.next_id.set(id + 1);
        BootReservation {
            shared: Rc::downgrade(&self.shared),
            id,
            session_id,
            kind,
            hints: hints.clone(),
            guard: Some(SlotGuard {
                shared: Rc::downgrade(&self.shared),
                id,
            }),
        }
    }

    pub(crate) fn has_queued_for(&self, session_id: u64) -> bool {
        self.shared.core.borrow().has_queued_for(session_id)
    }

    pub(crate) fn audio_hold_sessions(&self) -> Vec<u64> {
        self.shared.core.borrow().audio_hold_sessions()
    }

    /// `driver` queues the retry of each timed-out audio boot or ends its hold; the expiry ticker
    /// calls it after an audio boot expires and on every tick while a hold remains.
    pub(crate) fn set_retry_driver(&self, driver: Rc<dyn Fn()>) {
        *self.shared.retry_driver.borrow_mut() = Some(driver);
    }

    /// For a client that is leaving: no queued worker starts after this.
    pub(crate) fn close(&self) {
        self.shared.core.borrow_mut().close();
    }

    /// Lets camera and screen boots past `session_id`'s timed-out audio, e.g. when it will not be
    /// retried.
    pub(crate) fn end_audio_hold(&self, session_id: u64) {
        self.shared.core.borrow_mut().end_audio_hold(session_id);
        schedule_pump(&self.shared);
    }

    pub(crate) fn schedule_pump(&self) {
        schedule_pump(&self.shared);
    }

    #[cfg(test)]
    pub(crate) fn debug_state(&self) -> String {
        let core = self.shared.core.borrow();
        format!(
            "pump_scheduled={} active={:?} queued={:?}",
            self.shared.pump_scheduled.get(),
            core.active
                .iter()
                .map(|a| (a.id, a.credited_ms))
                .collect::<Vec<_>>(),
            core.queued
                .iter()
                .map(|q| (q.id, q.session_id, q.kind, boot_rank(q.kind, q.hints.get())))
                .collect::<Vec<_>>()
        )
    }

    #[cfg(all(test, target_arch = "wasm32"))]
    pub(crate) fn hold_audio_for_test(&self, session_id: u64) {
        self.shared.core.borrow_mut().audio_holds.push(AudioHold {
            session_id,
            enqueued_ms: 0.0,
            spent_ms: 0.0,
        });
    }

    #[cfg(all(test, target_arch = "wasm32"))]
    pub(crate) fn set_aging_for_test(&self, aging_ms: f64) {
        self.shared.core.borrow_mut().aging_ms = aging_ms;
    }

    #[cfg(all(test, target_arch = "wasm32"))]
    pub(crate) fn drive_retries_for_test(&self) {
        drive_retries(&self.shared);
    }

    #[cfg(test)]
    pub(crate) fn counts(&self) -> (usize, usize) {
        let core = self.shared.core.borrow();
        (core.active_len(), core.queued_len())
    }
}

/// One worker's place in the queue, between building its decoder and handing over the starter.
pub(crate) struct BootReservation {
    shared: Weak<Shared>,
    id: u64,
    session_id: u64,
    kind: BootKind,
    hints: Rc<Cell<PeerBootHints>>,
    guard: Option<SlotGuard>,
}

impl BootReservation {
    /// For the decoder: frees the slot when the worker settles, or when the decoder drops it unrun.
    pub(crate) fn take_settle_callback(&mut self) -> BootSettled {
        let guard = self.guard.take();
        Box::new(move |_outcome| drop(guard))
    }

    pub(crate) fn submit(self, handle: BootHandle) {
        let Some(shared) = self.shared.upgrade() else {
            return;
        };
        shared.core.borrow_mut().push(
            self.id,
            self.session_id,
            self.kind,
            self.hints,
            now_ms(),
            handle,
        );
        schedule_pump(&shared);
    }
}

struct SlotGuard {
    shared: Weak<Shared>,
    id: u64,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        let Some(shared) = self.shared.upgrade() else {
            return;
        };
        let Ok(mut core) = shared.core.try_borrow_mut() else {
            debug_assert!(false, "boot queue borrowed while releasing {}", self.id);
            log::warn!("worker boot queue busy; boot {} not released", self.id);
            return;
        };
        let released = core.release(self.id);
        drop(core);
        if released {
            schedule_pump(&shared);
        }
    }
}

fn now_ms() -> f64 {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::now()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        0.0
    }
}

/// Coalesced onto one microtask, so a caller mid-teardown never re-enters a decoder.
fn schedule_pump(shared: &Rc<Shared>) {
    let _already_scheduled = shared.pump_scheduled.replace(true);
    #[cfg(target_arch = "wasm32")]
    if !_already_scheduled {
        let weak = Rc::downgrade(shared);
        wasm_bindgen_futures::spawn_local(async move {
            if let Some(shared) = weak.upgrade() {
                shared.pump_scheduled.set(false);
                pump(&shared);
            }
        });
    }
}

/// One ticker per queue, running while any boot is in flight.
#[cfg(target_arch = "wasm32")]
fn ensure_expiry_ticker(shared: &Rc<Shared>) {
    if shared.ticking.replace(true) {
        return;
    }
    let weak = Rc::downgrade(shared);
    let tick_ms = shared.core.borrow().tick_ms;
    wasm_bindgen_futures::spawn_local(async move {
        loop {
            gloo_timers::future::TimeoutFuture::new(tick_ms.ceil() as u32).await;
            let Some(shared) = weak.upgrade() else {
                return;
            };
            let expired = shared.core.borrow_mut().take_expired(now_ms());
            // Each expired worker is terminated before its slot can be given to another boot.
            for (_, abort) in expired {
                abort();
            }
            drive_retries(&shared);
            pump(&shared);
            let core = shared.core.borrow();
            if core.active_len() == 0 && core.audio_holds.is_empty() {
                drop(core);
                shared.ticking.set(false);
                return;
            }
        }
    });
}

#[cfg(target_arch = "wasm32")]
fn drive_retries(shared: &Rc<Shared>) {
    if shared.core.borrow().audio_holds.is_empty() {
        return;
    }
    let driver = shared.retry_driver.borrow().clone();
    if let Some(driver) = driver {
        driver();
    }
}

#[cfg(target_arch = "wasm32")]
fn pump(shared: &Rc<Shared>) {
    loop {
        let next = shared.core.borrow_mut().take_next(now_ms());
        let Some((id, starter)) = next else {
            return;
        };
        if starter() {
            ensure_expiry_ticker(shared);
        } else {
            shared.core.borrow_mut().release(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hints(f: impl FnOnce(&mut PeerBootHints)) -> Rc<Cell<PeerBootHints>> {
        let mut h = PeerBootHints::default();
        f(&mut h);
        Rc::new(Cell::new(h))
    }

    fn noop() -> BootHandle {
        BootHandle {
            start: Box::new(|| true),
            abort: Box::new(|| {}),
        }
    }

    fn recording_abort(log: &Rc<RefCell<Vec<u64>>>, id: u64) -> BootHandle {
        let log = log.clone();
        BootHandle {
            start: Box::new(|| true),
            abort: Box::new(move || log.borrow_mut().push(id)),
        }
    }

    fn drain(core: &mut BootQueueCore, now_ms: f64) -> Vec<u64> {
        let mut order = Vec::new();
        while let Some((id, _)) = core.take_next(now_ms) {
            order.push(id);
            core.release(id);
        }
        order
    }

    #[test]
    fn audio_boots_before_any_video_and_talkers_first() {
        let mut core = BootQueueCore::new(MAX_CONCURRENT_WORKER_BOOTS);
        let quiet = hints(|_| {});
        let talker = hints(|h| {
            h.speaking = true;
            h.audio_on = true;
        });
        core.push(1, 10, BootKind::Video, quiet.clone(), 0.0, noop());
        core.push(2, 10, BootKind::Audio, quiet.clone(), 0.0, noop());
        core.push(3, 20, BootKind::Video, talker.clone(), 0.0, noop());
        core.push(4, 20, BootKind::Audio, talker.clone(), 0.0, noop());
        assert_eq!(drain(&mut core, 1.0), vec![4, 2, 3, 1]);
    }

    #[test]
    fn visible_video_boots_before_hidden_video() {
        let mut core = BootQueueCore::new(1);
        let hidden_talker = hints(|h| {
            h.speaking = true;
            h.video_on = true;
        });
        let visible = hints(|h| h.visible = true);
        core.push(1, 10, BootKind::Video, hidden_talker, 0.0, noop());
        core.push(2, 20, BootKind::Video, visible, 0.0, noop());
        assert_eq!(drain(&mut core, 1.0), vec![2, 1]);
    }

    #[test]
    fn a_screen_decoder_waits_until_its_peer_shares() {
        let mut core = BootQueueCore::new(4);
        let peer = hints(|_| {});
        core.push(1, 10, BootKind::Screen, peer.clone(), 0.0, noop());
        core.push(2, 10, BootKind::Video, peer.clone(), 0.0, noop());
        assert_eq!(drain(&mut core, 1.0), vec![2]);
        assert_eq!(core.queued_len(), 1, "the screen entry stays queued");
        peer.set(PeerBootHints {
            screen_on: true,
            ..peer.get()
        });
        assert_eq!(drain(&mut core, 2.0), vec![1]);
    }

    #[test]
    fn priority_is_read_when_the_slot_frees_not_when_queued() {
        let mut core = BootQueueCore::new(1);
        let a = hints(|_| {});
        let b = hints(|_| {});
        core.push(1, 10, BootKind::Audio, a, 0.0, noop());
        core.push(2, 20, BootKind::Audio, b.clone(), 0.0, noop());
        b.set(PeerBootHints {
            speaking: true,
            ..b.get()
        });
        assert_eq!(drain(&mut core, 1.0), vec![2, 1]);
    }

    #[test]
    fn at_most_limit_boots_are_active() {
        let mut core = BootQueueCore::new(MAX_CONCURRENT_WORKER_BOOTS);
        for id in 1..=10 {
            core.push(id, id, BootKind::Audio, hints(|_| {}), 0.0, noop());
        }
        let mut started = Vec::new();
        while let Some((id, _)) = core.take_next(1.0) {
            started.push(id);
        }
        assert_eq!(started.len(), MAX_CONCURRENT_WORKER_BOOTS);
        assert_eq!(core.active_len(), MAX_CONCURRENT_WORKER_BOOTS);
        assert!(core.take_next(1.0).is_none());
    }

    #[test]
    fn a_ready_or_failed_settle_frees_the_slot_for_the_next_boot() {
        let mut queue = WorkerBootQueue::new(1);
        let peer = hints(|_| {});
        let mut first = queue.reserve(10, BootKind::Audio, &peer);
        let mut second = queue.reserve(20, BootKind::Audio, &peer);
        let mut third = queue.reserve(30, BootKind::Audio, &peer);
        let settle_first = first.take_settle_callback();
        let settle_second = second.take_settle_callback();
        let _keep_third = third.take_settle_callback();
        first.submit(noop());
        second.submit(noop());
        third.submit(noop());

        let take = |queue: &mut WorkerBootQueue| queue.shared.core.borrow_mut().take_next(1.0);
        assert_eq!(take(&mut queue).map(|(id, _)| id), Some(1));
        assert!(take(&mut queue).is_none(), "one slot, one boot");
        settle_first(videocall_codecs::decoder::BootOutcome::Ready);
        assert_eq!(take(&mut queue).map(|(id, _)| id), Some(2));
        settle_second(videocall_codecs::decoder::BootOutcome::Failed);
        assert_eq!(take(&mut queue).map(|(id, _)| id), Some(3));
    }

    #[test]
    fn dropping_an_unstarted_decoder_removes_its_entry() {
        let queue = WorkerBootQueue::new(1);
        let peer = hints(|_| {});
        let mut reservation = queue.reserve(10, BootKind::Video, &peer);
        let settle = reservation.take_settle_callback();
        reservation.submit(noop());
        assert!(queue.has_queued_for(10));
        drop(settle);
        assert!(!queue.has_queued_for(10));
        assert_eq!(queue.counts(), (0, 0));
    }

    #[test]
    fn an_expired_boot_is_aborted_before_its_slot_is_reused_every_period() {
        let (timeout, tick) = (100.0, 50.0);
        let mut core = BootQueueCore::with_timeout(2, timeout, tick);
        let aborted = Rc::new(RefCell::new(Vec::new()));
        for id in 1..=6 {
            core.push(
                id,
                id,
                BootKind::Audio,
                hints(|_| {}),
                0.0,
                recording_abort(&aborted, id),
            );
        }
        let mut started = Vec::new();
        let mut now = 0.0;
        while now <= 3.0 * timeout {
            for (_, abort) in core.take_expired(now) {
                abort();
            }
            while let Some((id, _)) = core.take_next(now) {
                started.push(id);
            }
            assert!(
                core.active_len() <= 2,
                "never more than the bound at {now} ms"
            );
            now += tick;
        }
        assert_eq!(started, vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(
            *aborted.borrow(),
            vec![1, 2, 3, 4, 5, 6],
            "every expired boot was aborted"
        );
    }

    #[test]
    fn a_stalled_event_loop_credits_at_most_two_ticks_towards_the_timeout() {
        let (timeout, tick) = (100.0, 20.0);
        let mut core = BootQueueCore::with_timeout(1, timeout, tick);
        core.push(1, 1, BootKind::Audio, hints(|_| {}), 0.0, noop());
        core.take_next(0.0);
        assert!(
            core.take_expired(30_000.0).is_empty(),
            "a 30 s stall credits 40 ms"
        );
        assert!(core.take_expired(30_000.0 + 2.0 * tick).is_empty(), "80 ms");
        assert_eq!(core.take_expired(30_000.0 + 3.0 * tick).len(), 1, "100 ms");
    }

    #[test]
    fn a_camera_boot_waits_while_an_audio_boot_is_in_flight() {
        let mut core = BootQueueCore::new(MAX_CONCURRENT_WORKER_BOOTS);
        let peer = hints(|_| {});
        core.push(1, 10, BootKind::Audio, peer.clone(), 0.0, noop());
        core.push(2, 10, BootKind::Video, peer.clone(), 0.0, noop());
        peer.set(PeerBootHints {
            screen_on: true,
            ..peer.get()
        });
        core.push(3, 10, BootKind::Screen, peer, 0.0, noop());
        assert_eq!(core.take_next(1.0).map(|(id, _)| id), Some(1));
        assert!(
            core.take_next(1.0).is_none(),
            "free slots, but audio is still booting"
        );
        core.release(1);
        assert_eq!(core.take_next(2.0).map(|(id, _)| id), Some(3));
        assert_eq!(core.take_next(2.0).map(|(id, _)| id), Some(2));
    }

    #[test]
    fn a_camera_waits_for_the_retry_of_a_timed_out_audio_boot() {
        let mut core = BootQueueCore::with_timeout(4, 100.0, 50.0);
        let peer = hints(|_| {});
        core.push(1, 10, BootKind::Audio, peer.clone(), 0.0, noop());
        core.push(2, 10, BootKind::Video, peer.clone(), 0.0, noop());
        assert_eq!(core.take_next(0.0).map(|(id, _)| id), Some(1));
        let mut now = 0.0;
        while core.take_expired(now).is_empty() {
            now += 50.0;
        }
        assert_eq!(core.audio_hold_sessions(), vec![10]);
        assert!(
            core.take_next(now).is_none(),
            "the camera waits for the audio retry"
        );

        core.push(3, 10, BootKind::Audio, peer.clone(), now, noop());
        assert!(core.audio_hold_sessions().is_empty());
        assert_eq!(core.take_next(now).map(|(id, _)| id), Some(3));
        core.release(3);
        assert_eq!(core.take_next(now).map(|(id, _)| id), Some(2));
    }

    #[test]
    fn a_timed_out_audio_hold_lasts_until_it_is_ended() {
        let mut core = BootQueueCore::with_timeout(4, 100.0, 50.0);
        let peer = hints(|_| {});
        core.push(1, 10, BootKind::Audio, peer.clone(), 0.0, noop());
        core.push(2, 11, BootKind::Video, peer, 0.0, noop());
        assert_eq!(core.take_next(0.0).map(|(id, _)| id), Some(1));
        let mut now = 0.0;
        while core.take_expired(now).is_empty() {
            now += 50.0;
        }
        let an_hour_later = now + 3_600_000.0;
        assert!(core.take_expired(an_hour_later).is_empty());
        assert!(
            core.take_next(an_hour_later).is_none(),
            "no retry is queued and none is ruled out"
        );
        core.end_audio_hold(10);
        assert_eq!(core.take_next(an_hour_later).map(|(id, _)| id), Some(2));
    }

    #[test]
    fn one_hung_audio_decoder_holds_cameras_back_for_its_audio_budget_at_most() {
        // Production constants. Every attempt of one peer's NetEq hangs; a retry is queued while
        // the queue holds the peer (the retry driver); a camera boot takes 7 s.
        let tick = BOOT_EXPIRY_TICK_MS;
        let camera_boot_ms = 7_000.0;
        let mut core = BootQueueCore::new(MAX_CONCURRENT_WORKER_BOOTS);
        let quiet = hints(|_| {});
        core.push(1, 10, BootKind::Audio, quiet.clone(), 0.0, noop());
        let cameras = 34;
        for id in 100..100 + cameras {
            core.push(id, id, BootKind::Video, quiet.clone(), 0.0, noop());
        }
        let (mut retries, mut started, mut blocked_ms, mut now) = (0, 0, 0.0, 0.0);
        let mut booting: Vec<(u64, f64)> = Vec::new();
        while started < cameras && now < 600_000.0 {
            for (_, abort) in core.take_expired(now) {
                abort();
            }
            if !core.audio_hold_sessions().is_empty() {
                if retries < MAX_BOOT_RETRIES {
                    retries += 1;
                    core.push(
                        1 + u64::from(retries),
                        10,
                        BootKind::Audio,
                        quiet.clone(),
                        now,
                        noop(),
                    );
                } else {
                    core.end_audio_hold(10);
                }
            }
            booting.retain(|&(id, at)| now - at < camera_boot_ms || !core.release(id));
            let mut camera_started = false;
            while let Some((id, _)) = core.take_next(now) {
                if id >= 100 {
                    started += 1;
                    camera_started = true;
                    booting.push((id, now));
                }
            }
            if !camera_started && core.active_len() < MAX_CONCURRENT_WORKER_BOOTS {
                blocked_ms += tick;
            }
            now += tick;
        }
        assert_eq!(started, cameras);
        assert!(
            (1..MAX_BOOT_RETRIES).contains(&retries),
            "the budget, not the retry count, ended the attempts: {retries} retries"
        );
        assert!(
            blocked_ms > BOOT_SLOT_TIMEOUT_MS && blocked_ms <= AUDIO_BOOT_BUDGET_MS,
            "cameras could not start for {blocked_ms} ms in all; budget {AUDIO_BOOT_BUDGET_MS} ms"
        );
    }

    #[test]
    fn a_retried_audio_boot_holds_cameras_back_until_it_settles() {
        let mut core = BootQueueCore::new(MAX_CONCURRENT_WORKER_BOOTS);
        let quiet = hints(|_| {});
        core.push(1, 10, BootKind::Audio, quiet.clone(), 0.0, noop());
        core.push(2, 20, BootKind::Video, quiet.clone(), 0.0, noop());
        assert_eq!(core.take_next(0.0).map(|(id, _)| id), Some(1));
        let mut now = 0.0;
        while core.take_expired(now).is_empty() {
            now += BOOT_EXPIRY_TICK_MS;
        }
        core.push(3, 10, BootKind::Audio, quiet, now, noop());
        assert_eq!(core.take_next(now).map(|(id, _)| id), Some(3));

        // The retry is still taking its memory 9 s after it started.
        let retry_started = now;
        for _ in 0..9 {
            now += BOOT_EXPIRY_TICK_MS;
            assert!(core.take_expired(now).is_empty());
            assert!(
                core.take_next(now).is_none(),
                "a camera started {} ms into the audio retry",
                now - retry_started
            );
        }
        core.release(3);
        assert_eq!(core.take_next(now).map(|(id, _)| id), Some(2));
    }

    #[test]
    fn a_late_tick_does_not_shorten_the_audio_retry() {
        let tick = BOOT_EXPIRY_TICK_MS;
        let mut core = BootQueueCore::new(MAX_CONCURRENT_WORKER_BOOTS);
        let quiet = hints(|_| {});
        core.push(1, 10, BootKind::Audio, quiet.clone(), 0.0, noop());
        core.push(2, 20, BootKind::Video, quiet.clone(), 0.0, noop());
        assert_eq!(core.take_next(0.0).map(|(id, _)| id), Some(1));
        let mut now = 0.0;
        while now < BOOT_SLOT_TIMEOUT_MS - tick {
            now += tick;
            assert!(core.take_expired(now).is_empty());
        }
        // The tick that expires the first attempt is half a tick late.
        now += 1.5 * tick;
        assert_eq!(core.take_expired(now).len(), 1);
        core.push(3, 10, BootKind::Audio, quiet, now, noop());
        assert_eq!(core.take_next(now).map(|(id, _)| id), Some(3));

        let retry_limit = AUDIO_BOOT_BUDGET_MS - BOOT_SLOT_TIMEOUT_MS;
        let retry_started = now;
        for at in [0.5, 1.5, 2.5, 3.5, 4.5, 5.5, 6.5, 7.5, 8.5, 9.5] {
            now = retry_started + at * tick;
            assert!(
                core.take_expired(now).is_empty(),
                "the retry ended {} ms in, before its {retry_limit} ms",
                now - retry_started
            );
            assert!(core.take_next(now).is_none());
        }
        now = retry_started + retry_limit;
        assert_eq!(core.take_expired(now).len(), 1);
        assert!(core.audio_hold_sessions().is_empty());
        assert_eq!(core.take_next(now).map(|(id, _)| id), Some(2));
    }

    #[test]
    fn a_retry_keeps_its_place_ahead_of_cameras_that_aged_while_its_boot_hung() {
        let mut core = BootQueueCore::new(MAX_CONCURRENT_WORKER_BOOTS);
        let quiet = hints(|_| {});
        core.push(1, 10, BootKind::Audio, quiet.clone(), 0.0, noop());
        for id in 2..=35 {
            core.push(id, 100 + id, BootKind::Video, quiet.clone(), 0.0, noop());
        }
        assert_eq!(core.take_next(0.0).map(|(id, _)| id), Some(1));
        let mut now = 0.0;
        while core.take_expired(now).is_empty() {
            now += BOOT_EXPIRY_TICK_MS;
        }
        assert!(
            now >= BOOT_AGING_MS,
            "the cameras aged while the audio boot hung"
        );
        assert!(core.take_next(now).is_none());

        core.push(36, 10, BootKind::Audio, quiet, now + 20.0, noop());
        assert_eq!(core.take_next(now + 20.0).map(|(id, _)| id), Some(36));
    }

    #[test]
    fn a_blocked_camera_head_lets_later_audio_past_a_bounded_number_of_times() {
        let mut core = BootQueueCore::new(MAX_CONCURRENT_WORKER_BOOTS);
        let quiet = hints(|_| {});
        core.push(1, 1, BootKind::Audio, quiet.clone(), 0.0, noop());
        core.push(2, 2, BootKind::Video, quiet.clone(), 0.0, noop());
        assert_eq!(core.take_next(0.0).map(|(id, _)| id), Some(1));
        for id in 3..=12 {
            core.push(id, id, BootKind::Audio, quiet.clone(), 6_000.0, noop());
        }
        let now = 6_000.0;
        let mut admitted = Vec::new();
        while let Some((id, _)) = core.take_next(now) {
            admitted.push(id);
            if admitted.len() == 3 {
                core.release(3);
                core.release(4);
            }
        }
        assert_eq!(
            admitted,
            vec![3, 4, 5, 6],
            "four overtakes, then the aged camera holds"
        );
        for id in [1, 5, 6] {
            core.release(id);
        }
        assert_eq!(core.take_next(now).map(|(id, _)| id), Some(2));
    }

    #[test]
    fn aged_entries_go_in_arrival_order_and_keep_priority_only_inside_one_bucket() {
        let mut core = BootQueueCore::new(1);
        let quiet = hints(|_| {});
        core.push(1, 10, BootKind::Video, quiet.clone(), 0.0, noop());
        core.push(2, 20, BootKind::Video, quiet.clone(), 50.0, noop());
        core.push(3, 30, BootKind::Audio, quiet.clone(), 1_500.0, noop());
        core.push(4, 40, BootKind::Audio, quiet.clone(), 1_600.0, noop());
        core.push(5, 50, BootKind::Video, quiet.clone(), 2_200.0, noop());
        core.push(6, 60, BootKind::Audio, quiet.clone(), 2_300.0, noop());
        core.push(7, 70, BootKind::Audio, quiet, 6_900.0, noop());
        assert_eq!(drain(&mut core, 7_300.0), vec![1, 2, 3, 4, 6, 5, 7]);
    }

    #[test]
    fn an_aged_entry_is_not_starved_by_later_higher_priority_arrivals() {
        let mut core = BootQueueCore::new(1);
        core.push(1, 10, BootKind::Video, hints(|_| {}), 0.0, noop());
        let mut next_id = 2;
        let mut now = 0.0;
        let mut started_old_at = None;
        while now <= 2.0 * BOOT_AGING_MS {
            core.push(
                next_id,
                next_id,
                BootKind::Audio,
                hints(|_| {}),
                now,
                noop(),
            );
            next_id += 1;
            let (id, _) = core.take_next(now).expect("a slot is free");
            core.release(id);
            if id == 1 {
                started_old_at = Some(now);
                break;
            }
            now += 100.0;
        }
        assert_eq!(started_old_at, Some(BOOT_AGING_MS));
    }
}
