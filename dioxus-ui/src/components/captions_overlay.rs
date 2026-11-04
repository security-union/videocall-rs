// SPDX-License-Identifier: MIT OR Apache-2.0

//! Live-caption surfaces: the shared [`CaptionsHandle`], the caption lane, the
//! "Live captions on" notice, and the host's renderless renewal loop.
//!
//! The board signal is read only by [`CaptionsOverlay`], so a caption update
//! re-renders that small scope and never the attendants grid.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use dioxus::prelude::*;
use futures::channel::mpsc::{self, UnboundedReceiver};
use futures::future::{select, Either};
use futures::StreamExt;
use gloo_timers::callback::Timeout;
use gloo_timers::future::TimeoutFuture;
use videocall_client::{CaptionEvent, CaptionStatus, CaptionUpdate};

use crate::components::co_hosts::{post_co_host_notice, CoHostNoticeCtx};
use crate::console_log_collector::set_console_log_auth_token_for;

use crate::components::captions::{
    caption_language, captions_available, captions_button_state, captions_overlay_visible,
    captions_start_failure_text, captions_toggle, classify_captions_post, current_room_token,
    language_badge, next_renew_delay_ms, notice_announcement, notice_for_status,
    remember_room_token, renews_after, revoke_needs_retry, same_language, CaptionBoard,
    CaptionEffect, CaptionLine, CaptionStarter, CaptionsButtonState, CaptionsPostOutcome,
    CaptionsRole, CaptionsToggle, HostRejection, HostRejectionEvent, RefreshFailures,
    CAPTIONS_LANGUAGE_PREF_KEY, CAPTIONS_RENEW_INTERVAL_MS, CAPTIONS_REVOKE_RETRY_MS,
    CAPTIONS_SHOW_PREF_KEY, CAPTION_LANGUAGES,
};

/// Every piece of caption state one meeting view shares.
#[derive(Clone)]
pub struct CaptionsHandle {
    pub board: Signal<CaptionBoard>,
    /// `Some(starter)` while the server reports the room captioned.
    pub notice: Signal<Option<CaptionStarter>>,
    /// This host asked for captions and is renewing the lease.
    pub room_on: Signal<bool>,
    /// Whether the server accepted this host's latest enable.
    pub enable_accepted: Signal<bool>,
    /// A participant's local overlay preference.
    pub show: Signal<bool>,
    pub server_disabled: Signal<bool>,
    /// The server's `NOT_HOST`s: act as a viewer while the room is captioned.
    host_rejection: Signal<HostRejection>,
    is_host: ReadSignal<bool>,
    flag: bool,
    e2ee: bool,
    /// The viewer's caption language (ISO 639-1).
    pub language: Signal<String>,
    status_timer: Rc<RefCell<Option<Timeout>>>,
    status_timeout_ms: u32,
    post_lock: Rc<futures::lock::Mutex<()>>,
    /// Bumped on every start or revoke click, so a newer click ends an older
    /// revoke's retries.
    control_gen: Rc<Cell<u32>>,
    /// Set by a revoke click or a POST outcome that stops renewing, except
    /// `NOT_HOST`; cleared by a start click, a stopped status, or a status
    /// timeout (once any retrying revoke ends). While set, a status never
    /// re-adopts the lease.
    revoked_until_stopped: Rc<Cell<bool>>,
    /// The retrying revoke's click generation, and whether the status timed
    /// out while it ran.
    revoke_ladder: Rc<Cell<Option<(u32, bool)>>>,
    refresh_failures: Rc<Cell<RefreshFailures>>,
}

impl PartialEq for CaptionsHandle {
    fn eq(&self, other: &Self) -> bool {
        self.board == other.board && Rc::ptr_eq(&self.status_timer, &other.status_timer)
    }
}

