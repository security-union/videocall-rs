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
async fn refresh_single_flight() -> Result<(), ()> {
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
            let fut = crate::auth::refresh_access_token()
                .map(|r| r.map(|_| ()).map_err(|_| ()))
                .boxed_local()
                .shared();
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
) -> Result<MeetingInfo, JoinError> {
    let req = videocall_meeting_types::requests::UpdateMeetingRequest {
        waiting_room_enabled,
        admitted_can_admit,
        end_on_host_leave,
        allow_guests,
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
