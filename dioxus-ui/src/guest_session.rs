// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-meeting guest identity persistence (issue #2331).
//!
//! `POST /join-guest` resumes a previous guest identity only when the request
//! carries **both** the previous `user_id` and a guest JWT whose `sub` equals
//! it and whose `room` equals the meeting; an id alone is answered with a fresh
//! identity. Keyed per meeting because the token is bound to one `room` claim.

use crate::context::{read_session_storage, remove_session_storage, write_session_storage};
use crate::id_token::now_secs;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::Deserialize;

/// Prefix shared by every key this module owns. Also matches
/// [`LEGACY_SESSION_KEY`], so [`clear_all`] evicts that leftover too.
const KEY_PREFIX: &str = "vc_guest_";

/// The marker pre-#2331 builds wrote to record "this tab was a guest". Nothing
/// writes it any more; it is only evicted.
const LEGACY_SESSION_KEY: &str = "vc_guest_session_id";

fn id_key(meeting_id: &str) -> String {
    format!("{KEY_PREFIX}id:{meeting_id}")
}

fn token_key(meeting_id: &str) -> String {
    format!("{KEY_PREFIX}token:{meeting_id}")
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct GuestSession {
    pub user_id: Option<String>,
    pub token: Option<String>,
}

#[derive(Deserialize)]
struct ExpClaim {
    #[serde(default)]
    exp: Option<u64>,
}

/// Read the `exp` claim without verifying the signature — the server does that.
fn token_exp(token: &str) -> Option<u64> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice::<ExpClaim>(&bytes).ok()?.exp
}

/// The `(user_id, token)` pair to replay on a rejoin, if this tab has one.
pub(crate) fn resume_credentials(session: &GuestSession, now: u64) -> Option<(&str, &str)> {
    let user_id = session.user_id.as_deref().filter(|id| !id.is_empty())?;
    let token = session.token.as_deref().filter(|t| !t.is_empty())?;
    (!is_expired(token, now)).then_some((user_id, token))
}

const REFRESH_MARGIN_SECS: u64 = 300;

fn expires_at(token: &str) -> u64 {
    token_exp(token).unwrap_or(0)
}

fn is_expired(token: &str, now: u64) -> bool {
    token_exp(token).is_some_and(|exp| exp <= now)
}

/// Pick the token to keep for `user_id` out of `candidates` and what is stored.
///
/// The stored token is a candidate only while the identity is unchanged: a
/// token minted for the old `user_id` must never be replayed against a new one.
pub(crate) fn next_token<'a>(
    stored: &'a GuestSession,
    user_id: &str,
    candidates: &[Option<&'a str>],
    now: u64,
) -> Option<&'a str> {
    let carried = match stored.user_id.as_deref() {
        Some(prev) if prev == user_id => stored.token.as_deref(),
        _ => None,
    }
    .filter(|token| !token.is_empty() && !is_expired(token, now));

    let mut best: Option<(u64, &'a str)> = None;
    for token in candidates
        .iter()
        .copied()
        .flatten()
        .filter(|token| !token.is_empty() && !is_expired(token, now))
    {
        let rank = expires_at(token);
        if best.is_none_or(|(best_rank, _)| rank > best_rank) {
            best = Some((rank, token));
        }
    }

    match (best.map(|(_, token)| token), carried) {
        (Some(offered), Some(held)) => {
            let held_exp = expires_at(held);
            let outlives_held = expires_at(offered) > held_exp.saturating_add(REFRESH_MARGIN_SECS);
            let held_expiring = held_exp <= now.saturating_add(REFRESH_MARGIN_SECS);
            Some(if outlives_held || held_expiring {
                offered
            } else {
                held
            })
        }
        (offered, None) => offered,
        (None, held) => held,
    }
}

pub fn load(meeting_id: &str) -> GuestSession {
    GuestSession {
        user_id: read_session_storage(&id_key(meeting_id)),
        token: read_session_storage(&token_key(meeting_id)),
    }
}

pub fn resume(meeting_id: &str) -> Option<(String, String)> {
    let session = load(meeting_id);
    resume_credentials(&session, now_secs())
        .map(|(user_id, token)| (user_id.to_string(), token.to_string()))
}

#[derive(Debug, PartialEq)]
pub(crate) enum TokenWrite<'a> {
    Write(&'a str),
    Remove,
    Leave,
}