/// Creates the caption state. `flag` and `e2ee` gate caption TEXT and the CC
/// control only; status packets drive the notice regardless.
pub fn use_captions(
    flag: bool,
    e2ee: bool,
    is_host: ReadSignal<bool>,
    status_timeout_ms: u32,
) -> CaptionsHandle {
    let board = use_signal(CaptionBoard::default);
    let notice = use_signal(|| None::<CaptionStarter>);
    let mut room_on = use_signal(|| false);
    let mut enable_accepted = use_signal(|| false);
    let show = use_signal(|| flag && crate::local_storage::load_bool(CAPTIONS_SHOW_PREF_KEY, true));
    let server_disabled = use_signal(|| false);
    let host_rejection = use_signal(HostRejection::default);
    let language = use_signal(|| {
        let saved: String =
            crate::local_storage::load_json(CAPTIONS_LANGUAGE_PREF_KEY, String::new());
        caption_language(Some(saved.as_str()), &browser_languages()).to_string()
    });
    let status_timer: Rc<RefCell<Option<Timeout>>> = use_hook(|| Rc::new(RefCell::new(None)));
    let post_lock = use_hook(|| Rc::new(futures::lock::Mutex::new(())));
    let control_gen = use_hook(|| Rc::new(Cell::new(0)));
    let revoked_until_stopped = use_hook(|| Rc::new(Cell::new(false)));
    let revoke_ladder = use_hook(|| Rc::new(Cell::new(None)));
    let refresh_failures = use_hook(|| Rc::new(Cell::new(RefreshFailures::default())));

    use_effect(move || {
        let host = is_host();
        if !host && *room_on.peek() {
            room_on.set(false);
            enable_accepted.set(false);
        }
        apply_host_event(host_rejection, HostRejectionEvent::HostChanged);
    });
    {
        let status_timer = status_timer.clone();
        use_drop(move || {
            status_timer.borrow_mut().take();
        });
    }

    CaptionsHandle {
        board,
        notice,
        room_on,
        enable_accepted,
        show,
        server_disabled,
        host_rejection,
        is_host,
        flag,
        e2ee,
        language,
        status_timer,
        status_timeout_ms,
        post_lock,
        control_gen,
        revoked_until_stopped,
        revoke_ladder,
        refresh_failures,
    }
}

/// The browser's preferred languages, most preferred first.
fn browser_languages() -> Vec<String> {
    web_sys::window()
        .map(|w| w.navigator().languages())
        .map(|langs| langs.iter().filter_map(|l| l.as_string()).collect())
        .unwrap_or_default()
}

impl CaptionsHandle {
    /// Shows captions in `code` from now on and remembers the choice; a
    /// code that is not offered is ignored.
    pub fn set_language(&self, code: &str) {
        let Some((code, _)) = CAPTION_LANGUAGES.iter().find(|(c, _)| *c == code) else {
            log::warn!("[captions] ignoring unknown caption language {code:?}");
            return;
        };
        crate::local_storage::save_json(CAPTIONS_LANGUAGE_PREF_KEY, &code.to_string());
        let mut language = self.language;
        if *language.peek() != *code {
            language.set(code.to_string());
        }
    }

    /// Whether the CC control and caption text exist. Subscribes the caller.
    pub fn available(&self) -> bool {
        captions_available(self.flag, self.e2ee, (self.server_disabled)())
    }

    pub fn role(&self) -> CaptionsRole {
        let demoted = (self.host_rejection)().demotes(self.notice.read().is_some());
        CaptionsRole::from_host((self.is_host)() && !demoted)
    }

    pub fn button_state(&self) -> CaptionsButtonState {
        captions_button_state(
            self.role(),
            (self.room_on)(),
            (self.enable_accepted)(),
            self.notice.read().is_some(),
            (self.show)(),
        )
    }

    fn peek_role(&self) -> CaptionsRole {
        self.peek_role_when(self.notice.try_peek().is_ok_and(|n| n.is_some()))
    }

    fn peek_role_when(&self, captioned: bool) -> CaptionsRole {
        let demoted = self
            .host_rejection
            .try_peek()
            .is_ok_and(|r| r.demotes(captioned));
        CaptionsRole::from_host(*self.is_host.peek() && !demoted)
    }

