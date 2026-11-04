// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure, host-testable state for live captions: the on-screen caption lines,
//! the transcription notice, the CC-control role and toggle rules, and the
//! control-POST outcome policy.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};

use videocall_client::{CaptionEvent, CaptionStatus, CaptionTranslation};
use videocall_meeting_types::{error::APIError, responses::APIResponse};

use crate::meeting_api::JoinError;

/// Most caption lines shown at once (finals and interims together).
pub const MAX_CAPTION_LINES: usize = 3;
/// How long a final line stays after its last update.
pub const CAPTION_FINAL_LINGER_MS: f64 = 8_000.0;
/// An interim with no update for this long is dropped (its final never came).
pub const CAPTION_INTERIM_STALE_MS: f64 = 15_000.0;
/// The notice turns off this long after the last status packet.
pub const CAPTIONS_STATUS_TIMEOUT_MS: u32 = 35_000;
/// Host lease-renewal cadence; the server keeps a room captioned 120 s.
pub const CAPTIONS_RENEW_INTERVAL_MS: u32 = 40_000;
/// Renewal jitter, as a fraction of the interval either way.
pub const CAPTIONS_RENEW_JITTER: f64 = 0.10;
/// Waits before resending a revoke that failed transiently. Together they
/// outlast the server's 120 s lease.
pub const CAPTIONS_REVOKE_RETRY_MS: [u32; 8] =
    [1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000, 30_000];
/// localStorage key for a participant's local show/hide preference.
pub const CAPTIONS_SHOW_PREF_KEY: &str = "vc_captions_show";
/// localStorage key for a participant's caption language.
pub const CAPTIONS_LANGUAGE_PREF_KEY: &str = "vc_captions_language";
/// Caption languages a viewer can pick: ISO 639-1 code and the language's
/// own name. German is not offered: the English-to-German model is left
/// out of the translator for its license (German speech still translates).
pub const CAPTION_LANGUAGES: [(&str, &str); 4] = [
    ("en", "English"),
    ("es", "Español"),
    ("fr", "Français"),
    ("it", "Italiano"),
];
/// Speaker label for the local user's own captions.
pub const CAPTION_LOCAL_SPEAKER: &str = "You";
/// Tooltip on the participant CC button while the room is not captioned.
pub const CAPTIONS_OFF_HINT: &str = "Captions are off \u{2014} the host can turn them on";

/// Retirements remembered; an older one is forgotten once this many newer
/// utterances have retired, after which a late revision of it would show again.
const RETIRED_KEYS_MAX: usize = 32;
/// An evicted utterance stays gone, whatever revision follows, while remembered.
const EVICTED: u32 = u32::MAX;

/// One rendered caption line, keyed by `(speaker_session_id, utterance_id)`.
#[derive(Clone, Debug, PartialEq)]
pub struct CaptionLine {
    pub speaker_session_id: u64,
    pub utterance_id: u64,
    pub revision: u32,
    pub speaker: String,
    pub is_local: bool,
    pub is_final: bool,
    pub text: String,
    /// Language of `text`, empty when unknown.
    pub language: String,
    pub translations: Vec<CaptionTranslation>,
    pub updated_ms: f64,
}

impl CaptionLine {
    pub fn key(&self) -> (u64, u64) {
        (self.speaker_session_id, self.utterance_id)
    }

    /// Stable DOM key for the line.
    pub fn dom_key(&self) -> String {
        format!("{}-{}", self.speaker_session_id, self.utterance_id)
    }

    /// The text and its language in `language` when the producer rendered
    /// it, else the caption as sent.
    pub fn shown(&self, language: &str) -> (&str, &str) {
        match self
            .translations
            .iter()
            .find(|t| same_language(&t.language, language))
        {
            Some(t) => (&t.text, &t.language),
            None => (&self.text, &self.language),
        }
    }

    /// The screen-reader announcement of [`Self::shown`], with colons and
    /// some colon look-alikes dropped from the speaker.
    pub fn announcement(&self, language: &str) -> String {
        format!(
            "{}: {}",
            self.speaker.replace(
                [
                    ':', '\u{FF1A}', '\u{FE55}', '\u{A789}', '\u{2236}', '\u{FE13}', '\u{05C3}',
                    '\u{02D0}',
                ],
                ""
            ),
            self.shown(language).0
        )
    }
}

/// Whether two BCP-47 tags name the same language (`"en-US"` and `"EN"`).
pub fn same_language(a: &str, b: &str) -> bool {
    let primary = |tag: &str| {
        tag.split(['-', '_'])
            .next()
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let (a, b) = (primary(a), primary(b));
    !a.is_empty() && a.eq_ignore_ascii_case(&b)
}

/// The caption language to show: the saved choice when it is offered, else
/// the first offered browser language, else English.
pub fn caption_language(saved: Option<&str>, browser: &[String]) -> &'static str {
    let offered = |tag: &str| {
        CAPTION_LANGUAGES
            .iter()
            .find(|(code, _)| same_language(code, tag))
            .map(|(code, _)| *code)
    };
    saved
        .and_then(offered)
        .or_else(|| browser.iter().find_map(|tag| offered(tag)))
        .unwrap_or("en")
}

/// What applying a caption would do to a board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptionEffect {
    /// Stale or retired: nothing is stored.
    Ignored,
    /// A newer revision identical to what is shown: only its revision and
    /// linger clock advance.
    Refreshed,
    /// The rendered lines change, or a retirement is recorded.
    Applied,
}

/// The bounded set of caption lines one room view renders.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CaptionBoard {
    lines: Vec<CaptionLine>,
    retired: VecDeque<((u64, u64), u32)>,
}

impl CaptionBoard {
    pub fn lines(&self) -> &[CaptionLine] {
        &self.lines
    }

