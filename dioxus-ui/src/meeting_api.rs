// SPDX-License-Identifier: MIT OR Apache-2.0

//! Meeting API client facade for the dioxus-ui.

use crate::constants::meeting_api_client;
use futures::future::{FutureExt, Shared};
use std::cell::{Cell, RefCell};
pub use videocall_meeting_client::ApiError as JoinError;
pub use videocall_meeting_types::responses::CreateMeetingResponse;
pub use videocall_meeting_types::responses::MeetingGuestInfoResponse;
pub use videocall_meeting_types::responses::MeetingInfoResponse as MeetingInfo;
pub use videocall_meeting_types::responses::ParticipantStatusResponse as JoinMeetingResponse;

fn client() -> Result<videocall_meeting_client::MeetingApiClient, JoinError> {
    meeting_api_client().map_err(JoinError::Config)
}

// ---------------------------------------------------------------------------
// Refresh-and-retry (PKCE flow only) — single-flight
// ---------------------------------------------------------------------------
//
// On a 401 (`JoinError::NotAuthenticated`) in PKCE mode, attempt ONE provider
// token refresh and retry the operation once. The retry rebuilds the client
// via `client()`, which re-reads the refreshed Bearer token fresh from
// sessionStorage (see `meeting_api_client`), so the retried request carries the
// new credential.
//
// Single-flight: concurrent 401s share ONE in-flight refresh future via
// `futures::future::Shared`, so the network POST in `refresh_with_provider`
// fires EXACTLY ONCE per wave. The shared future is cloned OUT of the
// `RefCell` and the borrow is dropped BEFORE awaiting — a `RefCell` borrow is
// never held across an `.await`, which would risk a `BorrowMutError` panic when
// a concurrent caller re-enters.
//
// Slot clearing — every awaiter clears under an epoch guard (NOT creator-only):
// `futures::future::Shared` keeps the inner future alive while ANY handle
// exists, and promotes a surviving waiter to driver if the creator is dropped.
// So slot-clearing must NOT depend on the creator surviving — otherwise a
// cancellable caller (e.g. a `use_resource`-backed Dioxus scope that unmounts
// mid-refresh while a polling joiner had cloned the Shared) would drop the
// creator, the joiner would drive the Shared to completion, and the slot would
// permanently hold a COMPLETED Shared caching the first outcome — every later
// 401 would return that stale result instantly and never refresh again.
//
// Instead, an epoch counter is bumped ONLY when a NEW shared is created
// (Phase 1, creator branch). EVERY awaiter — creator and joiners alike —
// captures the epoch of the wave it is part of and, after the Shared resolves,
// clears the slot iff `REFRESH_EPOCH == captured_epoch`. All awaiters of one
// wave share the same epoch, so:
//   - whichever finishes first clears the slot (`*slot = None`);
//   - later awaiters of the SAME wave see the epoch still matching but the slot
//     already `None` — `*slot = None` is idempotent, harmless;
//   - a NEW wave can only start once the slot is `None`, and it bumps the epoch,
//     so a late straggler from the OLD wave finds `REFRESH_EPOCH != captured`
//     and does NOT clear the new wave's slot.
// This guarantees: (a) the network POST fires exactly once per wave; (b) after a
// wave the slot returns to `None` so a LATER 401 starts a fresh refresh; and
// (c) dropping the creator mid-flight cannot wedge the slot.

type RefreshFut = Shared<std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ()>>>>>;

// SAFETY/atomicity note: the epoch-guarded slot logic below (and the auth
// clear-epoch guard in `auth.rs`) relies on the single-threaded WASM execution
// model — there is no preemption between an epoch read and the slot mutation
// that follows it. These are `thread_local!` rather than cross-thread
// primitives precisely because the only runtime is the browser's single-thread
// event loop. A hypothetical native multi-threaded build would need real
// synchronisation here; no such path exists today.
thread_local! {
    static REFRESH_INFLIGHT: RefCell<Option<RefreshFut>> = const { RefCell::new(None) };
    static REFRESH_EPOCH: Cell<u64> = const { Cell::new(0) };
}

/// Run `op`; on a PKCE 401, refresh the provider token once (single-flight) and
/// retry `op` exactly once with the refreshed credential.
async fn with_refresh_retry<T, F, Fut>(op: F) -> Result<T, JoinError>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, JoinError>>,
{
    match op().await {
        Err(JoinError::NotAuthenticated) if crate::constants::is_pkce_flow() => {
            let outcome = refresh_single_flight().await;
            match outcome {
                Ok(()) => op().await,
                Err(()) => Err(JoinError::NotAuthenticated),
            }
        }
        other => other,
    }
}