/// What [`remember`] should do to the stored token. Only rotation removes it:
/// `is_expired` reads a wall clock, so deleting on expiry is irreversible.
pub(crate) fn token_write_action<'a>(
    stored: &GuestSession,
    keep: Option<&'a str>,
    rotated: bool,
) -> TokenWrite<'a> {
    match keep {
        Some(token) if stored.token.as_deref() != Some(token) => TokenWrite::Write(token),
        None if rotated => TokenWrite::Remove,
        _ => TokenWrite::Leave,
    }
}

/// Record the identity the server just returned for `meeting_id`. `user_id` is
/// always the response's, which may be a freshly minted one.
pub fn remember(
    meeting_id: &str,
    user_id: &str,
    room_token: Option<&str>,
    observer_token: Option<&str>,
) {
    if user_id.is_empty() {
        return;
    }
    let stored = load(meeting_id);
    let keep = next_token(&stored, user_id, &[room_token, observer_token], now_secs());
    let rotated = stored.user_id.as_deref() != Some(user_id);

    match token_write_action(&stored, keep, rotated) {
        TokenWrite::Write(token) => write_session_storage(&token_key(meeting_id), token),
        TokenWrite::Remove => remove_session_storage(&token_key(meeting_id)),
        TokenWrite::Leave => {}
    }
    if rotated {
        write_session_storage(&id_key(meeting_id), user_id);
    }
}

pub fn clear(meeting_id: &str) {
    remove_session_storage(&id_key(meeting_id));
    remove_session_storage(&token_key(meeting_id));
}

pub fn has_any() -> bool {
    !stored_keys().is_empty()
}

/// Drop every stored guest identity in this tab. For logout, which must not
/// leave a bearer credential behind.
pub fn clear_all() {
    for key in stored_keys() {
        remove_session_storage(&key);
    }
}

/// Evict only [`LEGACY_SESSION_KEY`], leaving per-meeting credentials in place.
/// For callers that mean "reset the guest flag", not "revoke the identity":
/// `check_session` runs on every meeting-page and chat-sidebar mount.
pub fn clear_legacy_marker() {
    remove_session_storage(LEGACY_SESSION_KEY);
}

fn stored_keys() -> Vec<String> {
    let Some(storage) = web_sys::window().and_then(|w| w.session_storage().ok().flatten()) else {
        return Vec::new();
    };
    let len = storage.length().unwrap_or(0);
    (0..len)
        .filter_map(|i| storage.key(i).ok().flatten())
        .filter(|key| key.starts_with(KEY_PREFIX))
        .collect()
}

#[cfg(test)]
mod tests {
    //! The storage round trip is covered by `tests/guest_session_persistence.rs`.

    use super::*;