    /// No lines and no remembered retirements.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty() && self.retired.is_empty()
    }

    /// What [`Self::apply`] would do with `event`, without changing anything.
    pub fn preview(&self, event: &CaptionEvent) -> CaptionEffect {
        let key = (event.speaker_session_id, event.utterance_id);
        if self
            .retired
            .iter()
            .any(|(k, rev)| *k == key && event.revision <= *rev)
        {
            return CaptionEffect::Ignored;
        }
        match self.lines.iter().find(|l| l.key() == key) {
            Some(line) if event.revision <= line.revision => CaptionEffect::Ignored,
            Some(line)
                if line.text == event.text
                    && line.is_final == event.is_final
                    && line.language == event.text_language
                    && line.translations == event.translations =>
            {
                CaptionEffect::Refreshed
            }
            _ => CaptionEffect::Applied,
        }
    }

    /// Applies one caption update and returns its effect. `now_ms` must come
    /// from a monotonic clock (`performance.now()`), like every other `now_ms`
    /// here: a wall clock stepping back would keep lines up for the step.
    pub fn apply(&mut self, event: CaptionEvent, now_ms: f64) -> CaptionEffect {
        let effect = self.preview(&event);
        if effect == CaptionEffect::Ignored {
            return effect;
        }
        let key = (event.speaker_session_id, event.utterance_id);
        let existing = self.lines.iter().position(|l| l.key() == key);
        if let (CaptionEffect::Refreshed, Some(i)) = (effect, existing) {
            self.lines[i].revision = event.revision;
            self.lines[i].updated_ms = now_ms;
            return effect;
        }
        if event.text.is_empty() {
            if let Some(i) = existing {
                self.lines.remove(i);
            }
            self.retire(key, event.revision);
            return effect;
        }
        let line = CaptionLine {
            speaker_session_id: event.speaker_session_id,
            utterance_id: event.utterance_id,
            revision: event.revision,
            speaker: if event.is_local {
                CAPTION_LOCAL_SPEAKER.to_string()
            } else {
                event.speaker_name
            },
            is_local: event.is_local,
            is_final: event.is_final,
            text: event.text,
            language: event.text_language,
            translations: event.translations,
            updated_ms: now_ms,
        };
        match existing {
            Some(i) => self.lines[i] = line,
            None => {
                self.lines.push(line);
                while self.lines.len() > MAX_CAPTION_LINES {
                    let i = self.eviction_index(key);
                    let evicted = self.lines.remove(i);
                    self.retire(evicted.key(), EVICTED);
                }
            }
        }
        effect
    }

    /// The line closest to expiring on its own, never `incoming`: a stale
    /// interim goes before a final that has only just appeared.
    fn eviction_index(&self, incoming: (u64, u64)) -> usize {
        self.lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.key() != incoming)
            .min_by(|a, b| expiry_ms(a.1).total_cmp(&expiry_ms(b.1)))
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    /// Whether [`Self::prune`] would remove anything at `now_ms`.
    pub fn has_expired(&self, now_ms: f64) -> bool {
        self.lines
            .iter()
            .any(|l| now_ms - l.updated_ms >= linger_ms(l))
    }

    /// When the next line expires, if any.
    pub fn next_expiry_ms(&self) -> Option<f64> {
        self.lines.iter().map(expiry_ms).min_by(f64::total_cmp)
    }

    /// Drops lines that have outlived their linger time. Returns whether
    /// anything was removed.
    pub fn prune(&mut self, now_ms: f64) -> bool {
        let before = self.lines.len();
        let mut expired = Vec::new();
        self.lines.retain(|l| {
            let keep = now_ms - l.updated_ms < linger_ms(l);
            if !keep {
                expired.push((l.key(), l.revision));
            }
            keep
        });
        for (key, rev) in expired {
            self.retire(key, rev);
        }
        self.lines.len() != before
    }

    fn retire(&mut self, key: (u64, u64), revision: u32) {
        self.retired.retain(|(k, _)| *k != key);
        self.retired.push_back((key, revision));
        while self.retired.len() > RETIRED_KEYS_MAX {
            self.retired.pop_front();
        }
    }
}

fn linger_ms(line: &CaptionLine) -> f64 {
    if line.is_final {
        CAPTION_FINAL_LINGER_MS
    } else {
        CAPTION_INTERIM_STALE_MS
    }
}

fn expiry_ms(line: &CaptionLine) -> f64 {
    line.updated_ms + linger_ms(line)
}

/// Whether caption TEXT and the CC control exist for this user. The
/// transcription notice does not depend on this.
pub fn captions_available(flag_enabled: bool, e2ee_enabled: bool, server_disabled: bool) -> bool {
    flag_enabled && !e2ee_enabled && !server_disabled
}

/// Who turned the room's captions on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptionStarter {
    Me,
    /// Someone else, by display name (empty when unknown).
    Named(String),
}

pub fn notice_for_status(status: &CaptionStatus) -> Option<CaptionStarter> {
    if !status.active {
        return None;
    }
    Some(if status.started_by_local {
        CaptionStarter::Me
    } else {
        CaptionStarter::Named(status.started_by_name.clone())
    })
}

/// Screen-reader text for the notice: empty until captions have been on.
pub fn notice_announcement(notice: Option<&CaptionStarter>, was_on: bool) -> String {
    match notice {
        Some(CaptionStarter::Me) => "You started live captions".to_string(),
        Some(CaptionStarter::Named(name)) if name.is_empty() => "Live captions started".to_string(),
        Some(CaptionStarter::Named(name)) => format!("Live captions started by {name}"),
        None if was_on => "Live captions stopped".to_string(),
        None => String::new(),
    }
}

/// Which CC control this user gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptionsRole {
    /// Host or co-host: turns room transcription on and off.
    Host,
    /// Everyone else: shows or hides the overlay locally.
    Viewer,
}

impl CaptionsRole {
    pub fn from_host(is_host: bool) -> Self {
        if is_host {
            Self::Host
        } else {
            Self::Viewer
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Viewer => "viewer",
        }
    }
}

/// Rendered state of the CC control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptionsButtonState {
    pub role: CaptionsRole,
    pub pressed: bool,
    pub disabled: bool,
    /// A host's start the server has not accepted yet.
    pub pending: bool,
}

/// `room_on`: this host is keeping a lease. `active`: the server reports the
/// room captioned. `show`: the participant's local overlay preference.
pub fn captions_button_state(
    role: CaptionsRole,
    room_on: bool,
    accepted: bool,
    active: bool,
    show: bool,
) -> CaptionsButtonState {
    match role {
        CaptionsRole::Host => CaptionsButtonState {
            role,
            pressed: (room_on && accepted) || active,
            disabled: false,
            pending: room_on && !accepted && !active,
        },
        CaptionsRole::Viewer => CaptionsButtonState {
            role,
            pressed: active && show,
            disabled: !active,
            pending: false,
        },
    }
}

/// Tooltip and overflow-menu description for the CC control.
pub fn captions_button_hint(state: CaptionsButtonState) -> &'static str {
    match (state.role, state.pressed, state.disabled) {
        (_, _, true) => CAPTIONS_OFF_HINT,
        (CaptionsRole::Host, true, _) => "Turn off captions for everyone",
        (CaptionsRole::Host, false, _) if state.pending => {
            "Turning on captions for everyone\u{2026}"
        }
        (CaptionsRole::Host, false, _) => "Turn on captions for everyone",
        (CaptionsRole::Viewer, true, _) => "Hide captions",
        (CaptionsRole::Viewer, false, _) => "Show captions",
    }
}

/// What activating the CC control does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptionsToggle {
    StartRoom,
    RevokeRoom,
    SetShow(bool),
    Nothing,
}