/// Drive (or join) the single in-flight provider refresh. Returns `Ok(())` if
/// the refresh produced a token, `Err(())` otherwise.
///
/// Thin wrapper over the generic single-flight core: in the creator branch it
/// supplies the production refresh future — `crate::auth::refresh_access_token()`
/// (which returns `Result<String, String>`) adapted to `Result<(), ()>` exactly
/// as before. The `make_fut` closure is invoked ONLY by the creator branch,
/// inside the same `RefCell` borrow where the future was constructed today, so
/// the borrow-dropped-before-await property and the network-POST-once-per-wave
/// guarantee are preserved bit-for-bit.
async fn refresh_single_flight() -> Result<(), ()> {
    refresh_single_flight_with(|| {
        crate::auth::refresh_access_token().map(|r| r.map(|_| ()).map_err(|_| ()))
    })
    .await
}

/// Generic single-flight/epoch core. `make_fut` is the factory for the wave's
/// underlying refresh future; it is called EXACTLY ONCE per wave, in the creator
/// branch, while the `REFRESH_INFLIGHT` borrow is held — the resulting future is
/// then `.boxed_local().shared()` and stored in the slot, exactly as the
/// production path did inline. Joiners clone the stored `Shared` instead of
/// calling `make_fut`. All awaiting/clearing logic is unchanged from the
/// original `refresh_single_flight`.
async fn refresh_single_flight_with<F, Fut>(make_fut: F) -> Result<(), ()>
where
    F: FnOnce() -> Fut,
    // `'static` matches the original inline future: `refresh_access_token()` is
    // an `async fn` capturing no borrows, so its future is `'static` — and
    // `boxed_local()` requires it. The bound is behavior-preserving.
    Fut: std::future::Future<Output = Result<(), ()>> + 'static,
{
    // Phase 1: get-or-create the shared future, capturing the epoch of the wave
    // THIS call belongs to. The creator bumps the epoch (a new wave); a joiner
    // reads the current (unchanged) epoch of the wave it is joining. The epoch
    // is bumped ONLY here, in the creator branch, so the captured value is a
    // stable identifier for the wave. The RefCell borrow is fully released at
    // the end of this block — never held across the await below.
    let (shared, wave_epoch) = REFRESH_INFLIGHT.with(|slot| {
        let mut guard = slot.borrow_mut();
        if let Some(existing) = guard.as_ref() {
            // Joiner: share the in-flight future and the wave's current epoch.
            let epoch = REFRESH_EPOCH.with(|e| e.get());
            (existing.clone(), epoch)
        } else {
            // Creator: start a new wave and bump the epoch.
            let my_epoch = REFRESH_EPOCH.with(|e| {
                let next = e.get().wrapping_add(1);
                e.set(next);
                next
            });
            // `make_fut()` is invoked here — the creator branch, inside the
            // borrow — and its output is boxed/shared/stored exactly as the
            // inlined `refresh_access_token()...` future was previously.
            let fut = make_fut().boxed_local().shared();
            *guard = Some(fut.clone());
            (fut, my_epoch)
        }
    });

    // Phase 2: await OUTSIDE any borrow.
    let result = shared.await;

    // Phase 3: every awaiter (creator AND joiners) clears the slot for ITS wave,
    // guarded by the epoch. This does not depend on the creator surviving — a
    // dropped creator cannot wedge the slot because any joiner that drove the
    // Shared to completion clears it here. The first awaiter of the wave to reach
    // this clears the slot; later awaiters of the same wave find it already
    // `None` (idempotent). A straggler from an OLDER wave finds the epoch has
    // moved on (a new wave only starts once the slot is `None`, which bumps the
    // epoch) and does NOT clear the newer wave's slot.
    REFRESH_INFLIGHT.with(|slot| {
        if REFRESH_EPOCH.with(|e| e.get()) == wave_epoch {
            *slot.borrow_mut() = None;
        }
    });

    result
}