    /// Entry point for the client's `on_caption` callback. A no-op once the
    /// owning component is gone.
    pub fn on_update(&self, update: CaptionUpdate) {
        let Ok(active) = self.notice.try_peek().map(|n| n.is_some()) else {
            return;
        };
        match update {
            CaptionUpdate::Status(status) => self.apply_status(&status),
            CaptionUpdate::Text(event) => {
                let accept = captions_available(self.flag, self.e2ee, *self.server_disabled.peek())
                    && captions_overlay_visible(self.peek_role(), active, *self.show.peek());
                if accept {
                    ingest_caption(self.board, event, now_ms());
                }
            }
        }
    }

    fn apply_status(&self, status: &CaptionStatus) {
        let next = notice_for_status(status);
        if next.is_some() {
            set_if_alive(self.server_disabled, false);
            let own = status.started_by_local && self.room_on.try_peek().is_ok_and(|on| *on);
            self.host_event(if own {
                HostRejectionEvent::OwnStatusActive
            } else {
                HostRejectionEvent::StatusActive
            });
            let me = self.clone();
            *self.status_timer.borrow_mut() =
                Some(Timeout::new(self.status_timeout_ms, move || {
                    match me.revoke_ladder.get() {
                        Some((gen, _)) => me.revoke_ladder.set(Some((gen, true))),
                        None => me.revoked_until_stopped.set(false),
                    }
                    me.host_event(HostRejectionEvent::StatusTimeout);
                    me.set_notice(None)
                }));
            if own {
                set_if_alive(self.enable_accepted, true);
            } else if status.started_by_local
                && !self.revoked_until_stopped.get()
                && self.peek_role_when(true) == CaptionsRole::Host
                && self.room_on.try_peek().is_ok_and(|on| !*on)
            {
                set_if_alive(self.enable_accepted, true);
                set_if_alive(self.room_on, true);
            } else if self.peek_role_when(true) != CaptionsRole::Host {
                set_if_alive(self.room_on, false);
            }
        } else {
            self.host_event(HostRejectionEvent::StatusStopped);
            self.revoked_until_stopped.set(false);
            self.status_timer.borrow_mut().take();
            if self.enable_accepted.try_peek().is_ok_and(|a| *a) {
                set_if_alive(self.room_on, false);
            }
        }
        self.set_notice(next);
    }

    fn host_event(&self, event: HostRejectionEvent) {
        apply_host_event(self.host_rejection, event);
    }

    fn set_notice(&self, next: Option<CaptionStarter>) {
        let mut notice = self.notice;
        match notice.try_peek() {
            Ok(current) if *current != next => {}
            _ => return,
        }
        if next.is_none() {
            let mut board = self.board;
            if board.try_peek().is_ok_and(|b| !b.is_empty()) {
                if let Ok(mut b) = board.try_write() {
                    *b = CaptionBoard::default();
                };
            }
        }
        if let Ok(mut n) = notice.try_write() {
            *n = next;
        };
    }

    /// Activates the CC control.
    pub fn toggle(&self, meeting_id: &str) {
        let state = captions_button_state(
            self.peek_role(),
            *self.room_on.peek(),
            *self.enable_accepted.peek(),
            self.notice.peek().is_some(),
            *self.show.peek(),
        );
        let mut room_on = self.room_on;
        match captions_toggle(state) {
            CaptionsToggle::StartRoom => {
                self.control_gen.set(self.control_gen.get().wrapping_add(1));
                self.revoked_until_stopped.set(false);
                self.revoke_ladder.set(None);
                set_if_alive(self.enable_accepted, false);
                room_on.set(true);
            }
            CaptionsToggle::RevokeRoom => {
                let gen = self.control_gen.get().wrapping_add(1);
                self.control_gen.set(gen);
                self.revoked_until_stopped.set(true);
                set_if_alive(self.enable_accepted, false);
                room_on.set(false);
                self.revoke_ladder.set(Some((gen, false)));
                let me = self.clone();
                let meeting_id = meeting_id.to_string();
                wasm_bindgen_futures::spawn_local(async move {
                    me.revoke(&meeting_id, gen).await;
                    if let Some((g, lapsed)) = me.revoke_ladder.get() {
                        if g == gen {
                            me.revoke_ladder.set(None);
                            if lapsed {
                                me.revoked_until_stopped.set(false);
                            }
                        }
                    }
                });
            }
            CaptionsToggle::SetShow(show) => {
                crate::local_storage::save_bool(CAPTIONS_SHOW_PREF_KEY, show);
                let mut s = self.show;
                s.set(show);
                if !show && !self.board.peek().is_empty() {
                    let mut board = self.board;
                    board.set(CaptionBoard::default());
                }
            }
            CaptionsToggle::Nothing => {}
        }
    }