    fn token_with_exp(exp: u64) -> String {
        let payload = URL_SAFE_NO_PAD.encode(format!(r#"{{"sub":"guest:x","exp":{exp}}}"#));
        format!("header.{payload}.signature")
    }

    fn session(user_id: Option<&str>, token: Option<&str>) -> GuestSession {
        GuestSession {
            user_id: user_id.map(str::to_string),
            token: token.map(str::to_string),
        }
    }

    #[test]
    fn token_exp_reads_the_payload_claim() {
        assert_eq!(
            token_exp(&token_with_exp(1_700_000_000)),
            Some(1_700_000_000)
        );
        assert_eq!(token_exp("not-a-jwt"), None);
        assert_eq!(token_exp("header.!!!not-base64!!!.sig"), None);
    }

    #[test]
    fn resume_requires_both_id_and_token() {
        let token = token_with_exp(2_000);
        assert_eq!(
            resume_credentials(&session(Some("guest:a"), Some(&token)), 1_000),
            Some(("guest:a", token.as_str()))
        );
        assert_eq!(
            resume_credentials(&session(Some("guest:a"), None), 1_000),
            None
        );
        assert_eq!(
            resume_credentials(&session(None, Some(&token)), 1_000),
            None
        );
    }

    #[test]
    fn resume_drops_an_expired_token() {
        let token = token_with_exp(1_000);
        assert_eq!(
            resume_credentials(&session(Some("guest:a"), Some(&token)), 1_001),
            None
        );
        assert!(resume_credentials(&session(Some("guest:a"), Some(&token)), 999).is_some());
    }

    #[test]
    fn next_token_prefers_the_latest_expiry() {
        let room = token_with_exp(9_000);
        let observer = token_with_exp(3_000);
        assert_eq!(
            next_token(
                &GuestSession::default(),
                "guest:a",
                &[Some(&observer), Some(&room)],
                1_000
            ),
            Some(room.as_str())
        );
    }

    #[test]
    fn next_token_keeps_the_stored_token_when_the_response_carries_none() {
        let stored_token = token_with_exp(9_000);
        assert_eq!(
            next_token(
                &session(Some("guest:a"), Some(&stored_token)),
                "guest:a",
                &[None, None],
                1_000
            ),
            Some(stored_token.as_str())
        );
    }

    #[test]
    fn next_token_keeps_an_equivalent_token_rather_than_rewriting_storage() {
        // What a status poll offers: the same credential, minted seconds later.
        let held = token_with_exp(90_000);
        let offered = token_with_exp(90_060);
        assert_eq!(
            next_token(
                &session(Some("guest:a"), Some(&held)),
                "guest:a",
                &[Some(&offered)],
                1_000
            ),
            Some(held.as_str())
        );
    }

    #[test]
    fn next_token_upgrades_to_a_much_longer_lived_token() {
        // Admission: the 30-minute observer token yields to the room token.
        let observer = token_with_exp(1_000 + 1_800);
        let room = token_with_exp(1_000 + 86_400);
        assert_eq!(
            next_token(
                &session(Some("guest:a"), Some(&observer)),
                "guest:a",
                &[Some(&room)],
                1_000
            ),
            Some(room.as_str())
        );
    }

    #[test]
    fn next_token_replaces_a_token_that_is_about_to_expire() {
        let nearly_dead = token_with_exp(1_060);
        let offered = token_with_exp(1_100);
        assert_eq!(
            next_token(
                &session(Some("guest:a"), Some(&nearly_dead)),
                "guest:a",
                &[Some(&offered)],
                1_000
            ),
            Some(offered.as_str())
        );
    }

    #[test]
    fn next_token_discards_the_stored_token_when_the_identity_changed() {
        let stored_token = token_with_exp(9_000);
        assert_eq!(
            next_token(
                &session(Some("guest:old"), Some(&stored_token)),
                "guest:new",
                &[None, None],
                1_000
            ),
            None
        );
    }

    #[test]
    fn next_token_drops_expired_candidates() {
        let expired = token_with_exp(500);
        let live = token_with_exp(9_000);
        assert_eq!(
            next_token(
                &GuestSession::default(),
                "guest:a",
                &[Some(&expired), Some(&live)],
                1_000
            ),
            Some(live.as_str())
        );
        assert_eq!(
            next_token(
                &GuestSession::default(),
                "guest:a",
                &[Some(&expired)],
                1_000
            ),
            None
        );
    }

    #[test]
    fn next_token_accepts_a_token_with_no_readable_expiry() {
        assert_eq!(
            next_token(
                &GuestSession::default(),
                "guest:a",
                &[Some("opaque-token")],
                1_000
            ),
            Some("opaque-token")
        );
    }

    #[test]
    fn an_aged_out_token_is_left_alone_when_the_identity_is_unchanged() {
        // `next_token` answers None for an expired stored token with nothing
        // offered. That means "nothing to keep", never "destroy what is there".
        let aged_out = token_with_exp(1_000);
        assert_eq!(
            token_write_action(&session(Some("guest:a"), Some(&aged_out)), None, false),
            TokenWrite::Leave
        );
    }

    #[test]
    fn a_rotated_identity_removes_the_stored_token() {
        let old = token_with_exp(9_000);
        assert_eq!(
            token_write_action(&session(Some("guest:old"), Some(&old)), None, true),
            TokenWrite::Remove
        );
    }

    #[test]
    fn a_different_token_is_written() {
        let held = token_with_exp(9_000);
        let fresh = token_with_exp(99_000);
        assert_eq!(
            token_write_action(&session(Some("guest:a"), Some(&held)), Some(&fresh), false),
            TokenWrite::Write(fresh.as_str())
        );
    }

    #[test]
    fn an_unchanged_token_is_not_rewritten() {
        let held = token_with_exp(9_000);
        assert_eq!(
            token_write_action(&session(Some("guest:a"), Some(&held)), Some(&held), false),
            TokenWrite::Leave
        );
    }
}