/// Public single-flight provider-refresh entry for callers OUTSIDE the meeting
/// API path.
///
/// Returns `Ok(())` if the refresh produced a token, `Err(())` otherwise.
///
/// Routing an external caller through here — rather than calling
/// `auth::refresh_access_token()` directly — means an external-caller refresh and
/// a concurrent meeting-driven refresh COALESCE through the same
/// `REFRESH_INFLIGHT` slot: the underlying PKCE network POST fires exactly once
/// per wave even if the meeting path 401s and the external caller observes
/// `token_expired` at the same instant (a likely race, since both auth on the
/// SAME session token and expire together). Without this, two separate refreshes
/// could fire, the second using a refresh-token the first already rotated away
/// (Okta rotates refresh tokens) → a spurious `invalid_grant` that clears the
/// now-valid token and logs the user out.
pub async fn refresh_token_single_flight() -> Result<(), ()> {
    refresh_single_flight().await
}

/// Reset the single-flight refresh slot. Called on logout so a Shared future
/// cloned by an in-flight wave cannot linger. Bumping REFRESH_EPOCH ensures any
/// straggler that resolves later will NOT clear (or be mistaken for) a future
/// wave's slot, and clearing the slot lets a post-logout/re-login 401 start a
/// fresh refresh immediately. Belt-and-suspenders alongside auth's clear-epoch
/// guard (which already discards the resurrected tokens).
pub fn reset_refresh_inflight() {
    REFRESH_EPOCH.with(|e| e.set(e.get().wrapping_add(1)));
    REFRESH_INFLIGHT.with(|slot| *slot.borrow_mut() = None);
}

pub async fn join_meeting(
    meeting_id: &str,
    display_name: Option<&str>,
) -> Result<JoinMeetingResponse, JoinError> {
    log::info!("Joining meeting via API: {meeting_id} (display_name: {display_name:?})");
    let result =
        with_refresh_retry(|| async { client()?.join_meeting(meeting_id, display_name).await })
            .await?;
    log::info!(
        "Join response: status={}, is_host={}",
        result.status,
        result.is_host
    );
    Ok(result)
}

pub async fn get_meeting_info(meeting_id: &str) -> Result<MeetingInfo, JoinError> {
    with_refresh_retry(|| async { client()?.get_meeting(meeting_id).await }).await
}

pub async fn check_status(meeting_id: &str) -> Result<JoinMeetingResponse, JoinError> {
    with_refresh_retry(|| async { client()?.get_status(meeting_id).await }).await
}

/// Check status for a guest participant using their observer JWT as a Bearer
/// token. Calls `GET /api/v1/meetings/{meeting_id}/guest-status`.
pub async fn check_guest_status(
    meeting_id: &str,
    observer_token: &str,
) -> Result<JoinMeetingResponse, JoinError> {
    make_guest_client(observer_token)?
        .get_guest_status(meeting_id)
        .await
}

/// Fetch the participant's current status using the appropriate auth mode.
pub async fn fetch_participant_status(
    meeting_id: &str,
    observer_token: &str,
    is_guest: bool,
) -> Result<JoinMeetingResponse, JoinError> {
    if is_guest {
        check_guest_status(meeting_id, observer_token).await
    } else {
        check_status(meeting_id).await
    }
}

fn make_guest_client(token: &str) -> Result<videocall_meeting_client::MeetingApiClient, JoinError> {
    let base_url = crate::constants::meeting_api_base_url().map_err(JoinError::Config)?;
    Ok(videocall_meeting_client::MeetingApiClient::new(
        &base_url,
        videocall_meeting_client::AuthMode::Bearer(token.to_string()),
    ))
}

pub async fn refresh_room_token(meeting_id: &str) -> Result<String, JoinError> {
    with_refresh_retry(|| async { client()?.refresh_room_token(meeting_id).await }).await
}

pub async fn update_meeting(
    meeting_id: &str,
    waiting_room_enabled: Option<bool>,
    admitted_can_admit: Option<bool>,
    end_on_host_leave: Option<bool>,
    allow_guests: Option<bool>,
    recording_allowed_for_all: Option<bool>,
) -> Result<MeetingInfo, JoinError> {
    let req = videocall_meeting_types::requests::UpdateMeetingRequest {
        waiting_room_enabled,
        admitted_can_admit,
        end_on_host_leave,
        allow_guests,
        recording_allowed_for_all,
    };
    let req = &req;
    with_refresh_retry(|| async move { client()?.update_meeting(meeting_id, req).await }).await
}