pub fn captions_toggle(state: CaptionsButtonState) -> CaptionsToggle {
    match (state.role, state.pressed, state.disabled) {
        (_, _, true) => CaptionsToggle::Nothing,
        (CaptionsRole::Host, true, _) => CaptionsToggle::RevokeRoom,
        (CaptionsRole::Host, false, _) if state.pending => CaptionsToggle::RevokeRoom,
        (CaptionsRole::Host, false, _) => CaptionsToggle::StartRoom,
        (CaptionsRole::Viewer, pressed, _) => CaptionsToggle::SetShow(!pressed),
    }
}

/// Whether the overlay renders: the room must be captioned, and a viewer
/// must not have hidden it.
pub fn captions_overlay_visible(role: CaptionsRole, active: bool, show: bool) -> bool {
    active && (role == CaptionsRole::Host || show)
}

/// Delay before the next renewal: `base_ms` moved by up to
/// [`CAPTIONS_RENEW_JITTER`] either way. `unit_random` is in `[0, 1)`.
pub fn renew_delay_ms(base_ms: u32, unit_random: f64) -> u32 {
    let offset = (unit_random.clamp(0.0, 1.0) * 2.0 - 1.0) * CAPTIONS_RENEW_JITTER;
    (base_ms as f64 * (1.0 + offset)).round() as u32
}

pub fn next_renew_delay_ms(
    base_ms: u32,
    outcome: Option<CaptionsPostOutcome>,
    unit_random: f64,
) -> u32 {
    match outcome {
        None => 0,
        Some(CaptionsPostOutcome::RetryNextTick) => renew_delay_ms(base_ms / 4, unit_random),
        Some(_) => renew_delay_ms(base_ms, unit_random),
    }
}

/// What a captions POST result means for the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptionsPostOutcome {
    Accepted,
    /// The server has captions switched off: stop and hide the control.
    FeatureOff,
    /// The meeting is missing or ended: stop renewing.
    MeetingGone,
    /// The server does not consider us a host: stop and act as a viewer.
    NotHost,
    /// The room was revoked: stop and drop this host's on-state.
    CaptionsOff,
    /// The meeting is not active: stop and drop this host's on-state.
    MeetingNotActive,
    /// The room token was rejected: refresh it and retry once; rejected
    /// again, stop.
    RefreshToken,
    /// The room token could not be refreshed: stop.
    Unauthorized,
    /// Anything else, including transport errors: retry on the next tick.
    RetryNextTick,
}

fn error_code(body: &str) -> Option<String> {
    serde_json::from_str::<APIResponse<APIError>>(body)
        .ok()
        .map(|r| r.result.code)
}

pub fn classify_captions_post(result: &Result<(), JoinError>) -> CaptionsPostOutcome {
    match result {
        Ok(()) => CaptionsPostOutcome::Accepted,
        // A code-less 404 is a meeting-api without the route at all.
        Err(JoinError::NotFound(body))
            if matches!(error_code(body).as_deref(), Some("NOT_FOUND") | None) =>
        {
            CaptionsPostOutcome::FeatureOff
        }
        Err(JoinError::NotFound(body))
            if error_code(body).as_deref() == Some("MEETING_NOT_FOUND") =>
        {
            CaptionsPostOutcome::MeetingGone
        }
        Err(JoinError::Forbidden(body)) if error_code(body).as_deref() == Some("NOT_HOST") => {
            CaptionsPostOutcome::NotHost
        }
        Err(JoinError::ServerError { status: 409, body })
            if error_code(body).as_deref() == Some("CAPTIONS_OFF") =>
        {
            CaptionsPostOutcome::CaptionsOff
        }
        Err(JoinError::ServerError { status: 409, body })
            if error_code(body).as_deref() == Some("MEETING_NOT_ACTIVE") =>
        {
            CaptionsPostOutcome::MeetingNotActive
        }
        Err(JoinError::NotAuthenticated) => CaptionsPostOutcome::RefreshToken,
        Err(JoinError::Forbidden(body)) if error_code(body).as_deref() == Some("FORBIDDEN") => {
            CaptionsPostOutcome::RefreshToken
        }
        Err(_) => CaptionsPostOutcome::RetryNextTick,
    }
}

/// Consecutive transient room-token refresh failures that stop renewing.
pub const CAPTIONS_REFRESH_FAILURES_MAX: u32 = 3;

/// The outcome of a rejected POST whose room token could not be refreshed:
/// a transient refresh failure retries on the next tick, anything else stops.
pub fn failed_refresh_outcome(error: &JoinError) -> CaptionsPostOutcome {
    match error {
        JoinError::Network(_) | JoinError::RateLimitExceeded => CaptionsPostOutcome::RetryNextTick,
        JoinError::ServerError { status, .. } if *status >= 500 || *status == 429 => {
            CaptionsPostOutcome::RetryNextTick
        }
        _ => CaptionsPostOutcome::Unauthorized,
    }
}

/// Consecutive transient refresh failures.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RefreshFailures(u32);

impl RefreshFailures {
    /// Records a failed refresh and returns what the POST comes to:
    /// [`CaptionsPostOutcome::Unauthorized`] on the
    /// [`CAPTIONS_REFRESH_FAILURES_MAX`]th transient failure in a row.
    pub fn record(&mut self, error: &JoinError) -> CaptionsPostOutcome {
        let transient = failed_refresh_outcome(error) == CaptionsPostOutcome::RetryNextTick;
        self.count(transient)
    }

    /// Records a refresh that timed out, as a transient failure.
    pub fn record_timeout(&mut self) -> CaptionsPostOutcome {
        self.count(true)
    }

    fn count(&mut self, transient: bool) -> CaptionsPostOutcome {
        if transient {
            self.0 += 1;
            if self.0 < CAPTIONS_REFRESH_FAILURES_MAX {
                return CaptionsPostOutcome::RetryNextTick;
            }
        }
        self.0 = 0;
        CaptionsPostOutcome::Unauthorized
    }
}

/// Whether the server's `NOT_HOST` demotes this host's control to a viewer's
/// while the room is captioned.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum HostRejection {
    #[default]
    None,
    /// Demoted; the next active status lifts it once.
    Rejected,
    /// Lifted by an active status, with no POST accepted since.
    Lifted,
    Latched,
    Starting,
}

/// An input to [`HostRejection::after`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostRejectionEvent {
    NotHost,
    Accepted,
    StatusActive,
    OwnStatusActive,
    StatusStopped,
    /// No status arrived within the status timeout.
    StatusTimeout,
    HostChanged,
    Refused,
}

