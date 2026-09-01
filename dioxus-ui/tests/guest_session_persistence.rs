// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Issue #2331: the real `sessionStorage` round trip behind
// `dioxus_ui::guest_session`. The decision logic is pinned in `src/`.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use dioxus_ui::auth::{check_session, clear_access_token, clear_id_token};
use dioxus_ui::guest_session::{
    clear, clear_all, clear_legacy_marker, has_any, load, remember, resume,
};
use wasm_bindgen_test::*;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

/// Far enough ahead that the token is live for any plausible clock.
const LIVE_EXP: u64 = 9_999_999_999;
/// 2001-09-09 — expired for any plausible clock.
const DEAD_EXP: u64 = 1_000_000_000;

fn token(sub: &str, exp: u64) -> String {
    let payload = URL_SAFE_NO_PAD.encode(format!(r#"{{"sub":"{sub}","exp":{exp}}}"#));
    format!("header.{payload}.signature")
}

fn session_storage() -> web_sys::Storage {
    web_sys::window()
        .unwrap()
        .session_storage()
        .unwrap()
        .unwrap()
}

#[wasm_bindgen_test]
fn remember_then_load_round_trips_the_pair() {
    clear_all();
    let room = token("guest:a", LIVE_EXP);
    remember("meeting-round-trip", "guest:a", Some(&room), None);

    let stored = load("meeting-round-trip");
    assert_eq!(stored.user_id.as_deref(), Some("guest:a"));
    assert_eq!(stored.token.as_deref(), Some(room.as_str()));
    assert_eq!(
        resume("meeting-round-trip"),
        Some(("guest:a".to_string(), room))
    );
}

#[wasm_bindgen_test]
fn identities_are_scoped_per_meeting() {
    clear_all();
    let token_a = token("guest:a", LIVE_EXP);
    let token_b = token("guest:b", LIVE_EXP);
    remember("meeting-a", "guest:a", Some(&token_a), None);
    remember("meeting-b", "guest:b", Some(&token_b), None);

    assert_eq!(load("meeting-a").user_id.as_deref(), Some("guest:a"));
    assert_eq!(load("meeting-b").user_id.as_deref(), Some("guest:b"));
    // Nothing to resume for a meeting this tab has never joined.
    assert_eq!(resume("meeting-c"), None);

    clear("meeting-a");
    assert_eq!(load("meeting-a"), Default::default());
    assert_eq!(load("meeting-b").token.as_deref(), Some(token_b.as_str()));
}

#[wasm_bindgen_test]
fn a_fresh_identity_replaces_the_id_and_drops_the_old_token() {
    clear_all();
    let old = token("guest:old", LIVE_EXP);
    remember("meeting-rotate", "guest:old", Some(&old), None);

    // What the server answers when the presented token no longer proves the
    // presented id: a different `user_id`, and no token yet (still waiting).
    remember("meeting-rotate", "guest:new", None, None);

    let stored = load("meeting-rotate");
    assert_eq!(stored.user_id.as_deref(), Some("guest:new"));
    assert_eq!(stored.token, None, "old identity's token must not survive");
    assert_eq!(resume("meeting-rotate"), None);
}

#[wasm_bindgen_test]
fn an_expired_token_is_never_stored_or_replayed() {
    clear_all();
    let dead = token("guest:a", DEAD_EXP);
    remember("meeting-expired", "guest:a", Some(&dead), None);

    assert_eq!(load("meeting-expired").user_id.as_deref(), Some("guest:a"));
    assert_eq!(load("meeting-expired").token, None);
    assert_eq!(resume("meeting-expired"), None);

    // …and the identity the server mints instead is what gets persisted.
    let fresh = token("guest:fresh", LIVE_EXP);
    remember("meeting-expired", "guest:fresh", None, Some(&fresh));
    assert_eq!(
        resume("meeting-expired"),
        Some(("guest:fresh".to_string(), fresh))
    );
}

#[wasm_bindgen_test]
fn the_longest_lived_token_of_a_response_is_kept() {
    clear_all();
    let observer = token("guest:a", 1_900_000_000);
    let room = token("guest:a", 2_000_000_000);
    remember("meeting-freshest", "guest:a", Some(&room), Some(&observer));
    assert_eq!(
        load("meeting-freshest").token.as_deref(),
        Some(room.as_str())
    );

    // A later status poll that carries only the shorter-lived observer token
    // must not downgrade what is stored.
    remember("meeting-freshest", "guest:a", None, Some(&observer));
    assert_eq!(
        load("meeting-freshest").token.as_deref(),
        Some(room.as_str())
    );
}

#[wasm_bindgen_test]
fn clear_all_sweeps_every_meeting_and_the_pre_2331_key() {
    clear_all();
    assert!(!has_any());

    remember(
        "meeting-sweep-1",
        "guest:a",
        Some(&token("guest:a", LIVE_EXP)),
        None,
    );
    remember(
        "meeting-sweep-2",
        "guest:b",
        Some(&token("guest:b", LIVE_EXP)),
        None,
    );
    // The global key older builds wrote; `check_session` must still see and
    // evict it after an upgrade.
    session_storage()
        .set_item("vc_guest_session_id", "guest:legacy")
        .unwrap();
    assert!(has_any());

    clear_all();
    assert!(!has_any());
    assert_eq!(load("meeting-sweep-1"), Default::default());
    assert_eq!(load("meeting-sweep-2"), Default::default());
    assert_eq!(
        session_storage().get_item("vc_guest_session_id").unwrap(),
        None
    );
}

#[wasm_bindgen_test]
fn unrelated_session_storage_keys_are_left_alone() {
    clear_all();
    session_storage()
        .set_item("vc_access_token", "keep-me")
        .unwrap();
    remember(
        "meeting-untouched",
        "guest:a",
        Some(&token("guest:a", LIVE_EXP)),
        None,
    );

    clear_all();
    assert_eq!(
        session_storage()
            .get_item("vc_access_token")
            .unwrap()
            .as_deref(),
        Some("keep-me")
    );
    session_storage().remove_item("vc_access_token").unwrap();
}

/// Seed the raw keys: `remember` refuses to store an expired token, so a token
/// stored live that has since aged out can only be written directly.
fn seed_raw(meeting_id: &str, user_id: &str, token: &str) {
    let storage = session_storage();
    storage
        .set_item(&format!("vc_guest_id:{meeting_id}"), user_id)
        .unwrap();
    storage
        .set_item(&format!("vc_guest_token:{meeting_id}"), token)
        .unwrap();
    // Fails loudly if the key format drifts, rather than passing vacuously.
    let seeded = load(meeting_id);
    assert_eq!(seeded.user_id.as_deref(), Some(user_id));
    assert_eq!(seeded.token.as_deref(), Some(token));
}

#[wasm_bindgen_test]
fn an_aged_out_token_survives_a_response_that_carries_none() {
    clear_all();
    let dead = token("guest:a", DEAD_EXP);
    seed_raw("meeting-aged", "guest:a", &dead);

    // A status poll for the SAME identity that carries no token of its own.
    remember("meeting-aged", "guest:a", None, None);

    let stored = load("meeting-aged");
    assert_eq!(stored.user_id.as_deref(), Some("guest:a"));
    assert_eq!(
        stored.token.as_deref(),
        Some(dead.as_str()),
        "expiry alone must not delete an unchanged identity's token"
    );
    // Expiry is still enforced where it matters: this is not replayed.
    assert_eq!(resume("meeting-aged"), None);
}

#[wasm_bindgen_test]
fn a_rotated_identity_still_deletes_an_aged_out_token() {
    clear_all();
    let dead = token("guest:a", DEAD_EXP);
    seed_raw("meeting-aged-rotate", "guest:a", &dead);

    // The server answered with a different identity: the old one's token must
    // not outlive it, expired or not.
    remember("meeting-aged-rotate", "guest:b", None, None);

    let stored = load("meeting-aged-rotate");
    assert_eq!(stored.user_id.as_deref(), Some("guest:b"));
    assert_eq!(stored.token, None);
}

/// The shape that makes `check_session` take its guest fast path.
fn inject_pkce_config() {
    let config = js_sys::Object::new();
    let set = |key: &str, val: &wasm_bindgen::JsValue| {
        js_sys::Reflect::set(&config, &key.into(), val).unwrap();
    };
    set("apiBaseUrl", &"http://test:8080".into());
    set("wsUrl", &"ws://test:8080".into());
    set("webTransportHost", &"https://test:4433".into());
    set("oauthEnabled", &"true".into());
    set("e2eeEnabled", &"false".into());
    set("webTransportEnabled", &"false".into());
    set("usersAllowedToStream", &"".into());
    set("serverElectionPeriodMs", &wasm_bindgen::JsValue::from(2000));
    set("oauthFlow", &"pkce".into());
    set("oauthClientId", &"videocall-test-client".into());
    let frozen = js_sys::Object::freeze(&config);
    js_sys::Reflect::set(&gloo_utils::window(), &"__APP_CONFIG".into(), &frozen).unwrap();
    // One wasm module serves every case, so clear the `app_config()` memo (#1492).
    dioxus_ui::constants::reset_config_cache_for_test();
}

/// The #2331 regression: `check_session` runs on every meeting-page and
/// chat-sidebar mount, and swept the identity of the guest sitting in the call.
#[wasm_bindgen_test]
async fn check_session_leaves_an_admitted_guest_its_identity() {
    clear_all();
    clear_access_token();
    clear_id_token();
    inject_pkce_config();

    let room = token("guest:a", LIVE_EXP);
    remember("meeting-mid-call-auth", "guest:a", Some(&room), None);

    // Guest fast path: no OAuth token, so this returns without a round trip.
    assert!(
        check_session().await.is_err(),
        "a guest holds no OAuth session"
    );

    assert_eq!(
        resume("meeting-mid-call-auth"),
        Some(("guest:a".to_string(), room)),
        "check_session must not revoke the identity the guest is mid-meeting on"
    );
}

#[wasm_bindgen_test]
fn clear_legacy_marker_spares_the_per_meeting_identity() {
    clear_all();
    let room = token("guest:a", LIVE_EXP);
    remember("meeting-mid-call", "guest:a", Some(&room), None);
    session_storage()
        .set_item("vc_guest_session_id", "guest:legacy")
        .unwrap();

    // What `check_session` runs on every meeting-page and chat-sidebar mount.
    clear_legacy_marker();

    assert_eq!(
        session_storage().get_item("vc_guest_session_id").unwrap(),
        None,
        "the pre-#2331 marker is still evicted"
    );
    assert_eq!(
        resume("meeting-mid-call"),
        Some(("guest:a".to_string(), room)),
        "an admitted guest keeps the identity it is mid-meeting on"
    );
}