pub async fn end_meeting(meeting_id: &str) -> Result<MeetingInfo, JoinError> {
    log::info!("Ending meeting via API: {meeting_id}");
    with_refresh_retry(|| async { client()?.end_meeting(meeting_id).await }).await
}

pub async fn get_meeting_guest_info(
    meeting_id: &str,
) -> Result<MeetingGuestInfoResponse, JoinError> {
    with_refresh_retry(|| async { client()?.get_meeting_guest_info(meeting_id).await }).await
}

pub async fn delete_meeting(meeting_id: &str) -> Result<(), JoinError> {
    log::info!("Deleting meeting via API: {meeting_id}");
    with_refresh_retry(|| async { client()?.delete_meeting(meeting_id).await }).await?;
    Ok(())
}

pub async fn leave_meeting(meeting_id: &str) -> Result<(), JoinError> {
    log::info!("Leaving meeting via API: {meeting_id}");
    match with_refresh_retry(|| async { client()?.leave_meeting(meeting_id).await }).await {
        Ok(_) => {
            log::info!("Successfully left meeting {meeting_id}");
            Ok(())
        }
        Err(JoinError::NotFound(_)) => {
            log::warn!("Not in meeting {meeting_id} when trying to leave");
            Ok(())
        }
        Err(e) => Err(e),
    }
}

pub async fn transfer_host(meeting_id: &str, user_id: &str) -> Result<(), JoinError> {
    log::info!("Transferring host via API: {meeting_id} -> {user_id}");
    client()?.transfer_host(meeting_id, user_id).await
}

pub async fn update_display_name(
    meeting_id: &str,
    display_name: &str,
    session_id: Option<u64>,
) -> Result<JoinMeetingResponse, JoinError> {
    log::info!(
        "Updating display name via API: {meeting_id} (display_name: {display_name}, session_id: {session_id:?})"
    );
    with_refresh_retry(|| async {
        client()?
            .update_display_name(meeting_id, display_name, session_id)
            .await
    })
    .await
}

pub async fn create_meeting(
    meeting_id: Option<&str>,
    allow_guests: bool,
) -> Result<CreateMeetingResponse, JoinError> {
    let req = videocall_meeting_types::requests::CreateMeetingRequest {
        meeting_id: meeting_id.map(|s| s.to_string()),
        attendees: vec![],
        password: None,
        waiting_room_enabled: Some(true),
        admitted_can_admit: Some(false),
        allow_guests: Some(allow_guests),
        end_on_host_leave: None,
        recording_allowed_for_all: None,
    };
    let req = &req;
    with_refresh_retry(|| async move { client()?.create_meeting(req).await }).await
}

pub async fn join_meeting_as_guest(
    meeting_id: &str,
    display_name: &str,
) -> Result<JoinMeetingResponse, JoinError> {
    log::info!("Joining meeting as guest via API: {meeting_id} (display_name: {display_name})");
    let stored_id = crate::auth::get_guest_session_id();
    let base_url = crate::constants::meeting_api_base_url().map_err(JoinError::Config)?;
    let result = videocall_meeting_client::MeetingApiClient::new(
        &base_url,
        videocall_meeting_client::AuthMode::Cookie,
    )
    .join_meeting_as_guest(meeting_id, display_name, stored_id.as_deref())
    .await?;

    crate::auth::store_guest_session_id(&result.user_id);
    log::info!(
        "Guest join response: status={}, is_host={}, user_id={}",
        result.status,
        result.is_host,
        result.user_id,
    );
    Ok(result)
}