impl HostRejection {
    pub fn after(self, event: HostRejectionEvent) -> Self {
        use HostRejection::*;
        use HostRejectionEvent as E;
        match (self, event) {
            (_, E::HostChanged) => None,
            (Lifted | Latched | Starting, E::NotHost) => Latched,
            (_, E::NotHost) => Rejected,
            (Rejected, E::StatusActive) => Lifted,
            (Rejected, E::OwnStatusActive) => Lifted,
            (Latched, E::OwnStatusActive) => Starting,
            (_, E::Accepted) => None,
            (Lifted, E::StatusStopped | E::StatusTimeout) => None,
            (Rejected, E::StatusTimeout) => None,
            (Starting, E::Refused | E::StatusTimeout) => Latched,
            (Latched | Starting, E::StatusStopped) => Rejected,
            (state, _) => state,
        }
    }

    pub fn demotes(self, captioned: bool) -> bool {
        captioned && matches!(self, HostRejection::Rejected | HostRejection::Latched)
    }
}

/// Whether the next enable POST is a renewal: only once one was accepted.
/// `outcome` is `None` when the request timed out.
pub fn renews_after(renewing: bool, outcome: Option<CaptionsPostOutcome>) -> bool {
    renewing || outcome == Some(CaptionsPostOutcome::Accepted)
}

/// What to tell a host whose start was rejected; `None` when it was not.
pub fn captions_start_failure_text(outcome: Option<CaptionsPostOutcome>) -> Option<String> {
    use CaptionsPostOutcome::*;
    let reason = match outcome? {
        Accepted | RetryNextTick => return None,
        NotHost => crate::components::co_hosts::CoHostErrorKind::NotHost.message(),
        FeatureOff | CaptionsOff => "They aren't available in this meeting.",
        MeetingNotActive => "The meeting isn't active.",
        MeetingGone => "This meeting is no longer available.",
        Unauthorized | RefreshToken => "Try again.",
    };
    Some(format!("Couldn't start captions. {reason}"))
}

/// Whether a revoke must be sent again: it failed transiently or timed out.
pub fn revoke_needs_retry(outcome: Option<CaptionsPostOutcome>) -> bool {
    matches!(outcome, None | Some(CaptionsPostOutcome::RetryNextTick))
}

/// Short uppercase label for a BCP-47 tag (`"it-IT"` -> `"IT"`), or `None`.
pub fn language_badge(tag: &str) -> Option<String> {
    let primary = tag.split(['-', '_']).next().unwrap_or("").trim();
    if primary.is_empty() || primary.len() > 3 || !primary.chars().all(|c| c.is_ascii_alphabetic())
    {
        return None;
    }
    Some(primary.to_ascii_uppercase())
}

thread_local! {
    static ROOM_TOKENS: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
}

/// Records the latest room token for `meeting_id`'s captions requests.
pub fn remember_room_token(meeting_id: &str, token: &str) {
    ROOM_TOKENS.with(|t| {
        t.borrow_mut()
            .insert(meeting_id.to_string(), token.to_string())
    });
}

pub fn current_room_token(meeting_id: &str) -> Option<String> {
    ROOM_TOKENS.with(|t| t.borrow().get(meeting_id).cloned())
}

