// SPDX-License-Identifier: MIT OR Apache-2.0

//! Renews the local participant's presence lease on an interval while they sit
//! in the waiting room, or linger in the pre-join lobby before
//! `PRESENCE_CONNECT_WINDOW_SECS` expires the REST `/join` admission.

use dioxus::prelude::*;
use futures::future::{select, Either};
use gloo_timers::callback::Interval;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::meeting_api::JoinError;

/// What a keepalive attempt's result implies for the local scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepaliveOutcome {
    /// Keep the interval armed; try again next tick.
    Continue,
    /// The server said there is nothing left to renew (neither waiting nor
    /// admitted, already connected, or the meeting ended) — stop calling.
    Stop,
}

/// A 404 (`JoinError::NotFound`) stops the loop; anything else continues it.
pub fn classify_keepalive_result(result: &Result<(), JoinError>) -> KeepaliveOutcome {
    match result {
        Err(JoinError::NotFound(_)) => KeepaliveOutcome::Stop,
        _ => KeepaliveOutcome::Continue,
    }
}

/// How long one keepalive may stay pending before it is abandoned, so the
/// in-flight guard is clear again before the next tick.
fn request_deadline_ms(interval_ms: u32) -> u32 {
    interval_ms - interval_ms / 4
}

/// Renders nothing. Each tick sends the latest `observer_token` prop without
/// restarting the interval. `interval_ms` is a test-only seam; production call
/// sites leave it `None`.
#[component]
pub fn PresenceKeepalive(
    meeting_id: String,
    is_guest: bool,
    observer_token: String,
    meeting_joined: Signal<bool>,
    #[props(default = true)] stop_on_not_found: bool,
    #[props(default)] interval_ms: Option<u32>,
) -> Element {
    type KeepaliveCell = Rc<RefCell<Option<Interval>>>;
    let cell: KeepaliveCell = use_hook(|| Rc::new(RefCell::new(None)));
    let cell_effect = cell.clone();
    let in_flight: Rc<Cell<bool>> = use_hook(|| Rc::new(Cell::new(false)));
    let latest_token: Rc<RefCell<String>> = use_hook(|| Rc::new(RefCell::new(String::new())));
    *latest_token.borrow_mut() = observer_token;
    use_effect(move || {
        if meeting_joined() {
            *cell_effect.borrow_mut() = None;
            return;
        }
        if cell_effect.borrow().is_some() {
            return;
        }
        let ms = interval_ms.unwrap_or(
            (videocall_meeting_types::presence::PRESENCE_HEARTBEAT_INTERVAL_SECS * 1000) as u32,
        );
        let deadline_ms = request_deadline_ms(ms);
        let fire = {
            let meeting_id = meeting_id.clone();
            let latest_token = latest_token.clone();
            let stop = cell_effect.clone();
            let in_flight = in_flight.clone();
            move || {
                if meeting_joined() {
                    return;
                }
                if in_flight.get() {
                    return;
                }
                in_flight.set(true);
                let meeting_id = meeting_id.clone();
                let observer_token = latest_token.borrow().clone();
                let stop = stop.clone();
                let in_flight = in_flight.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let request = async {
                        if is_guest {
                            crate::meeting_api::presence_keepalive_guest(
                                &meeting_id,
                                &observer_token,
                            )
                            .await
                        } else {
                            crate::meeting_api::presence_keepalive(&meeting_id).await
                        }
                    };
                    let deadline = gloo_timers::future::TimeoutFuture::new(deadline_ms);
                    let result = match select(std::pin::pin!(request), deadline).await {
                        Either::Left((result, _)) => result,
                        Either::Right(((), _)) => {
                            in_flight.set(false);
                            log::warn!(
                                "[presence-keepalive] {meeting_id}: abandoned after {deadline_ms}ms"
                            );
                            return;
                        }
                    };
                    in_flight.set(false);
                    if let Err(e) = &result {
                        log::warn!("[presence-keepalive] {meeting_id}: {e:?}");
                    }
                    if stop_on_not_found
                        && classify_keepalive_result(&result) == KeepaliveOutcome::Stop
                    {
                        log::debug!(
                            "[presence-keepalive] {meeting_id}: nothing left to renew, stopping"
                        );
                        *stop.borrow_mut() = None;
                    }
                });
            }
        };
        fire();
        let interval = Interval::new(ms, fire);
        *cell_effect.borrow_mut() = Some(interval);
    });
    use_drop(move || {
        *cell.borrow_mut() = None;
    });
    rsx! {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_stops_the_loop() {
        assert_eq!(
            classify_keepalive_result(&Err(JoinError::NotFound("gone".into()))),
            KeepaliveOutcome::Stop
        );
    }

    #[test]
    fn success_continues_the_loop() {
        assert_eq!(
            classify_keepalive_result(&Ok(())),
            KeepaliveOutcome::Continue
        );
    }

    #[test]
    fn a_hung_request_is_abandoned_before_the_next_tick() {
        let production_ms =
            (videocall_meeting_types::presence::PRESENCE_HEARTBEAT_INTERVAL_SECS * 1000) as u32;
        for interval_ms in [150, 400, production_ms] {
            let deadline = request_deadline_ms(interval_ms);
            assert!(
                deadline > 0 && deadline < interval_ms,
                "deadline {deadline}ms must fall inside the {interval_ms}ms interval"
            );
        }
    }

    #[test]
    fn a_transient_error_continues_the_loop() {
        assert_eq!(
            classify_keepalive_result(&Err(JoinError::NotAuthenticated)),
            KeepaliveOutcome::Continue,
            "not the stop signal — worth trying again next tick"
        );
    }
}