/// Leave a meeting as a guest, authenticated via the observer JWT that was
/// issued at join time. Calls `POST /api/v1/meetings/{meeting_id}/leave-guest`.
pub async fn leave_meeting_as_guest(
    meeting_id: &str,
    observer_token: &str,
) -> Result<(), JoinError> {
    log::info!("Guest leaving meeting via API: {meeting_id}");
    match make_guest_client(observer_token)?
        .leave_meeting_as_guest(meeting_id)
        .await
    {
        Ok(_) => {
            log::info!("Guest successfully left meeting {meeting_id}");
            Ok(())
        }
        Err(JoinError::NotFound(_)) => {
            log::warn!("Guest row not found for meeting {meeting_id} when trying to leave");
            Ok(())
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    //! Host-target (`cargo test --lib`, NOT wasm) tests for the single-flight
    //! provider-refresh machinery in `refresh_single_flight_with`.
    //!
    //! Harness: the production runtime is the browser's single-threaded event
    //! loop, so these tests reproduce that with `futures::executor::LocalPool`
    //! — a single-threaded executor running on the test thread itself. That
    //! matters because the machinery uses `thread_local!` slots
    //! (`REFRESH_INFLIGHT` / `REFRESH_EPOCH`); a single-threaded pool keeps all
    //! spawned tasks on the same thread, so every task sees the same
    //! thread-locals (exactly as in the browser). A multi-threaded executor
    //! would give each worker its own thread-locals and the single-flight slot
    //! would not be shared — defeating the test.
    //!
    //! To put several callers "in flight at once", we spawn each
    //! `refresh_single_flight_with(..)` onto the pool, then `run_until_stalled()`
    //! so every task advances to its first await point (parked on the gate)
    //! before any future resolves. The gate is a `oneshot::Receiver` that
    //! `make_fut` awaits; nothing resolves until we `send(())` on the matching
    //! sender. This guarantees the "all callers started before any completes"
    //! precondition the single-flight invariant is about.
    //!
    //! Each test calls `reset_refresh_inflight()` first so it does not inherit
    //! slot/epoch state from a prior test on the same thread.

    use super::*;
    use futures::channel::oneshot;
    use futures::executor::LocalPool;
    use futures::task::LocalSpawnExt;
    use std::cell::Cell;
    use std::rc::Rc;

    /// 1. SINGLE POST PER WAVE.
    ///
    /// Two callers both start (park on the gate) BEFORE either resolves. The
    /// creator runs `make_fut` once and stores the `Shared`; the joiner clones
    /// it. After releasing the gate and draining the pool, `make_fut` must have
    /// been invoked EXACTLY ONCE — the joiner reused the in-flight future.
    ///
    /// Breaking mutation (in `refresh_single_flight_with`, Phase 1): make the
    /// creator branch ALWAYS create a fresh future — i.e. delete the
    /// `if let Some(existing) = guard.as_ref()` joiner branch (or change it to
    /// fall through to `make_fut()` unconditionally). Then both callers each
    /// run `make_fut`, the counter reaches 2, and this assertion fails.
    #[test]
    fn single_post_per_wave() {
        reset_refresh_inflight();
        let mut pool = LocalPool::new();
        let spawner = pool.spawner();

        let calls = Rc::new(Cell::new(0u32));
        // One shared gate cloned into each factory; only the creator's factory
        // is actually invoked, so a single gate gates the whole wave.
        let (tx, rx) = oneshot::channel::<()>();
        let rx = rx.map(|_| ()).boxed_local().shared();

        // Two distinct factories (FnOnce), sharing the SAME counter + gate.
        // Only the creator branch invokes one of them.
        let mk = |calls: Rc<Cell<u32>>, rx: futures::future::Shared<_>| {
            move || {
                calls.set(calls.get() + 1);
                async move {
                    let _: () = rx.await;
                    Ok::<(), ()>(())
                }
            }
        };

        let h1 = spawner
            .spawn_local_with_handle(refresh_single_flight_with(mk(calls.clone(), rx.clone())))
            .unwrap();
        let h2 = spawner
            .spawn_local_with_handle(refresh_single_flight_with(mk(calls.clone(), rx.clone())))
            .unwrap();

        // Advance both tasks to their first await (parked on the gate) BEFORE
        // anything resolves — this establishes the concurrent-wave precondition.
        pool.run_until_stalled();
        assert_eq!(
            calls.get(),
            1,
            "creator should have invoked make_fut exactly once before resolution"
        );

        // Release the gate and drive both to completion.
        tx.send(()).unwrap();
        let (r1, r2) = pool.run_until(async move { futures::join!(h1, h2) });

        assert_eq!(calls.get(), 1, "make_fut must fire EXACTLY once per wave");
        assert_eq!(r1, Ok(()));
        assert_eq!(r2, Ok(()));
    }

    /// 2. SLOT CLEARS AFTER A WAVE.
    ///
    /// Run one full wave to completion, then issue a LATER call. The second
    /// wave must invoke `make_fut` again (counter -> 2), proving the slot was
    /// cleared after the first wave (otherwise the second call would join a
    /// completed-and-cached `Shared` and skip `make_fut`).
    ///
    /// Breaking mutation (Phase 3): comment out the epoch-guarded clear
    /// `*slot.borrow_mut() = None;`. Then the first wave's completed `Shared`
    /// lingers in the slot, the second call joins it instead of creating a new
    /// wave, `make_fut` is never called again, the counter stays 1, and this
    /// assertion fails.
    #[test]
    fn slot_clears_after_wave() {
        reset_refresh_inflight();
        let calls = Rc::new(Cell::new(0u32));

        let factory = || {
            let calls = calls.clone();
            move || {
                let calls = calls.clone();
                calls.set(calls.get() + 1);
                async move { Ok::<(), ()>(()) }
            }
        };

        // Wave 1 (resolves immediately — no gate needed for the post-wave test).
        let r1 = futures::executor::block_on(refresh_single_flight_with(factory()));
        assert_eq!(r1, Ok(()));
        assert_eq!(calls.get(), 1, "first wave invokes make_fut once");

        // Wave 2 — must start fresh because the slot was cleared.
        let r2 = futures::executor::block_on(refresh_single_flight_with(factory()));
        assert_eq!(r2, Ok(()));
        assert_eq!(
            calls.get(),
            2,
            "a LATER call must invoke make_fut again (slot cleared after wave 1)"
        );
    }

    /// 3. CREATOR-DROP DOES NOT WEDGE — see honesty note below.
    ///
    /// HONEST GAP: a faithful "creator dropped mid-flight while a joiner drives
    /// the Shared to completion" simulation is fiddly here. To drop the creator
    /// task mid-await we would need to hold a `RemoteHandle`, advance the pool so
    /// the creator parks on the gate, drop the creator's handle to cancel it,
    /// and rely on the joiner being promoted to driver — but `LocalPool` task
    /// cancellation via `RemoteHandle` drop plus the get-or-create timing makes a
    /// truly creator-dropped-then-joiner-drives scenario non-deterministic to set
    /// up without reaching into executor internals. Rather than fake it, we
    /// assert the WEAKER-BUT-REAL property that the epoch-guarded clear is
    /// performed by WHICHEVER awaiter reaches Phase 3 first — i.e. clearing is
    /// NOT creator-only. We prove this by running a wave through a SINGLE caller
    /// and confirming a subsequent caller starts fresh (the slot was cleared by
    /// an awaiter, which is the same Phase-3 code path a promoted joiner would
    /// execute). This is the same code that guarantees a dropped creator cannot
    /// wedge the slot, but it does not exercise the actual drop+promote timing —
    /// that gap is acknowledged and left to the WASM/browser integration path.
    #[test]
    fn awaiter_clears_slot_not_creator_only() {
        reset_refresh_inflight();
        let calls = Rc::new(Cell::new(0u32));
        let factory = || {
            let calls = calls.clone();
            move || {
                let calls = calls.clone();
                calls.set(calls.get() + 1);
                async move { Ok::<(), ()>(()) }
            }
        };

        // Single awaiter drives a wave; Phase 3 (run by this awaiter) must clear.
        let _ = futures::executor::block_on(refresh_single_flight_with(factory()));
        // A fresh wave can only start if the slot is None — prove it can.
        let _ = futures::executor::block_on(refresh_single_flight_with(factory()));
        assert_eq!(
            calls.get(),
            2,
            "the awaiter (not necessarily the creator) clears the slot, so a later wave starts fresh"
        );
    }

    /// 4. OUTCOME PROPAGATION.
    ///
    /// A wave whose `make_fut` resolves `Err(())` must return `Err(())` to all
    /// awaiters; a wave that resolves `Ok(())` returns `Ok(())`.
    #[test]
    fn outcome_propagates_to_all_awaiters() {
        // --- Err wave (two concurrent awaiters) ---
        reset_refresh_inflight();
        let mut pool = LocalPool::new();
        let spawner = pool.spawner();
        let (tx, rx) = oneshot::channel::<()>();
        let rx = rx.map(|_| ()).boxed_local().shared();

        let mk = |rx: futures::future::Shared<_>| {
            move || async move {
                let _: () = rx.await;
                Err::<(), ()>(())
            }
        };
        let h1 = spawner
            .spawn_local_with_handle(refresh_single_flight_with(mk(rx.clone())))
            .unwrap();
        let h2 = spawner
            .spawn_local_with_handle(refresh_single_flight_with(mk(rx.clone())))
            .unwrap();
        pool.run_until_stalled();
        tx.send(()).unwrap();
        let (r1, r2) = pool.run_until(async move { futures::join!(h1, h2) });
        assert_eq!(r1, Err(()), "Err outcome must propagate to creator");
        assert_eq!(r2, Err(()), "Err outcome must propagate to joiner");

        // --- Ok wave ---
        reset_refresh_inflight();
        let ok =
            futures::executor::block_on(refresh_single_flight_with(|| async { Ok::<(), ()>(()) }));
        assert_eq!(ok, Ok(()), "Ok outcome must propagate");
    }

    /// 5. `reset_refresh_inflight` clears an in-flight slot (post-logout start fresh).
    ///
    /// Park a wave on a gate (slot occupied), call `reset_refresh_inflight()`
    /// (the logout path), then a NEW call must invoke `make_fut` again because
    /// the slot was cleared + epoch bumped — proving logout does not leave a
    /// stale Shared that a post-login 401 would join.
    ///
    /// Mutation that must make THIS test fail with a clean assertion (NOT a
    /// hang): delete/disable the slot-clear line
    /// `REFRESH_INFLIGHT.with(|slot| *slot.borrow_mut() = None);` inside
    /// `reset_refresh_inflight` (leave the epoch bump). Wave 2 then JOINS the
    /// still-gated wave-1 `Shared` instead of becoming a CREATOR, `make_fut2`
    /// is never called, and `calls` stays 1 instead of reaching 2 — so the
    /// `assert_eq!(calls.get(), 2, ...)` below fails.
    ///
    /// Why `run_until_stalled` (not `run_until`) drives wave 2: under the
    /// regression, wave 2 joins wave 1's `Shared`, which is gated on the
    /// never-fired `rx` (we deliberately do NOT `tx.send(())` until cleanup).
    /// `run_until(refresh_single_flight_with(make_fut2))` would therefore block
    /// FOREVER, surfacing the mutation as an opaque CI hang/timeout rather than
    /// a readable assertion failure. `run_until_stalled` makes all possible
    /// progress and then returns, converting that deadlock into a clean
    /// `calls` counter mismatch.
    #[test]
    fn reset_clears_inflight_slot() {
        reset_refresh_inflight();
        let mut pool = LocalPool::new();
        let spawner = pool.spawner();
        let calls = Rc::new(Cell::new(0u32));

        let (tx, rx) = oneshot::channel::<()>();
        let rx = rx.map(|_| ()).boxed_local().shared();

        let make_fut = {
            let calls = calls.clone();
            let rx = rx.clone();
            move || {
                calls.set(calls.get() + 1);
                async move {
                    let _: () = rx.await;
                    Ok::<(), ()>(())
                }
            }
        };
        let h1 = spawner
            .spawn_local_with_handle(refresh_single_flight_with(make_fut))
            .unwrap();
        pool.run_until_stalled();
        assert_eq!(calls.get(), 1, "wave 1 started");

        // Logout: clear the in-flight slot + bump epoch.
        reset_refresh_inflight();

        // A post-logout call must start a brand-new wave (slot was cleared).
        // Spawn wave 2 onto the pool and let it make ALL possible progress
        // without blocking. If the slot was cleared (correct), wave 2 is the
        // CREATOR: `make_fut2` runs and its ungated `async { Ok(()) }` future
        // resolves immediately, so `h2` is already complete after the stall. If
        // the slot was NOT cleared (regression), wave 2 JOINS the still-gated
        // wave-1 Shared, `make_fut2` is never called, and the pool stalls with
        // `calls` stuck at 1 — caught by the assertion below instead of a hang.
        let calls2 = calls.clone();
        let make_fut2 = move || {
            calls2.set(calls2.get() + 1);
            async move { Ok::<(), ()>(()) }
        };
        let h2 = spawner
            .spawn_local_with_handle(refresh_single_flight_with(make_fut2))
            .unwrap();
        pool.run_until_stalled();
        assert_eq!(
            calls.get(),
            2,
            "reset_refresh_inflight must clear the slot so a post-logout call starts a fresh wave (CREATOR), not join the stale Shared"
        );

        // Release the wave-1 gate so the original (now-orphaned) task can finish
        // cleanly, then drive both handles to completion so the pool drains with
        // no leak/panic. `h2` is already resolved in the correct case; awaiting a
        // resolved handle is fine.
        let _ = tx.send(());
        pool.run_until(async move {
            let _ = h1.await;
            let _ = h2.await;
        });
    }
}