    async fn revoke(&self, meeting_id: &str, gen: u32) {
        let mut retries = CAPTIONS_REVOKE_RETRY_MS.iter();
        loop {
            let outcome = self
                .post(meeting_id, false, false, CAPTIONS_RENEW_INTERVAL_MS)
                .await;
            self.apply_post_outcome(outcome);
            if !revoke_needs_retry(outcome) {
                return;
            }
            let Some(&delay) = retries.next() else {
                log::warn!("[captions] {meeting_id}: revoke kept failing; giving up");
                return;
            };
            TimeoutFuture::new(delay).await;
            if !self.revoke_still_wanted(gen) {
                return;
            }
        }
    }

    /// A failed revoke is resent only while the view is mounted, no later
    /// start or revoke click has superseded it, and the server has not
    /// reported the room stopped since the click.
    fn revoke_still_wanted(&self, gen: u32) -> bool {
        self.control_gen.get() == gen
            && self.room_on.try_peek().is_ok_and(|on| !*on)
            && self.revoked_until_stopped.get()
    }

    fn post(
        &self,
        meeting_id: &str,
        enabled: bool,
        renew: bool,
        interval_ms: u32,
    ) -> impl std::future::Future<Output = Option<CaptionsPostOutcome>> {
        let lock = self.post_lock.clone();
        let control_gen = self.control_gen.clone();
        let failures = self.refresh_failures.clone();
        let queued_at = control_gen.get();
        let meeting_id = meeting_id.to_string();
        let (tx, rx) = futures::channel::oneshot::channel();
        wasm_bindgen_futures::spawn_local(async move {
            let _turn = lock.lock().await;
            if enabled && control_gen.get() != queued_at {
                return;
            }
            let _ =
                tx.send(post_captions(&meeting_id, enabled, renew, interval_ms, &failures).await);
        });
        async move { rx.await.ok().flatten() }
    }

    /// Returns whether the caller should keep renewing.
    fn apply_post_outcome(&self, outcome: Option<CaptionsPostOutcome>) -> bool {
        match outcome {
            Some(CaptionsPostOutcome::FeatureOff) => {
                log::info!("[captions] captions are disabled on this server");
                self.host_event(HostRejectionEvent::Refused);
                self.revoked_until_stopped.set(true);
                set_if_alive(self.server_disabled, true);
                set_if_alive(self.room_on, false);
                set_if_alive(self.enable_accepted, false);
                false
            }
            Some(CaptionsPostOutcome::NotHost) => {
                log::info!("[captions] the server does not treat us as a host");
                self.host_event(HostRejectionEvent::NotHost);
                set_if_alive(self.room_on, false);
                set_if_alive(self.enable_accepted, false);
                false
            }
            Some(CaptionsPostOutcome::CaptionsOff | CaptionsPostOutcome::MeetingNotActive) => {
                log::info!("[captions] captions are off or the meeting is not active");
                self.host_event(HostRejectionEvent::Refused);
                self.revoked_until_stopped.set(true);
                set_if_alive(self.room_on, false);
                set_if_alive(self.enable_accepted, false);
                false
            }
            Some(CaptionsPostOutcome::Unauthorized | CaptionsPostOutcome::RefreshToken) => {
                log::info!("[captions] the room token was rejected");
                self.host_event(HostRejectionEvent::Refused);
                self.revoked_until_stopped.set(true);
                set_if_alive(self.room_on, false);
                set_if_alive(self.enable_accepted, false);
                false
            }
            Some(CaptionsPostOutcome::MeetingGone) => {
                self.host_event(HostRejectionEvent::Refused);
                self.revoked_until_stopped.set(true);
                set_if_alive(self.room_on, false);
                set_if_alive(self.enable_accepted, false);
                false
            }
            Some(CaptionsPostOutcome::Accepted) => {
                self.host_event(HostRejectionEvent::Accepted);
                true
            }
            Some(CaptionsPostOutcome::RetryNextTick) | None => true,
        }
    }
}