pub fn forget_room_token(meeting_id: &str) {
    ROOM_TOKENS.with(|t| t.borrow_mut().remove(meeting_id));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(
        session: u64,
        utterance: u64,
        revision: u32,
        is_final: bool,
        text: &str,
    ) -> CaptionEvent {
        CaptionEvent {
            speaker_session_id: session,
            speaker_name: format!("Speaker {session}"),
            is_local: false,
            utterance_id: utterance,
            revision,
            is_final,
            text: text.to_string(),
            text_language: if is_final { "en" } else { "it" }.to_string(),
            source_text: String::new(),
            source_language: String::new(),
            timestamp_ms: 0,
            translations: vec![],
        }
    }

    fn translated(mut e: CaptionEvent, pairs: &[(&str, &str)]) -> CaptionEvent {
        e.translations = pairs
            .iter()
            .map(|(language, text)| CaptionTranslation {
                language: language.to_string(),
                text: text.to_string(),
            })
            .collect();
        e
    }

    #[test]
    fn a_line_shows_the_viewers_language_or_falls_back_to_its_text() {
        let mut b = CaptionBoard::default();
        let e = event(1, 1, 1, true, "Good morning");
        b.apply(
            translated(e, &[("en", "Good morning"), ("it-IT", "Buongiorno")]),
            0.0,
        );
        let line = &b.lines()[0];
        assert_eq!(line.shown("it"), ("Buongiorno", "it-IT"));
        assert_eq!(line.shown("IT-ch"), ("Buongiorno", "it-IT"));
        assert_eq!(
            line.shown("de"),
            ("Good morning", "en"),
            "not rendered in German"
        );
        assert_eq!(line.announcement("it"), "Speaker 1: Buongiorno");
    }

    #[test]
    fn a_retranslated_interim_with_the_same_text_is_applied_not_refreshed() {
        let mut b = CaptionBoard::default();
        b.apply(event(1, 1, 2, false, "Buenos días"), 0.0);
        let same = event(1, 1, 3, false, "Buenos días");
        let fresh = translated(
            event(1, 1, 3, false, "Buenos días"),
            &[("en", "Good morning")],
        );
        assert_eq!(b.preview(&same), CaptionEffect::Refreshed);
        assert_eq!(b.preview(&fresh), CaptionEffect::Applied);
        b.apply(fresh, 1.0);
        assert_eq!(b.lines()[0].shown("en").0, "Good morning");
    }

    #[test]
    fn the_caption_language_prefers_the_saved_then_the_browser_then_english() {
        let browser = |tags: &[&str]| tags.iter().map(|t| t.to_string()).collect::<Vec<_>>();
        assert_eq!(caption_language(Some("fr"), &browser(&["it-IT"])), "fr");
        assert_eq!(
            caption_language(Some("de"), &browser(&["de-AT", "it-IT"])),
            "it",
            "German is not offered, even as the browser language"
        );
        assert_eq!(
            caption_language(Some("xx"), &browser(&["pt-BR", "it-IT"])),
            "it"
        );
        assert_eq!(caption_language(None, &browser(&["es-419", "fr"])), "es");
        assert_eq!(caption_language(None, &browser(&["ja", "zh"])), "en");
        assert_eq!(caption_language(Some(""), &[]), "en");
    }

    #[test]
    fn languages_match_on_their_primary_subtag() {
        assert!(same_language("en-US", "EN"));
        assert!(same_language("pt_BR", "pt"));
        assert!(!same_language("es", "en"));
        assert!(!same_language("", ""));
    }

    fn texts(board: &CaptionBoard) -> Vec<&str> {
        board.lines().iter().map(|l| l.text.as_str()).collect()
    }

    fn status(active: bool, name: &str, local: bool) -> CaptionStatus {
        CaptionStatus {
            active,
            started_by_user_id: "host@example.com".to_string(),
            started_by_name: name.to_string(),
            started_by_local: local,
        }
    }

    #[test]
    fn a_higher_revision_replaces_the_line_in_place() {
        let mut b = CaptionBoard::default();
        assert_eq!(
            b.apply(event(1, 1, 1, false, "buon"), 0.0),
            CaptionEffect::Applied
        );
        b.apply(event(1, 1, 2, false, "buongiorno"), 10.0);
        b.apply(event(1, 1, 3, true, "good morning"), 20.0);
        assert_eq!(texts(&b), ["good morning"]);
        let line = &b.lines()[0];
        assert!(line.is_final);
        assert_eq!(line.language, "en");
        assert_eq!(line.revision, 3);
    }

    #[test]
    fn lower_or_equal_revisions_are_ignored() {
        let mut b = CaptionBoard::default();
        b.apply(event(1, 1, 5, true, "final"), 0.0);
        assert_eq!(
            b.preview(&event(1, 1, 4, false, "late interim")),
            CaptionEffect::Ignored
        );
        assert_eq!(
            b.apply(event(1, 1, 5, false, "duplicate"), 1.0),
            CaptionEffect::Ignored
        );
        assert_eq!(texts(&b), ["final"]);
        assert!(b.lines()[0].is_final);
    }

    #[test]
    fn an_identical_newer_revision_only_advances_revision_and_clock() {
        let mut b = CaptionBoard::default();
        b.apply(event(1, 1, 1, false, "ciao"), 0.0);
        assert_eq!(
            b.apply(event(1, 1, 3, false, "ciao"), 1_000.0),
            CaptionEffect::Refreshed
        );
        assert_eq!(texts(&b), ["ciao"]);
        assert_eq!(b.lines()[0].revision, 3);
        assert_eq!(b.lines()[0].updated_ms, 1_000.0);
        assert_eq!(
            b.apply(event(1, 1, 2, false, "ciao bella"), 2_000.0),
            CaptionEffect::Ignored
        );
        assert_eq!(texts(&b), ["ciao"]);
        assert_eq!(
            b.preview(&event(1, 1, 4, true, "ciao")),
            CaptionEffect::Applied
        );
        let mut relabelled = event(1, 1, 4, false, "ciao");
        relabelled.text_language = "es".to_string();
        assert_eq!(b.preview(&relabelled), CaptionEffect::Applied);
    }

    #[test]
    fn a_new_line_never_evicts_itself() {
        let mut b = CaptionBoard::default();
        for s in 1..=3 {
            b.apply(event(s, 1, 1, false, "interim"), s as f64);
        }
        assert_eq!(
            b.apply(event(4, 1, 1, true, "short final"), 10.0),
            CaptionEffect::Applied
        );
        assert_eq!(texts(&b), ["interim", "interim", "short final"]);
        assert_eq!(b.lines()[0].speaker_session_id, 2);
    }

    #[test]
    fn utterances_are_keyed_by_speaker_and_utterance() {
        let mut b = CaptionBoard::default();
        b.apply(event(1, 7, 1, false, "a"), 0.0);
        b.apply(event(2, 7, 1, false, "b"), 0.0);
        assert_eq!(texts(&b), ["a", "b"]);
    }

    #[test]
    fn only_the_newest_lines_are_kept_and_evicted_ones_stay_gone() {
        let mut b = CaptionBoard::default();
        for u in 1..=MAX_CAPTION_LINES as u64 + 1 {
            b.apply(event(1, u, 1, true, &format!("line {u}")), u as f64);
        }
        assert_eq!(texts(&b), ["line 2", "line 3", "line 4"]);
        assert_eq!(
            b.apply(event(1, 1, 2, true, "line 1 again"), 10.0),
            CaptionEffect::Ignored
        );
        assert_eq!(texts(&b), ["line 2", "line 3", "line 4"]);
    }

    #[test]
    fn eviction_takes_the_line_closest_to_expiring() {
        let mut b = CaptionBoard::default();
        b.apply(event(1, 1, 1, false, "a1"), 0.0);
        b.apply(event(2, 1, 1, true, "b final"), 5.0);
        b.apply(event(3, 1, 1, true, "c final"), 6.0);
        b.apply(event(4, 1, 1, false, "d1"), 7.0);
        assert_eq!(texts(&b), ["a1", "c final", "d1"], "the older final goes");

        // A dead interim outranks a final that has only just appeared.
        let mut b = CaptionBoard::default();
        b.apply(event(1, 1, 1, false, "a1"), 0.0);
        b.apply(event(2, 1, 1, false, "b1"), 1_000.0);
        b.apply(event(3, 1, 1, true, "c final"), 10_000.0);
        b.apply(event(4, 1, 1, false, "d1"), 10_200.0);
        assert_eq!(texts(&b), ["b1", "c final", "d1"]);
        assert_eq!(
            b.apply(event(1, 1, 2, true, "a final"), 10_300.0),
            CaptionEffect::Ignored,
            "the evicted utterance stays gone"
        );
    }

    #[test]
    fn retirements_are_bounded_and_deduplicated() {
        let mut b = CaptionBoard::default();
        for u in 1..=(RETIRED_KEYS_MAX as u64 + 8) {
            b.apply(event(1, u, 1, true, ""), 0.0);
            b.apply(event(1, u, 2, true, ""), 0.0);
        }
        assert_eq!(b.retired.len(), RETIRED_KEYS_MAX);
        assert_eq!(
            b.retired.back(),
            Some(&((1, RETIRED_KEYS_MAX as u64 + 8), 2))
        );
        assert_eq!(
            b.apply(event(1, 1, 3, true, "back"), 0.0),
            CaptionEffect::Applied,
            "a retirement older than the window is forgotten"
        );
    }

    #[test]
    fn retiring_one_utterance_again_does_not_push_others_out() {
        let mut b = CaptionBoard::default();
        b.apply(event(1, 1, 1, true, ""), 0.0);
        for rev in 1..=RETIRED_KEYS_MAX as u32 {
            b.apply(event(1, 2, rev, true, ""), 0.0);
        }
        assert_eq!(
            b.apply(event(1, 1, 1, true, "late"), 0.0),
            CaptionEffect::Ignored
        );
    }

    #[test]
    fn a_board_holding_only_retirements_is_not_empty() {
        let mut b = CaptionBoard::default();
        assert!(b.is_empty());
        b.apply(event(1, 1, 1, true, "final"), 0.0);
        assert!(b.prune(CAPTION_FINAL_LINGER_MS));
        assert!(b.lines().is_empty());
        assert!(!b.is_empty());
    }

    #[test]
    fn the_button_hint_follows_role_pressed_and_disabled() {
        use CaptionsRole::{Host, Viewer};
        for (role, pressed, disabled, want) in [
            (Host, true, false, "Turn off captions for everyone"),
            (Host, false, false, "Turn on captions for everyone"),
            (Viewer, true, false, "Hide captions"),
            (Viewer, false, false, "Show captions"),
            (Viewer, false, true, CAPTIONS_OFF_HINT),
            (Host, true, true, CAPTIONS_OFF_HINT),
        ] {
            let state = CaptionsButtonState {
                role,
                pressed,
                disabled,
                pending: false,
            };
            assert_eq!(captions_button_hint(state), want, "{state:?}");
        }
    }

    #[test]
    fn four_concurrent_interim_speakers_do_not_thrash() {
        let mut b = CaptionBoard::default();
        let mut t = 0.0;
        for s in 1..=3 {
            t += 1.0;
            b.apply(event(s, 1, 1, false, "x"), t);
        }
        t += 1.0;
        assert_eq!(
            b.apply(event(4, 1, 1, false, "d"), t),
            CaptionEffect::Applied
        );
        assert_eq!(texts(&b), ["x", "x", "d"]);
        let before = b.lines().to_vec();
        for rev in 2..10 {
            for s in 1..=4 {
                t += 1.0;
                let effect = b.apply(event(s, 1, rev, false, &format!("s{s} r{rev}")), t);
                if s == 1 {
                    assert_eq!(effect, CaptionEffect::Ignored, "rev {rev}");
                }
            }
        }
        let keys: Vec<_> = b.lines().iter().map(CaptionLine::key).collect();
        assert_eq!(
            keys,
            before.iter().map(CaptionLine::key).collect::<Vec<_>>()
        );
        assert_eq!(texts(&b), ["s2 r9", "s3 r9", "s4 r9"]);
    }

    #[test]
    fn finals_linger_then_expire_and_stalled_interims_expire_later() {
        let mut b = CaptionBoard::default();
        b.apply(event(1, 1, 1, true, "final"), 0.0);
        b.apply(event(2, 1, 1, false, "interim"), 0.0);
        assert_eq!(b.next_expiry_ms(), Some(CAPTION_FINAL_LINGER_MS));
        assert!(!b.has_expired(CAPTION_FINAL_LINGER_MS - 1.0));
        assert!(b.has_expired(CAPTION_FINAL_LINGER_MS));
        assert!(!b.prune(CAPTION_FINAL_LINGER_MS - 1.0));
        assert!(b.prune(CAPTION_FINAL_LINGER_MS));
        assert_eq!(texts(&b), ["interim"]);
        assert_eq!(b.next_expiry_ms(), Some(CAPTION_INTERIM_STALE_MS));
        assert!(b.prune(CAPTION_INTERIM_STALE_MS));
        assert!(b.lines().is_empty());
        assert_eq!(b.next_expiry_ms(), None);
        assert_eq!(
            b.apply(event(1, 1, 1, false, "replayed"), 20_000.0),
            CaptionEffect::Ignored
        );
    }

    #[test]
    fn an_update_restarts_the_linger_clock() {
        let mut b = CaptionBoard::default();
        b.apply(event(1, 1, 1, false, "x"), 0.0);
        b.apply(event(1, 1, 2, true, "y"), 5_000.0);
        assert!(!b.prune(5_000.0 + CAPTION_FINAL_LINGER_MS - 1.0));
    }

    #[test]
    fn empty_text_removes_the_line() {
        let mut b = CaptionBoard::default();
        b.apply(event(1, 1, 1, false, "uh"), 0.0);
        assert_eq!(
            b.apply(event(1, 1, 2, true, ""), 1.0),
            CaptionEffect::Applied
        );
        assert!(b.lines().is_empty());
        assert_eq!(
            b.apply(event(1, 1, 2, false, "uh"), 2.0),
            CaptionEffect::Ignored
        );
        assert_eq!(
            b.apply(event(1, 1, 3, true, "uh huh"), 3.0),
            CaptionEffect::Applied
        );
        assert_eq!(texts(&b), ["uh huh"]);
    }

    #[test]
    fn the_local_speaker_is_labelled_you() {
        let mut b = CaptionBoard::default();
        let mut e = event(1, 1, 1, true, "hello");
        e.is_local = true;
        b.apply(e, 0.0);
        assert_eq!(b.lines()[0].speaker, CAPTION_LOCAL_SPEAKER);
    }

    #[test]
    fn colons_and_some_look_alikes_are_dropped_from_the_announced_speaker() {
        let mut b = CaptionBoard::default();
        let mut e = event(1, 1, 1, true, "hello");
        e.speaker_name =
            "Bob: I\u{FF1A} re\u{A789}sign\u{FE13} no\u{05C3}w\u{02D0}\u{FE55}\u{2236}".to_string();
        b.apply(e, 0.0);
        assert_eq!(b.lines()[0].announcement("en"), "Bob I resign now: hello");
    }

    #[test]
    fn availability_requires_the_flag_no_e2ee_and_no_server_veto() {
        assert!(captions_available(true, false, false));
        assert!(!captions_available(false, false, false));
        assert!(!captions_available(true, true, false));
        assert!(!captions_available(true, false, true));
    }

    #[test]
    fn the_notice_follows_status_packets() {
        assert_eq!(
            notice_for_status(&status(true, "Hana", false)),
            Some(CaptionStarter::Named("Hana".to_string()))
        );
        assert_eq!(
            notice_for_status(&status(true, "Hana", true)),
            Some(CaptionStarter::Me)
        );
        assert_eq!(notice_for_status(&status(false, "", false)), None);
    }

    #[test]
    fn a_remote_starter_named_you_is_not_the_viewer() {
        let starter = notice_for_status(&status(true, "you", false));
        assert_eq!(starter, Some(CaptionStarter::Named("you".to_string())));
        assert_eq!(
            notice_announcement(starter.as_ref(), false),
            "Live captions started by you"
        );
    }

    #[test]
    fn the_announcement_names_the_starter_and_the_stop() {
        let named = |n: &str| CaptionStarter::Named(n.to_string());
        assert_eq!(
            notice_announcement(Some(&CaptionStarter::Me), false),
            "You started live captions"
        );
        assert_eq!(
            notice_announcement(Some(&named("Hana")), false),
            "Live captions started by Hana"
        );
        assert_eq!(
            notice_announcement(Some(&named("")), true),
            "Live captions started"
        );
        assert_eq!(notice_announcement(None, true), "Live captions stopped");
        assert_eq!(notice_announcement(None, false), "");
    }

    #[test]
    fn host_button_controls_the_room() {
        use CaptionsRole::Host;
        let idle = captions_button_state(Host, false, false, false, false);
        assert_eq!(
            idle,
            CaptionsButtonState {
                role: Host,
                pressed: false,
                disabled: false,
                pending: false,
            }
        );
        assert_eq!(captions_toggle(idle), CaptionsToggle::StartRoom);
        for (room_on, accepted, active) in [
            (true, true, false),
            (false, false, true),
            (true, false, true),
            (true, true, true),
        ] {
            let s = captions_button_state(Host, room_on, accepted, active, false);
            assert!(s.pressed && !s.disabled && !s.pending, "{s:?}");
            assert_eq!(captions_toggle(s), CaptionsToggle::RevokeRoom);
        }
        let pending = captions_button_state(Host, true, false, false, false);
        assert!(pending.pending && !pending.pressed && !pending.disabled);
        assert_eq!(captions_toggle(pending), CaptionsToggle::RevokeRoom);
        assert_eq!(
            captions_button_hint(pending),
            "Turning on captions for everyone\u{2026}"
        );
    }

    #[test]
    fn viewer_button_only_shows_or_hides_and_is_disabled_until_active() {
        use CaptionsRole::Viewer;
        let off = captions_button_state(Viewer, true, true, false, true);
        assert!(off.disabled && !off.pressed);
        assert_eq!(captions_toggle(off), CaptionsToggle::Nothing);

        let shown = captions_button_state(Viewer, false, false, true, true);
        assert!(shown.pressed && !shown.disabled);
        assert_eq!(captions_toggle(shown), CaptionsToggle::SetShow(false));

        let hidden = captions_button_state(Viewer, false, false, true, false);
        assert!(!hidden.pressed);
        assert_eq!(captions_toggle(hidden), CaptionsToggle::SetShow(true));
    }

    #[test]
    fn overlay_visibility() {
        assert!(captions_overlay_visible(CaptionsRole::Host, true, false));
        assert!(captions_overlay_visible(CaptionsRole::Viewer, true, true));
        assert!(!captions_overlay_visible(CaptionsRole::Viewer, true, false));
        assert!(!captions_overlay_visible(CaptionsRole::Host, false, true));
    }

    #[test]
    fn renew_delay_stays_within_ten_percent() {
        assert_eq!(renew_delay_ms(40_000, 0.0), 36_000);
        assert_eq!(renew_delay_ms(40_000, 0.5), 40_000);
        assert_eq!(renew_delay_ms(40_000, 1.0), 44_000);
        assert!(renew_delay_ms(40_000, 0.999) < 44_000);
        assert_eq!(renew_delay_ms(40_000, -1.0), 36_000);
        assert_eq!(renew_delay_ms(40_000, 2.0), 44_000);
    }

    #[test]
    fn failed_posts_are_sent_again_sooner_than_a_renewal() {
        use CaptionsPostOutcome::*;
        assert_eq!(next_renew_delay_ms(40_000, None, 1.0), 0);
        assert_eq!(next_renew_delay_ms(40_000, Some(RetryNextTick), 0.0), 9_000);
        assert_eq!(
            next_renew_delay_ms(40_000, Some(RetryNextTick), 0.5),
            10_000
        );
        assert_eq!(
            next_renew_delay_ms(40_000, Some(RetryNextTick), 1.0),
            11_000
        );
        assert_eq!(next_renew_delay_ms(40_000, Some(Accepted), 0.0), 36_000);
        assert_eq!(next_renew_delay_ms(40_000, Some(Accepted), 1.0), 44_000);
    }

    fn body(code: &str) -> String {
        format!(r#"{{"success":false,"result":{{"code":"{code}","message":"m"}}}}"#)
    }

    #[test]
    fn post_outcomes_are_decided_by_status_and_body_code() {
        use CaptionsPostOutcome::*;
        let conflict = |code: &str| JoinError::ServerError {
            status: 409,
            body: body(code),
        };
        let cases: Vec<(Result<(), JoinError>, CaptionsPostOutcome)> = vec![
            (Ok(()), Accepted),
            (Err(JoinError::NotFound(body("NOT_FOUND"))), FeatureOff),
            (
                Err(JoinError::NotFound(body("MEETING_NOT_FOUND"))),
                MeetingGone,
            ),
            (
                Err(JoinError::NotFound(body("NOT_IN_MEETING"))),
                RetryNextTick,
            ),
            // No route at all (a meeting-api predating captions): hide the control.
            (Err(JoinError::NotFound(String::new())), FeatureOff),
            (Err(JoinError::NotFound("<html>".to_string())), FeatureOff),
            (Err(JoinError::Forbidden(body("NOT_HOST"))), NotHost),
            (Err(JoinError::Forbidden(body("FORBIDDEN"))), RefreshToken),
            (Err(JoinError::Forbidden(String::new())), RetryNextTick),
            (
                Err(JoinError::Forbidden("<html>Forbidden</html>".to_string())),
                RetryNextTick,
            ),
            (Err(JoinError::NotFound(body("NOT_HOST"))), RetryNextTick),
            (
                Err(JoinError::Forbidden(body("MEETING_NOT_FOUND"))),
                RetryNextTick,
            ),
            (Err(conflict("CAPTIONS_OFF")), CaptionsOff),
            (Err(conflict("MEETING_NOT_ACTIVE")), MeetingNotActive),
            (
                Err(JoinError::ServerError {
                    status: 400,
                    body: body("MEETING_NOT_ACTIVE"),
                }),
                RetryNextTick,
            ),
            (Err(conflict("CONFLICT")), RetryNextTick),
            (
                Err(JoinError::ServerError {
                    status: 503,
                    body: body("CAPTIONS_OFF"),
                }),
                RetryNextTick,
            ),
            (
                Err(JoinError::Forbidden(body("CAPTIONS_OFF"))),
                RetryNextTick,
            ),
            (
                Err(JoinError::ServerError {
                    status: 429,
                    body: body("RATE_LIMITED"),
                }),
                RetryNextTick,
            ),
            (
                Err(JoinError::ServerError {
                    status: 503,
                    body: body("NOT_FOUND"),
                }),
                RetryNextTick,
            ),
            (Err(JoinError::NotAuthenticated), RefreshToken),
            (Err(JoinError::Config(String::new())), RetryNextTick),
        ];
        for (result, want) in cases {
            assert_eq!(classify_captions_post(&result), want, "{result:?}");
        }
    }

    #[test]
    fn only_an_accepted_post_turns_the_next_one_into_a_renewal() {
        use CaptionsPostOutcome::*;
        assert!(!renews_after(false, None));
        assert!(!renews_after(false, Some(RetryNextTick)));
        assert!(renews_after(false, Some(Accepted)));
        assert!(renews_after(true, Some(RetryNextTick)));
        assert!(renews_after(true, None));
        assert!(!renews_after(false, Some(RefreshToken)));
        assert!(renews_after(true, Some(RefreshToken)));
    }

    #[test]
    fn only_a_transient_refresh_failure_retries() {
        use CaptionsPostOutcome::*;
        let server = |status| JoinError::ServerError {
            status,
            body: String::new(),
        };
        assert_eq!(failed_refresh_outcome(&server(503)), RetryNextTick);
        assert_eq!(failed_refresh_outcome(&server(429)), RetryNextTick);
        assert_eq!(
            failed_refresh_outcome(&JoinError::RateLimitExceeded),
            RetryNextTick
        );
        assert_eq!(failed_refresh_outcome(&server(200)), Unauthorized);
        assert_eq!(
            failed_refresh_outcome(&JoinError::NotAuthenticated),
            Unauthorized
        );
        assert_eq!(
            failed_refresh_outcome(&JoinError::NotFound(String::new())),
            Unauthorized
        );
    }

    #[test]
    fn transient_refresh_failures_stop_at_the_cap() {
        use CaptionsPostOutcome::*;
        let busy = JoinError::ServerError {
            status: 503,
            body: String::new(),
        };
        let mut f = RefreshFailures::default();
        for _ in 1..CAPTIONS_REFRESH_FAILURES_MAX {
            assert_eq!(f.record(&busy), RetryNextTick);
        }
        assert_eq!(f.record(&busy), Unauthorized);
        assert_eq!(f, RefreshFailures::default());

        let mut f = RefreshFailures::default();
        assert_eq!(f.record(&JoinError::NotAuthenticated), Unauthorized);
        assert_eq!(f, RefreshFailures::default());

        assert_eq!(f.record(&busy), RetryNextTick);
        for _ in 2..CAPTIONS_REFRESH_FAILURES_MAX {
            assert_eq!(f.record_timeout(), RetryNextTick);
        }
        assert_eq!(f.record_timeout(), Unauthorized);
    }

    #[test]
    fn a_not_host_is_lifted_once_by_an_active_status() {
        use HostRejection::*;
        use HostRejectionEvent::*;
        let walk = |events: &[HostRejectionEvent]| {
            events
                .iter()
                .fold(HostRejection::default(), |s, e| s.after(*e))
        };
        assert_eq!(walk(&[NotHost]), Rejected);
        assert!(Rejected.demotes(true) && Latched.demotes(true));
        assert!(!HostRejection::None.demotes(true) && !Lifted.demotes(true));
        for state in [HostRejection::None, Rejected, Lifted, Latched] {
            assert!(!state.demotes(false), "{state:?}");
        }
        assert_eq!(walk(&[NotHost, StatusActive]), Lifted);
        assert_eq!(walk(&[NotHost, StatusActive, StatusActive]), Lifted);
        assert_eq!(walk(&[NotHost, StatusActive, NotHost]), Latched);
        assert_eq!(
            walk(&[NotHost, StatusActive, NotHost, StatusActive]),
            Latched
        );
        assert_eq!(walk(&[NotHost, StatusActive, Accepted, NotHost]), Rejected);
        assert_eq!(
            walk(&[NotHost, StatusActive, NotHost, StatusStopped]),
            Rejected
        );
        assert_eq!(
            walk(&[NotHost, StatusActive, NotHost, HostChanged]),
            HostRejection::None
        );
        assert_eq!(walk(&[StatusActive, Accepted]), HostRejection::None);
        assert_eq!(
            walk(&[NotHost, StatusActive, NotHost, Accepted]),
            HostRejection::None
        );
        assert_eq!(walk(&[NotHost, Accepted]), HostRejection::None);
        assert_eq!(walk(&[NotHost, StatusTimeout]), HostRejection::None);
        assert_eq!(walk(&[NotHost, OwnStatusActive]), Lifted);
        let starting = [NotHost, StatusActive, NotHost, OwnStatusActive];
        assert_eq!(walk(&starting), Starting);
        assert!(!Starting.demotes(true));
        for (then, state) in [
            (NotHost, Latched),
            (Refused, Latched),
            (StatusTimeout, Latched),
            (StatusStopped, Rejected),
            (StatusActive, Starting),
            (OwnStatusActive, Starting),
            (Accepted, HostRejection::None),
            (HostChanged, HostRejection::None),
        ] {
            assert_eq!(walk(&[&starting[..], &[then]].concat()), state, "{then:?}");
        }
        assert_eq!(walk(&[NotHost, StatusActive, Refused]), Lifted);
        assert_eq!(walk(&[OwnStatusActive]), HostRejection::None);
        assert_eq!(
            walk(&[NotHost, StatusActive, NotHost, StatusTimeout]),
            Latched
        );
        for decay in [StatusStopped, StatusTimeout] {
            assert_eq!(
                walk(&[NotHost, StatusActive, decay]),
                HostRejection::None,
                "{decay:?}"
            );
            assert_eq!(walk(&[StatusActive, decay]), HostRejection::None);
        }
    }

    #[test]
    fn only_a_transient_failure_or_timeout_resends_a_revoke() {
        use CaptionsPostOutcome::*;
        assert!(revoke_needs_retry(None));
        assert!(revoke_needs_retry(Some(RetryNextTick)));
        for done in [
            Accepted,
            FeatureOff,
            MeetingGone,
            NotHost,
            CaptionsOff,
            MeetingNotActive,
            RefreshToken,
            Unauthorized,
        ] {
            assert!(!revoke_needs_retry(Some(done)), "{done:?}");
        }
        let total: u32 = CAPTIONS_REVOKE_RETRY_MS.iter().sum();
        assert!(
            total >= 120_000,
            "retries stop before the lease lapses: {total}"
        );
    }

    #[test]
    fn language_badges() {
        assert_eq!(language_badge("it-IT").as_deref(), Some("IT"));
        assert_eq!(language_badge("en").as_deref(), Some("EN"));
        assert_eq!(language_badge(""), None);
        assert_eq!(language_badge("<script>"), None);
        assert_eq!(language_badge("abcd-EF"), None);
    }

    #[test]
    fn the_room_token_store_keeps_the_latest_value_per_meeting() {
        remember_room_token("m1", "first");
        remember_room_token("m1", "second");
        assert_eq!(current_room_token("m1").as_deref(), Some("second"));
        remember_room_token("m2", "other");
        forget_room_token("m1");
        assert_eq!(current_room_token("m1"), None);
        assert_eq!(current_room_token("m2").as_deref(), Some("other"));
    }

    #[test]
    fn a_room_token_is_never_returned_for_another_meeting() {
        remember_room_token("meeting-a", "token-a");
        assert_eq!(current_room_token("meeting-b"), None);
    }

    #[test]
    fn a_late_token_for_a_previous_meeting_keeps_the_current_one() {
        remember_room_token("meeting-a", "token-a");
        remember_room_token("meeting-b", "token-b");
        remember_room_token("meeting-a", "token-a-refreshed");
        assert_eq!(current_room_token("meeting-b").as_deref(), Some("token-b"));
    }
}