fn apply_host_event(mut rejection: Signal<HostRejection>, event: HostRejectionEvent) {
    let Ok(current) = rejection.try_peek().map(|r| *r) else {
        return;
    };
    let next = current.after(event);
    if next != current {
        if let Ok(mut r) = rejection.try_write() {
            *r = next;
        };
    }
}

fn set_if_alive(mut signal: Signal<bool>, value: bool) {
    if signal.try_peek().is_ok_and(|v| *v != value) {
        if let Ok(mut s) = signal.try_write() {
            *s = value;
        };
    }
}

/// Applies a caption to `board`, writing the signal only when the rendered
/// lines change.
pub fn ingest_caption(mut board: Signal<CaptionBoard>, event: CaptionEvent, now_ms: f64) {
    let effect = match board.try_peek() {
        Ok(b) => b.preview(&event),
        Err(_) => return,
    };
    match effect {
        CaptionEffect::Applied => {
            if let Ok(mut b) = board.try_write() {
                b.apply(event, now_ms);
            }
        }
        CaptionEffect::Refreshed => {
            #[allow(deprecated)]
            board.write_silent().apply(event, now_ms);
        }
        CaptionEffect::Ignored => {}
    }
}

/// The caption lane plus its screen-reader log. `lane_hidden` drops the lane
/// and keeps the log.
#[component]
pub fn CaptionsOverlay(
    board: Signal<CaptionBoard>,
    language: Signal<String>,
    #[props(default)] lane_hidden: bool,
) -> Element {
    let wake = use_hook(move || {
        let (tx, rx) = mpsc::unbounded::<()>();
        spawn(prune_when_due(board, rx));
        tx
    });
    let next_expiry = use_memo(move || board.read().next_expiry_ms());
    use_effect(move || {
        next_expiry();
        let _ = wake.unbounded_send(());
    });

    let lines = board.read().lines().to_vec();
    let viewer = language();
    rsx! {
        if !lane_hidden {
            div {
                class: "captions-overlay",
                "data-testid": "captions-overlay",
                "aria-hidden": "true",
                for (i, line, (text, text_lang)) in lines.iter().enumerate().map(|(i, l)| (i, l, l.shown(&viewer))) {
                    div {
                        key: "{line.dom_key()}",
                        class: caption_line_class(line, i.checked_sub(1).map(|p| &lines[p])),
                        "data-testid": "caption-line",
                        "data-final": if line.is_final { "true" } else { "false" },
                        div { class: "caption-line__body",
                            bdi { class: "caption-line__speaker", "data-testid": "caption-line-speaker", "{line.speaker}" }
                            if !same_language(text_lang, &viewer) {
                                if let Some(badge) = language_badge(text_lang) {
                                    span { class: "caption-line__lang", "data-testid": "caption-line-lang", "{badge}" }
                                }
                            }
                            bdi {
                                class: "caption-line__text",
                                "data-testid": "caption-line-text",
                                lang: if text_lang.is_empty() { None } else { Some(text_lang.to_string()) },
                                "{text}"
                            }
                        }
                    }
                }
            }
        }
        div {
            class: "visually-hidden",
            "data-testid": "captions-live-region",
            role: "log",
            "aria-live": "polite",
            "aria-relevant": "additions",
            // Keyed by the caption's own text, not the viewer's rendering, so a
            // language change re-labels the entries instead of re-announcing them.
            for line in lines.iter().filter(|l| l.is_final && !l.is_local) {
                p {
                    key: "{line.dom_key()}-{line.text}",
                    lang: Some(line.shown(&viewer).1).filter(|l| !l.is_empty()).map(str::to_string),
                    "{line.announcement(&viewer)}"
                }
            }
        }
    }
}

fn caption_line_class(line: &CaptionLine, previous: Option<&CaptionLine>) -> String {
    let state = if line.is_final { "final" } else { "interim" };
    let continued = previous.is_some_and(|p| p.speaker_session_id == line.speaker_session_id);
    format!(
        "caption-line caption-line--{state}{}{}",
        if continued {
            " caption-line--continued"
        } else {
            ""
        },
        if line.is_local {
            " caption-line--local"
        } else {
            ""
        }
    )
}

/// The board's clock: `performance.now()`, which never steps back like the
/// wall clock can, so lines expire on time across a clock correction.
pub fn now_ms() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or_else(js_sys::Date::now)
}

async fn prune_when_due(mut board: Signal<CaptionBoard>, mut wake: UnboundedReceiver<()>) {
    loop {
        let Ok(next) = board.try_peek().map(|b| b.next_expiry_ms()) else {
            return;
        };
        let woken = match next {
            None => wake.next().await.is_some(),
            Some(at) => {
                let wait = (at - now_ms()).max(0.0).ceil() as u32 + 1;
                match select(TimeoutFuture::new(wait), wake.next()).await {
                    Either::Left(_) => {
                        let now = now_ms();
                        if board.try_peek().is_ok_and(|b| b.has_expired(now)) {
                            board.write().prune(now);
                        }
                        true
                    }
                    Either::Right((msg, _)) => msg.is_some(),
                }
            }
        };
        if !woken {
            return;
        }
        while let Ok(Some(())) = wake.try_next() {}
    }
}

/// The renewal loop, the caption lane, and a status region announcing
/// captions starting and stopping. Renewal and the lane render only while
/// captions are available (flag on, no E2EE, no server veto); the region
/// always renders. `lane_hidden` drops only the visible lane.
#[component]
pub fn CaptionsLayer(
    meeting_id: String,
    captions: CaptionsHandle,
    lane_hidden: Memo<bool>,
    #[props(default)] renew_interval_ms: Option<u32>,
) -> Element {
    let was_on: Rc<Cell<bool>> = use_hook(|| Rc::new(Cell::new(false)));
    let notice = captions.notice.read().clone();
    let on = notice.is_some();
    let announcement = notice_announcement(notice.as_ref(), was_on.get());
    was_on.set(was_on.get() || on);
    let available = captions.available();
    let role = captions.role();
    let renewing = available && role == CaptionsRole::Host && (captions.room_on)();
    let lane = available && captions_overlay_visible(role, on, (captions.show)());
    rsx! {
        div {
            class: "visually-hidden",
            "data-testid": "captions-status-region",
            role: "status",
            "aria-live": "polite",
            "{announcement}"
        }
        if renewing {
            CaptionsRenewal {
                meeting_id,
                captions: captions.clone(),
                interval_ms: renew_interval_ms,
            }
        }
        if lane {
            CaptionsOverlay {
                board: captions.board,
                language: captions.language,
                lane_hidden: lane_hidden(),
            }
        }
    }
}

/// The viewer's caption-language choice, next to the "Live captions on"
/// notice.
#[component]
pub fn CaptionsLanguagePicker(captions: CaptionsHandle) -> Element {
    let current = (captions.language)();
    rsx! {
        label { class: "meeting-status-item captions-language",
            span { class: "visually-hidden", "Caption language" }
            span { class: "captions-language__code", "aria-hidden": "true", "{current.to_uppercase()}" }
            select {
                class: "captions-language__select",
                "data-testid": "captions-language-select",
                value: "{current}",
                onchange: move |e| captions.set_language(&e.value()),
                for (code, name) in CAPTION_LANGUAGES {
                    option { key: "{code}", value: "{code}", selected: code == current, "{name}" }
                }
            }
        }
    }
}

/// Shown to every participant while the server reports the room captioned.
#[component]
pub fn CaptionsLiveIndicator(starter: CaptionStarter) -> Element {
    rsx! {
        div {
            class: "meeting-status-item meeting-status-item--captions",
            "data-testid": "captions-live-indicator",
            title: "Speech in this meeting is being transcribed",
            span { class: "captions-live-dot", "aria-hidden": "true" }
            span { class: "meeting-status-item-label",
                "Live captions on"
                match starter {
                    CaptionStarter::Me => rsx! {
                        " \u{b7} "
                        bdi { "data-testid": "captions-live-starter", "you" }
                        " started them"
                    },
                    CaptionStarter::Named(name) if !name.is_empty() => rsx! {
                        " \u{b7} started by "
                        bdi { "data-testid": "captions-live-starter", "{name}" }
                    },
                    CaptionStarter::Named(_) => rsx! {},
                }
            }
        }
    }
}

async fn post_captions(
    meeting_id: &str,
    enabled: bool,
    renew: bool,
    interval_ms: u32,
    failures: &Cell<RefreshFailures>,
) -> Option<CaptionsPostOutcome> {
    let refreshing = Cell::new(false);
    let request = async {
        let mut outcome = send_captions_post(meeting_id, enabled, renew).await;
        if outcome == CaptionsPostOutcome::RefreshToken {
            refreshing.set(true);
            let refreshed = crate::meeting_api::refresh_room_token(meeting_id).await;
            refreshing.set(false);
            outcome = match refreshed {
                Ok(token) => {
                    failures.set(RefreshFailures::default());
                    set_console_log_auth_token_for(meeting_id, &token);
                    remember_room_token(meeting_id, &token);
                    send_captions_post(meeting_id, enabled, renew).await
                }
                Err(e) => {
                    log::debug!("[captions] {meeting_id}: room token refresh failed: {e:?}");
                    let mut f = failures.get();
                    let outcome = f.record(&e);
                    failures.set(f);
                    outcome
                }
            };
        }
        if outcome == CaptionsPostOutcome::Accepted {
            failures.set(RefreshFailures::default());
        }
        outcome
    };
    let deadline = TimeoutFuture::new(interval_ms - interval_ms / 4);
    let refresh_timed_out = match select(std::pin::pin!(request), deadline).await {
        Either::Left((outcome, _)) => return Some(outcome),
        Either::Right(_) => refreshing.get(),
    };
    if !refresh_timed_out {
        return None;
    }
    let mut f = failures.get();
    let outcome = f.record_timeout();
    failures.set(f);
    (outcome == CaptionsPostOutcome::Unauthorized).then_some(outcome)
}

async fn send_captions_post(meeting_id: &str, enabled: bool, renew: bool) -> CaptionsPostOutcome {
    let token = current_room_token(meeting_id).unwrap_or_default();
    let result = crate::meeting_api::set_captions(meeting_id, &token, enabled, renew).await;
    if let Err(e) = &result {
        log::debug!("[captions] {meeting_id}: enabled={enabled} renew={renew} failed: {e:?}");
    }
    classify_captions_post(&result)
}

/// Renders nothing. While mounted, enables captions now (or renews them, when
/// adopting an accepted lease) and keeps renewing. Unmounting stops renewing
/// without a POST.
#[component]
fn CaptionsRenewal(
    meeting_id: String,
    captions: CaptionsHandle,
    #[props(default)] interval_ms: Option<u32>,
) -> Element {
    let ms = interval_ms.unwrap_or(CAPTIONS_RENEW_INTERVAL_MS);
    let failure_notice = try_use_context::<CoHostNoticeCtx>();
    use_hook(move || {
        let mut renew = *captions.enable_accepted.peek();
        spawn(async move {
            loop {
                let outcome = captions.post(&meeting_id, true, renew, ms).await;
                if outcome == Some(CaptionsPostOutcome::Accepted) {
                    set_if_alive(captions.enable_accepted, true);
                }
                if let (Some(ctx), Some(text)) =
                    (failure_notice, captions_start_failure_text(outcome))
                {
                    if captions.enable_accepted.try_peek().is_ok_and(|a| !*a) {
                        post_co_host_notice(ctx, text, true);
                    }
                }
                if !captions.apply_post_outcome(outcome) {
                    return;
                }
                renew = renews_after(renew, outcome);
                TimeoutFuture::new(next_renew_delay_ms(ms, outcome, js_sys::Math::random())).await;
            }
        })
    });
    rsx! {}
}
