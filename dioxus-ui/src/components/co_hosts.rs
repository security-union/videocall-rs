// SPDX-License-Identifier: MIT OR Apache-2.0

//! Co-hosts. The meeting OWNER (`creator_id`, `host_user_id` on
//! the wire) is single and permanent; the HOST ROLE may be held by several
//! participants, and a co-host is any non-owner holding it. Owner-only
//! surfaces gate on [`MeetingOwnership`]; host powers gate on the host role.

use crate::components::attendants::action_bar_announce_text;
use crate::components::canvas_generator::focus_element_by_id;
use crate::components::toggle_switch::ToggleSwitch;
use crate::context::HostSetCtx;
use crate::meeting_api::{CoHostEntry, JoinError};
use dioxus::prelude::*;
use gloo_timers::callback::Timeout;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use videocall_meeting_types::GUEST_USER_ID_PREFIX;

/// Whether the local user owns the meeting. Built only from identities, so the
/// host flag cannot be passed where ownership is required.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MeetingOwnership(bool);

impl MeetingOwnership {
    pub fn of(owner_user_id: Option<&str>, local_user_id: Option<&str>) -> Self {
        Self(matches!(
            (owner_user_id, local_user_id),
            (Some(owner), Some(local)) if !owner.is_empty() && owner == local
        ))
    }

    pub fn is_owner(self) -> bool {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostRole {
    Host,
    CoHost,
}

impl HostRole {
    pub fn label(self) -> &'static str {
        match self {
            HostRole::Host => "Host",
            HostRole::CoHost => "Co-host",
        }
    }
}

/// "Co-host" only while the owner also holds the role; otherwise every role
/// holder reads "Host".
pub fn host_role(
    is_host: bool,
    user_id: &str,
    owner_user_id: Option<&str>,
    owner_holds_host: bool,
) -> Option<HostRole> {
    if !is_host {
        return None;
    }
    if owner_holds_host && owner_user_id != Some(user_id) {
        Some(HostRole::CoHost)
    } else {
        Some(HostRole::Host)
    }
}

pub fn host_indicator(is_self: bool, role: Option<HostRole>) -> Option<&'static str> {
    match (is_self, role) {
        (true, Some(HostRole::Host)) => Some("(You/Host)"),
        (true, Some(HostRole::CoHost)) => Some("(You/Co-host)"),
        (true, None) => Some("(You)"),
        (false, Some(HostRole::Host)) => Some("(Host)"),
        (false, Some(HostRole::CoHost)) => Some("(Co-host)"),
        (false, None) => None,
    }
}

/// The live host set when provided, else the creator id.
pub fn peer_holds_host(
    host_set: Option<&HostSetCtx>,
    owner_user_id: Option<&str>,
    user_id: &str,
) -> bool {
    match host_set {
        Some(hs) => hs.is_host(user_id),
        None => owner_user_id == Some(user_id),
    }
}

pub fn owner_holds_host(host_set: Option<&HostSetCtx>, owner_user_id: Option<&str>) -> bool {
    owner_user_id.is_some_and(|owner| peer_holds_host(host_set, owner_user_id, owner))
}

/// Whether Meeting Options — the action-bar slot, the in-call dialog, and the
/// pre-join controls — should be shown: the meeting OWNER, or anyone
/// CURRENTLY holding the host role. Co-host management, the password, ending
/// the meeting for everyone, and deleting it stay OWNER-only — gate those on
/// `viewer.is_owner()` directly, never through this function.
///
/// `is_host` should be the caller's live host-role signal, falling back to a
/// join-time host-role snapshot before that live signal is populated.
pub fn can_edit_meeting_options(viewer: MeetingOwnership, is_host: bool) -> bool {
    viewer.is_owner() || is_host
}

pub fn host_change_toast_text(granted: bool) -> &'static str {
    if granted {
        "You now have host controls"
    } else {
        "You no longer have host controls"
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoHostMenuAction {
    Make,
    Remove,
    /// Revoke from a host shown as "Host", e.g. a transfer target.
    RemoveHostRole,
}

impl CoHostMenuAction {
    pub fn label(self) -> &'static str {
        match self {
            CoHostMenuAction::Make => "Make co-host",
            CoHostMenuAction::Remove => "Remove co-host",
            CoHostMenuAction::RemoveHostRole => "Remove host role",
        }
    }

    /// A grant with no explicit `persist` is saved for future meetings by
    /// default, so this is unconditional.
    pub fn success_text(self, name: &str) -> String {
        match self {
            CoHostMenuAction::Make => {
                format!("{name} is now a co-host and saved for future meetings.")
            }
            CoHostMenuAction::Remove => format!("{name} is no longer a co-host."),
            CoHostMenuAction::RemoveHostRole => format!("{name} no longer has host controls."),
        }
    }

    fn failure_text(self, name: &str, reason: &str) -> String {
        match self {
            CoHostMenuAction::Make => format!("Couldn't make {name} a co-host. {reason}"),
            CoHostMenuAction::Remove => format!("Couldn't remove {name} as co-host. {reason}"),
            CoHostMenuAction::RemoveHostRole => {
                format!("Couldn't remove {name}'s host controls. {reason}")
            }
        }
    }
}

#[component]
pub fn CoHostMenuIcon(action: CoHostMenuAction, size: u32) -> Element {
    rsx! {
        svg {
            xmlns: "http://www.w3.org/2000/svg",
            width: "{size}",
            height: "{size}",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "2",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            path { d: "M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2" }
            circle { cx: "9", cy: "7", r: "4" }
            if action == CoHostMenuAction::Make {
                line { x1: "19", y1: "8", x2: "19", y2: "14" }
            }
            line { x1: "22", y1: "11", x2: "16", y2: "11" }
        }
    }
}

pub struct CoHostTarget<'a> {
    pub user_id: &'a str,
    pub is_self: bool,
    pub is_guest: bool,
    pub is_host: bool,
    /// Decides whether a host target reads "Co-host" or "Host".
    pub owner_holds_host: bool,
}

/// The co-host item a peer's menu offers the viewer, if any.
pub fn co_host_menu_action(
    viewer: MeetingOwnership,
    owner_user_id: Option<&str>,
    target: &CoHostTarget<'_>,
) -> Option<CoHostMenuAction> {
    if !viewer.is_owner()
        || target.is_self
        || target.user_id.is_empty()
        || owner_user_id == Some(target.user_id)
    {
        return None;
    }
    if target.is_host {
        return Some(if target.owner_holds_host {
            CoHostMenuAction::Remove
        } else {
            CoHostMenuAction::RemoveHostRole
        });
    }
    if target.is_guest || target.user_id.starts_with(GUEST_USER_ID_PREFIX) {
        return None;
    }
    Some(CoHostMenuAction::Make)
}

/// Only the owner may kick another host or the owner; the server 403s it.
pub fn can_kick(viewer: MeetingOwnership, peer_is_host: bool, peer_is_owner: bool) -> bool {
    viewer.is_owner() || !(peer_is_host || peer_is_owner)
}

/// The host items a peer's menu offers. Mute and disable-video also need the
/// peer's mic / camera to be on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PeerHostMenu {
    /// A host's client ignores host mute and disable-video, so they are never
    /// offered on a role holder.
    pub mute_and_disable_video: bool,
    pub kick: bool,
    pub transfer: bool,
    pub co_host: Option<CoHostMenuAction>,
}

pub fn peer_host_menu(
    viewer_is_host: bool,
    viewer: MeetingOwnership,
    owner_user_id: Option<&str>,
    target: &CoHostTarget<'_>,
) -> PeerHostMenu {
    let host_actions = viewer_is_host && !target.is_self;
    let peer_is_owner = owner_user_id == Some(target.user_id);
    PeerHostMenu {
        mute_and_disable_video: host_actions && !target.is_host,
        kick: host_actions && can_kick(viewer, target.is_host, peer_is_owner),
        transfer: host_actions && !target.is_guest && !target.is_host,
        co_host: co_host_menu_action(viewer, owner_user_id, target),
    }
}

/// A meeting-api refusal, classified so no raw server wording reaches the UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoHostErrorKind {
    LastPresentHost,
    NotACoHost,
    NotInMeeting,
    MeetingNotFound,
    NotOwner,
    NotHost,
    NeedsActiveMeeting,
    LimitReached,
    GuestTarget,
    OwnerTarget,
    SelfTarget,
    TooLong,
    Empty,
    InvalidRequest,
    RateLimited,
    SessionExpired,
    Network,
    Unknown,
}

impl CoHostErrorKind {
    pub fn message(self) -> &'static str {
        match self {
            Self::LastPresentHost => "Can't remove the only host in the meeting.",
            Self::NotACoHost => "They're no longer a co-host or host of this meeting.",
            Self::NotInMeeting => "They're no longer in the meeting.",
            Self::MeetingNotFound => "This meeting could not be found.",
            Self::NotOwner => "Only the meeting owner can do that.",
            Self::NotHost => "Only hosts can do that.",
            Self::NeedsActiveMeeting => {
                "The meeting isn't running, so co-hosts can only be saved for future meetings."
            }
            Self::LimitReached => "A meeting can have at most 100 co-hosts.",
            Self::GuestTarget => "Guests can't be co-hosts.",
            Self::OwnerTarget => "That's you, the meeting owner.",
            Self::SelfTarget => "You can't do that to yourself.",
            Self::TooLong => "That ID is too long (254 characters at most).",
            Self::Empty => "Enter an email or user ID.",
            Self::InvalidRequest => {
                "That request wasn't accepted. Check the details and try again."
            }
            Self::RateLimited => "Too many requests. Wait a moment and try again.",
            Self::SessionExpired => "Your session has expired. Sign in again.",
            Self::Network => "Network error. Check your connection and try again.",
            Self::Unknown => "Something went wrong. Try again.",
        }
    }
}

fn error_body_parts(body: &str) -> (Option<String>, String) {
    let parsed: Option<serde_json::Value> = serde_json::from_str(body).ok();
    let result = parsed.as_ref().map(|v| &v["result"]);
    let code = result.and_then(|r| r["code"].as_str()).map(str::to_string);
    let message = result
        .and_then(|r| r["message"].as_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    (code, message)
}

fn classify_bad_request(message: &str) -> CoHostErrorKind {
    if message.contains("persist") || message.contains("active meeting") {
        CoHostErrorKind::NeedsActiveMeeting
    } else if message.contains("at most") {
        CoHostErrorKind::LimitReached
    } else if message.contains("guest") {
        CoHostErrorKind::GuestTarget
    } else if message.contains("owner") {
        CoHostErrorKind::OwnerTarget
    } else if message.contains("yourself") {
        CoHostErrorKind::SelfTarget
    } else if message.contains("too long") {
        CoHostErrorKind::TooLong
    } else if message.contains("empty") {
        CoHostErrorKind::Empty
    } else {
        CoHostErrorKind::InvalidRequest
    }
}

pub fn classify_co_host_error(error: &JoinError) -> CoHostErrorKind {
    let body = match error {
        JoinError::ServerError { body, .. }
        | JoinError::Forbidden(body)
        | JoinError::NotFound(body) => body.as_str(),
        JoinError::MeetingNotActive => return CoHostErrorKind::NeedsActiveMeeting,
        JoinError::NotAuthenticated => return CoHostErrorKind::SessionExpired,
        JoinError::Network(_) => return CoHostErrorKind::Network,
        JoinError::RateLimitExceeded => return CoHostErrorKind::RateLimited,
        _ => return CoHostErrorKind::Unknown,
    };
    let (code, message) = error_body_parts(body);
    match code.as_deref() {
        Some("LAST_PRESENT_HOST") => CoHostErrorKind::LastPresentHost,
        Some("CO_HOST_NOT_FOUND") => CoHostErrorKind::NotACoHost,
        Some("PARTICIPANT_NOT_FOUND") | Some("NOT_IN_MEETING") => CoHostErrorKind::NotInMeeting,
        Some("MEETING_NOT_FOUND") => CoHostErrorKind::MeetingNotFound,
        Some("NOT_OWNER") => CoHostErrorKind::NotOwner,
        Some("NOT_HOST") => CoHostErrorKind::NotHost,
        Some("MEETING_NOT_ACTIVE") => CoHostErrorKind::NeedsActiveMeeting,
        Some("RATE_LIMIT_EXCEEDED") => CoHostErrorKind::RateLimited,
        Some("BAD_REQUEST") | Some("INVALID_INPUT") => classify_bad_request(&message),
        _ => match error {
            JoinError::Forbidden(_) => CoHostErrorKind::NotOwner,
            JoinError::NotFound(_) => CoHostErrorKind::MeetingNotFound,
            _ => CoHostErrorKind::Unknown,
        },
    }
}

pub fn co_host_error_message(error: &JoinError) -> &'static str {
    classify_co_host_error(error).message()
}

const MAX_CO_HOST_ID_LEN: usize = 254;

/// Mirrors the server's target validation for inline feedback.
pub fn validate_co_host_input(
    raw: &str,
    owner_user_id: Option<&str>,
) -> Result<String, CoHostErrorKind> {
    let id = raw.trim();
    if id.is_empty() {
        return Err(CoHostErrorKind::Empty);
    }
    if id.len() > MAX_CO_HOST_ID_LEN {
        return Err(CoHostErrorKind::TooLong);
    }
    if owner_user_id == Some(id) {
        return Err(CoHostErrorKind::OwnerTarget);
    }
    if id.starts_with(GUEST_USER_ID_PREFIX) {
        return Err(CoHostErrorKind::GuestTarget);
    }
    Ok(id.to_string())
}

/// How a list entry's "save for future meetings" state is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PersistControl {
    Switch,
    Saved,
    ThisMeetingOnly,
    Hidden,
}

/// A re-grant flips the saved state and would also lift a suspension, so a
/// suspended entry gets a static tag; so does un-saving on an idle meeting.
pub fn persist_control(meeting_active: bool, entry: &CoHostEntry) -> PersistControl {
    if !entry.designated {
        return PersistControl::Hidden;
    }
    if entry.suspended || (entry.persistent && !meeting_active) {
        return if entry.persistent {
            PersistControl::Saved
        } else {
            PersistControl::ThisMeetingOnly
        };
    }
    PersistControl::Switch
}

#[derive(Clone, Debug, PartialEq)]
pub struct CoHostNotice {
    pub seq: u64,
    pub text: String,
    pub is_error: bool,
}

#[derive(Clone, Copy, PartialEq)]
pub struct CoHostNoticeCtx(pub Signal<Option<CoHostNotice>>);

const NOTICE_MS: u32 = 6_000;
const REFRESH_DEBOUNCE_MS: u32 = 1_500;

thread_local! {
    static NOTICE_SEQ: Cell<u64> = const { Cell::new(0) };
    static SECTION_IDS: Cell<u64> = const { Cell::new(0) };
}

fn bump(counter: &'static std::thread::LocalKey<Cell<u64>>) -> u64 {
    counter.with(|c| {
        let next = c.get().wrapping_add(1);
        c.set(next);
        next
    })
}

pub fn post_co_host_notice(ctx: CoHostNoticeCtx, text: String, is_error: bool) {
    let seq = bump(&NOTICE_SEQ);
    let mut notice = ctx.0;
    match notice.try_write() {
        Ok(mut slot) => {
            *slot = Some(CoHostNotice {
                seq,
                text,
                is_error,
            })
        }
        Err(_) => return,
    }
    Timeout::new(NOTICE_MS, move || {
        let current = notice
            .try_peek()
            .ok()
            .and_then(|n| n.as_ref().map(|n| n.seq));
        if current == Some(seq) {
            if let Ok(mut slot) = notice.try_write() {
                *slot = None;
            }
        }
    })
    .forget();
}

/// The only reader of the notice signal, so a notice re-renders this layer and
/// not its parent. The status region stays mounted and changes text in place.
#[component]
pub fn CoHostNoticeLayer(notice: Signal<Option<CoHostNotice>>) -> Element {
    let current = notice();
    let status_text = match &current {
        Some(n) if !n.is_error => action_bar_announce_text(&n.text, n.seq as u32),
        _ => String::new(),
    };
    rsx! {
        span {
            class: "visually-hidden",
            role: "status",
            "aria-live": "polite",
            "aria-atomic": "true",
            "data-testid": "co-host-status",
            "{status_text}"
        }
        if let Some(n) = current {
            div {
                key: "{n.seq}",
                class: if n.is_error { "peer-toast toast-left co-host-notice co-host-notice--error" } else { "peer-toast toast-joined co-host-notice" },
                role: n.is_error.then_some("alert"),
                "data-testid": "co-host-notice",
                span { class: "toast-text",
                    span { class: "toast-name", "{n.text}" }
                }
            }
        }
    }
}

/// The local user's "you now have / no longer have host controls" toast, with
/// an always-mounted status region beside it.
#[component]
pub fn HostChangeNotice(toast: Signal<Option<String>>) -> Element {
    let shown = use_hook(|| Rc::new(Cell::new(0u32)));
    let current = toast();
    let status_text = match &current {
        Some(text) => {
            shown.set(shown.get().wrapping_add(1));
            action_bar_announce_text(text, shown.get())
        }
        None => String::new(),
    };
    rsx! {
        span {
            class: "visually-hidden",
            role: "status",
            "aria-live": "polite",
            "aria-atomic": "true",
            "data-testid": "host-change-status",
            "{status_text}"
        }
        if let Some(text) = current {
            div { class: "peer-toast toast-joined", "data-testid": "host-change-toast",
                span { class: "toast-icon",
                    svg {
                        width: "16",
                        height: "16",
                        view_box: "0 0 24 24",
                        fill: "none",
                        stroke: "currentColor",
                        stroke_width: "2",
                        stroke_linecap: "round",
                        stroke_linejoin: "round",
                        "aria-hidden": "true",
                        path { d: "M2 18h20l-2-9-4 4-4-7-4 7-4-4-2 9Z" }
                    }
                }
                span { class: "toast-text",
                    span { class: "toast-name", "{text}" }
                }
            }
        }
    }
}

/// A peer-menu co-host action as plain data, run from the element's onclick.
#[derive(Clone, Debug, PartialEq)]
pub struct CoHostRequest {
    pub action: CoHostMenuAction,
    pub meeting_id: String,
    pub user_id: String,
    pub display_name: String,
}

pub fn run_co_host_request(request: CoHostRequest, notice: Option<CoHostNoticeCtx>) {
    wasm_bindgen_futures::spawn_local(async move {
        let CoHostRequest {
            action,
            meeting_id,
            user_id,
            display_name,
        } = request;
        let result = match action {
            // `None`: a false here would un-save a saved co-host who is paused.
            CoHostMenuAction::Make => {
                crate::meeting_api::grant_co_host(&meeting_id, &user_id, None).await
            }
            CoHostMenuAction::Remove | CoHostMenuAction::RemoveHostRole => {
                crate::meeting_api::revoke_co_host(&meeting_id, &user_id).await
            }
        };
        let (text, is_error) = match result {
            Ok(_) => (action.success_text(&display_name), false),
            Err(e) => {
                log::warn!("co-host {action:?} for {user_id} failed: {e}");
                (
                    action.failure_text(&display_name, co_host_error_message(&e)),
                    true,
                )
            }
        };
        if let Some(ctx) = notice {
            post_co_host_notice(ctx, text, is_error);
        }
    });
}

pub fn kick_failure_text(name: &str, error: &JoinError) -> String {
    format!(
        "Couldn't remove {name} from the meeting. {}",
        co_host_error_message(error)
    )
}

#[derive(Clone, Debug, PartialEq)]
enum ListState {
    Loading,
    Ready,
    Forbidden,
    Failed { reason: &'static str, attempt: u64 },
}

#[derive(Clone, Debug, PartialEq)]
enum Mutation {
    /// No `persist`: a new grant is saved for future meetings by default.
    Add { user_id: String },
    SetPersist {
        user_id: String,
        name: String,
        persist: bool,
    },
    Remove {
        user_id: String,
        name: String,
        focus_next: String,
    },
}

impl Mutation {
    fn success_text(&self) -> String {
        match self {
            Mutation::Add { user_id } => {
                format!("{user_id} was added as a co-host and saved for future meetings.")
            }
            Mutation::SetPersist {
                name,
                persist: true,
                ..
            } => format!("{name} is saved as a co-host for future meetings."),
            Mutation::SetPersist {
                name,
                persist: false,
                ..
            } => format!("{name} is a co-host for this meeting only."),
            Mutation::Remove { name, .. } => CoHostMenuAction::Remove.success_text(name),
        }
    }

    fn failure_text(&self, reason: &str) -> String {
        match self {
            Mutation::Add { user_id } => {
                format!("Couldn't add {user_id} as a co-host. {reason}")
            }
            Mutation::SetPersist { name, .. } => format!("Couldn't update {name}. {reason}"),
            Mutation::Remove { name, .. } => CoHostMenuAction::Remove.failure_text(name, reason),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct SectionMessage {
    seq: u64,
    text: String,
    is_error: bool,
}

fn entry_name(entry: &CoHostEntry) -> String {
    entry
        .display_name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| entry.user_id.clone())
}

fn remove_button_id(base: &str, user_id: &str) -> String {
    format!("{base}-remove-{user_id}")
}

/// Where focus goes once `rows[index]` is removed: the next row's Remove, else
/// the Add input.
fn focus_after_removal(base: &str, rows: &[CoHostEntry], index: usize) -> String {
    match rows.get(index + 1) {
        Some(next) => remove_button_id(base, &next.user_id),
        None => format!("{base}-input"),
    }
}

/// Owner-only co-host management, rendered wherever `MeetingOptionsControls`
/// is. Presence comes from `HostSetCtx` when present; `refresh` re-fetches the
/// list on a trailing debounce.
#[component]
pub fn CoHostsSection(
    meeting_id: String,
    #[props(default)] owner_user_id: Option<String>,
    meeting_active: bool,
    #[props(default)] refresh: Option<Signal<u64>>,
    #[props(default)] collapsible: bool,
    /// Title the section like its sibling settings cards.
    #[props(default)]
    card_title: bool,
    /// Renders the roster with no add input, no Remove buttons, and no save
    /// switches; grant/revoke stay owner-only regardless.
    #[props(default)]
    read_only: bool,
) -> Element {
    let host_set = try_use_context::<HostSetCtx>();
    let mut entries = use_signal(Vec::<CoHostEntry>::new);
    let mut list_state = use_signal(|| ListState::Loading);
    let mut busy = use_signal(|| false);
    let mut retrying = use_signal(|| false);
    let mut message = use_signal(|| None::<SectionMessage>);
    let mut draft = use_signal(String::new);
    let mut draft_error = use_signal(|| None::<CoHostErrorKind>);
    // The server said the meeting isn't running; cleared when it becomes active.
    let known_inactive = use_hook(|| Rc::new(Cell::new(false)));
    let was_active = use_hook(|| Rc::new(Cell::new(meeting_active)));
    if meeting_active && !was_active.get() {
        known_inactive.set(false);
    }
    was_active.set(meeting_active);
    let message_seq = use_hook(|| Rc::new(Cell::new(0u64)));
    let mut reload = use_signal(|| 0u64);
    let fetch_generation = use_hook(|| Rc::new(Cell::new(0u64)));
    // The generation of whichever fetch is entitled to clear `retrying`: the
    // one in flight when it was set true. A later fetch that starts while
    // still retrying takes over that entitlement, so an older, superseded
    // fetch landing afterward cannot clear it out from under a newer one
    // that is still pending.
    let retry_generation: Rc<Cell<Option<u64>>> = use_hook(|| Rc::new(Cell::new(None)));
    let pending_refresh: Rc<RefCell<Option<Timeout>>> = use_hook(|| Rc::new(RefCell::new(None)));
    let last_refresh: Rc<Cell<Option<u64>>> = use_hook(|| Rc::new(Cell::new(None)));
    let base = use_hook(|| format!("co-hosts-{}", bump(&SECTION_IDS)));
    let input_id = format!("{base}-input");
    let active = meeting_active && !known_inactive.get();

    {
        let meeting_id = meeting_id.clone();
        let fetch_generation = fetch_generation.clone();
        let retry_generation = retry_generation.clone();
        let input_id = input_id.clone();
        use_effect(move || {
            let _ = reload();
            let generation = fetch_generation.get().wrapping_add(1);
            fetch_generation.set(generation);
            // Still retrying when this fetch launches: it inherits the
            // entitlement to clear `retrying`, taking it over from whatever
            // earlier fetch held it.
            if *retrying.peek() {
                retry_generation.set(Some(generation));
            }
            let meeting_id = meeting_id.clone();
            let fetch_generation = fetch_generation.clone();
            let retry_generation = retry_generation.clone();
            let input_id = input_id.clone();
            spawn(async move {
                let result = crate::meeting_api::list_co_hosts(&meeting_id).await;
                let was_retry = *retrying.peek();
                if retry_generation.get() == Some(generation) {
                    retrying.set(false);
                }
                if fetch_generation.get() != generation {
                    return;
                }
                match result {
                    Ok(list) => {
                        entries.set(list);
                        list_state.set(ListState::Ready);
                        if was_retry {
                            focus_element_by_id(&input_id);
                        }
                    }
                    Err(e) => match classify_co_host_error(&e) {
                        CoHostErrorKind::NotOwner => list_state.set(ListState::Forbidden),
                        kind => {
                            log::warn!("list_co_hosts failed: {e}");
                            list_state.set(ListState::Failed {
                                reason: kind.message(),
                                attempt: generation,
                            });
                        }
                    },
                }
            });
        });
    }

    use_effect(move || {
        let Some(refresh) = refresh else {
            return;
        };
        let value = refresh();
        if last_refresh.replace(Some(value)).is_none() {
            return;
        }
        *pending_refresh.borrow_mut() = Some(Timeout::new(REFRESH_DEBOUNCE_MS, move || {
            if let Ok(mut r) = reload.try_write() {
                *r = r.wrapping_add(1);
            }
        }));
    });

    let mutate = {
        let meeting_id = meeting_id.clone();
        let fetch_generation = fetch_generation.clone();
        let input_id = input_id.clone();
        let known_inactive = known_inactive.clone();
        let message_seq = message_seq.clone();
        let base = base.clone();
        use_callback(move |mutation: Mutation| {
            if *busy.peek() {
                return;
            }
            busy.set(true);
            let meeting_id = meeting_id.clone();
            let fetch_generation = fetch_generation.clone();
            let input_id = input_id.clone();
            let known_inactive = known_inactive.clone();
            let message_seq = message_seq.clone();
            let base = base.clone();
            spawn(async move {
                let result = match &mutation {
                    // No explicit `persist`: saved for future meetings by default.
                    Mutation::Add { user_id } => {
                        crate::meeting_api::grant_co_host(&meeting_id, user_id, None).await
                    }
                    Mutation::SetPersist {
                        user_id, persist, ..
                    } => {
                        crate::meeting_api::grant_co_host(&meeting_id, user_id, Some(*persist))
                            .await
                    }
                    Mutation::Remove { user_id, .. } => {
                        crate::meeting_api::revoke_co_host(&meeting_id, user_id).await
                    }
                };
                fetch_generation.set(fetch_generation.get().wrapping_add(1));
                busy.set(false);
                let mut focus_target = None;
                let (text, is_error) = match result {
                    Ok(list) => {
                        entries.set(list);
                        list_state.set(ListState::Ready);
                        match &mutation {
                            Mutation::Add { .. } => draft.set(String::new()),
                            Mutation::Remove { focus_next, .. } => {
                                focus_target = Some(focus_next.clone())
                            }
                            Mutation::SetPersist { .. } => {}
                        }
                        (mutation.success_text(), false)
                    }
                    Err(e) => {
                        log::warn!("co-host change failed: {e}");
                        let kind = classify_co_host_error(&e);
                        if kind == CoHostErrorKind::NeedsActiveMeeting {
                            known_inactive.set(true);
                        }
                        focus_target = match &mutation {
                            Mutation::Add { .. } => Some(input_id),
                            // Its switch turns into a static tag.
                            Mutation::SetPersist { user_id, .. }
                                if kind == CoHostErrorKind::NeedsActiveMeeting =>
                            {
                                Some(remove_button_id(&base, user_id))
                            }
                            _ => None,
                        };
                        (mutation.failure_text(kind.message()), true)
                    }
                };
                message_seq.set(message_seq.get().wrapping_add(1));
                message.set(Some(SectionMessage {
                    seq: message_seq.get(),
                    text,
                    is_error,
                }));
                if let Some(id) = focus_target {
                    // After the re-render that drops the row or the switch.
                    gloo_timers::future::TimeoutFuture::new(0).await;
                    focus_element_by_id(&id);
                }
            });
        })
    };

    let on_submit = {
        let owner_user_id = owner_user_id.clone();
        let input_id = input_id.clone();
        move |e: FormEvent| {
            e.prevent_default();
            if *busy.peek() {
                return;
            }
            match validate_co_host_input(&draft.peek(), owner_user_id.as_deref()) {
                Ok(user_id) => {
                    draft_error.set(None);
                    mutate.call(Mutation::Add { user_id });
                }
                Err(kind) => {
                    draft_error.set(Some(kind));
                    focus_element_by_id(&input_id);
                }
            }
        }
    };

    let state = list_state();
    let rows = entries();
    let is_busy = busy();
    let is_retrying = retrying();
    let busy_attr = is_busy.then_some("true");
    let input_error_id = format!("{base}-input-error");
    let heading_id = format!("{base}-heading");
    let invalid = draft_error().is_some();
    let status_text = match message() {
        Some(msg) if !msg.is_error => action_bar_announce_text(&msg.text, msg.seq as u32),
        _ => String::new(),
    };
    let error_msg = message().filter(|m| m.is_error);
    let summary_count = match state {
        ListState::Ready => format!(" ({})", rows.len()),
        _ => String::new(),
    };

    let content = rsx! {
        if is_busy {
            span { class: "co-hosts-busy", "aria-hidden": "true", "Saving…" }
        }
        match state.clone() {
            ListState::Loading => rsx! {
                p { class: "co-hosts-empty", "Loading co-hosts…" }
            },
            ListState::Forbidden => rsx! {
                p { class: "co-hosts-empty", "data-testid": "co-hosts-forbidden",
                    "Co-hosts can only be viewed and changed by the meeting owner."
                }
            },
            ListState::Failed { reason, attempt } => rsx! {
                div { class: "co-hosts-failed",
                    // Keyed per attempt so a repeated identical failure is re-announced.
                    {rsx! {
                        p {
                            key: "{attempt}",
                            class: "toggle-error",
                            role: "alert",
                            "data-testid": "co-hosts-load-error",
                            "Couldn't load co-hosts. {reason}"
                        }
                    }}
                    button {
                        r#type: "button",
                        class: "btn-apple btn-secondary btn-sm",
                        "data-testid": "co-hosts-retry",
                        "aria-disabled": is_retrying.then_some("true"),
                        onclick: move |_| {
                            if *retrying.peek() {
                                return;
                            }
                            retrying.set(true);
                            let next = reload.peek().wrapping_add(1);
                            reload.set(next);
                        },
                        if is_retrying { "Retrying…" } else { "Try again" }
                    }
                }
            },
            ListState::Ready if rows.is_empty() => rsx! {
                p { class: "co-hosts-empty", "data-testid": "co-hosts-empty", "No co-hosts yet." }
            },
            ListState::Ready => rsx! {
                ul { class: "co-hosts-list", "aria-busy": busy_attr,
                    for (index, entry) in rows.iter().cloned().enumerate() {
                        {
                            let name = entry_name(&entry);
                            let present = host_set
                                .as_ref()
                                .map(|hs| hs.is_host(&entry.user_id))
                                .unwrap_or(entry.is_present_host);
                            // A read-only viewer never gets the interactive
                            // switch: the entry's save state still needs
                            // showing, so `Switch` degrades to the matching
                            // static tag instead of being hidden outright.
                            let control = match persist_control(active, &entry) {
                                PersistControl::Switch if read_only => {
                                    if entry.persistent {
                                        PersistControl::Saved
                                    } else {
                                        PersistControl::ThisMeetingOnly
                                    }
                                }
                                other => other,
                            };
                            let toggle_user = entry.user_id.clone();
                            let toggle_name = name.clone();
                            let remove = Mutation::Remove {
                                user_id: entry.user_id.clone(),
                                name: name.clone(),
                                focus_next: focus_after_removal(&base, &rows, index),
                            };
                            rsx! {
                                li {
                                    key: "{entry.user_id}",
                                    class: "co-hosts-row",
                                    "data-testid": "co-host-row",
                                    "data-user-id": "{entry.user_id}",
                                    div { class: "co-hosts-identity",
                                        span { class: "co-hosts-name", "{name}" }
                                        if entry.display_name.is_some() {
                                            span { class: "co-hosts-id", "{entry.user_id}" }
                                        }
                                        if present {
                                            span {
                                                class: "co-hosts-tag co-hosts-present",
                                                "data-testid": "co-host-present",
                                                "In meeting"
                                            }
                                        }
                                        if entry.suspended {
                                            span {
                                                class: "co-hosts-tag",
                                                "data-testid": "co-host-suspended",
                                                "Paused for this meeting. Add them again to restore."
                                            }
                                        }
                                        if !entry.designated {
                                            span {
                                                class: "co-hosts-tag",
                                                "data-testid": "co-host-undesignated",
                                                "Host by transfer"
                                            }
                                        }
                                    }
                                    div { class: "co-hosts-controls",
                                        match control {
                                            PersistControl::Switch => rsx! {
                                                span { class: "co-hosts-persist-label", aria_hidden: "true",
                                                    "Save for future meetings"
                                                }
                                                ToggleSwitch {
                                                    enabled: entry.persistent,
                                                    soft_disabled: is_busy,
                                                    aria_label: format!("Save for future meetings ({name})"),
                                                    on_toggle: move |persist: bool| {
                                                        mutate.call(Mutation::SetPersist {
                                                            user_id: toggle_user.clone(),
                                                            name: toggle_name.clone(),
                                                            persist,
                                                        });
                                                    },
                                                }
                                            },
                                            PersistControl::Saved => rsx! {
                                                span { class: "co-hosts-tag", "data-testid": "co-host-saved", "Saved" }
                                            },
                                            PersistControl::ThisMeetingOnly => rsx! {
                                                span { class: "co-hosts-tag", "This meeting only" }
                                            },
                                            PersistControl::Hidden => rsx! {},
                                        }
                                        if !read_only {
                                            button {
                                                r#type: "button",
                                                id: remove_button_id(&base, &entry.user_id),
                                                class: "btn-apple btn-secondary btn-sm",
                                                "data-testid": "co-host-remove",
                                                "aria-disabled": busy_attr,
                                                "aria-label": "Remove {name} as co-host",
                                                onclick: move |_| mutate.call(remove.clone()),
                                                "Remove"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            },
        }
        if !read_only && !matches!(state, ListState::Forbidden | ListState::Loading) {
            form { class: "co-hosts-add", onsubmit: on_submit,
                // Visually hidden: the compact one-line row relies on the
                // input's placeholder for sighted users, but keeps a real
                // programmatic label for assistive tech.
                label { r#for: "{input_id}", class: "visually-hidden", "Add co-host" }
                div { class: "co-hosts-add-row",
                    input {
                        id: "{input_id}",
                        class: "input-apple co-hosts-input",
                        "data-testid": "co-host-input",
                        r#type: "text",
                        autocomplete: "off",
                        placeholder: "Email or user ID",
                        value: "{draft}",
                        "aria-invalid": invalid.then_some("true"),
                        "aria-describedby": invalid.then(|| input_error_id.clone()),
                        oninput: move |e: FormEvent| {
                            draft.set(e.value());
                            draft_error.set(None);
                        },
                    }
                    button {
                        r#type: "submit",
                        class: "btn-apple btn-primary co-hosts-add-btn",
                        "data-testid": "co-host-add",
                        "aria-disabled": busy_attr,
                        "Add"
                    }
                }
                if let Some(kind) = draft_error() {
                    p { id: "{input_error_id}", class: "toggle-error", role: "alert", "{kind.message()}" }
                }
                p { class: "co-hosts-hint", "New co-hosts are saved for future meetings." }
            }
        }
        p {
            class: "co-hosts-message",
            role: "status",
            "aria-live": "polite",
            "aria-atomic": "true",
            "data-testid": "co-hosts-status",
            "{status_text}"
        }
        if let Some(msg) = error_msg {
            p {
                key: "{msg.seq}",
                class: "toggle-error co-hosts-message",
                role: "alert",
                "data-testid": "co-hosts-error",
                "{msg.text}"
            }
        }
    };

    // A read-only viewer (a co-host or present host who cannot manage
    // co-hosts) sees why the list has no controls; the owner sees what the
    // controls do.
    let hint_text = if read_only {
        "Managed by the meeting owner."
    } else {
        "Co-hosts share your host controls and can change meeting options. Only you can manage co-hosts."
    };

    rsx! {
        section {
            class: if collapsible { "co-hosts-section co-hosts-section--collapsible" } else { "co-hosts-section" },
            "data-testid": "co-hosts-section",
            "aria-labelledby": "{heading_id}",
            // Reflects `retrying` regardless of which list-state branch is
            // showing, so a superseded retry's response cannot leave a signal
            // stuck true with no visible surface to catch it.
            "data-retrying": is_retrying.then_some("true"),
            "data-read-only": read_only.then_some("true"),
            if collapsible {
                details { class: "co-hosts-details",
                    summary { class: "co-hosts-summary",
                        span { id: "{heading_id}", class: "co-hosts-heading", "Co-hosts" }
                        if !summary_count.is_empty() {
                            span { class: "co-hosts-count", "{summary_count}" }
                        }
                        svg {
                            class: "co-hosts-chevron",
                            "aria-hidden": "true",
                            xmlns: "http://www.w3.org/2000/svg",
                            width: "16",
                            height: "16",
                            view_box: "0 0 24 24",
                            fill: "none",
                            stroke: "currentColor",
                            stroke_width: "2",
                            stroke_linecap: "round",
                            stroke_linejoin: "round",
                            polyline { points: "6 9 12 15 18 9" }
                        }
                    }
                    div { class: "co-hosts-body",
                        p { class: "co-hosts-hint", "{hint_text}" }
                        {content}
                    }
                }
            } else {
                if card_title {
                    h3 { id: "{heading_id}", class: "settings-card-title", "Co-hosts" }
                } else {
                    h4 { id: "{heading_id}", class: "co-hosts-heading", "Co-hosts" }
                }
                p { class: "co-hosts-hint", "{hint_text}" }
                {content}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: &str = "owner@example.com";
    const COHOST: &str = "cohost@example.com";
    const ALICE: &str = "alice@example.com";

    fn owner() -> MeetingOwnership {
        MeetingOwnership::of(Some(OWNER), Some(OWNER))
    }

    fn co_host_viewer() -> MeetingOwnership {
        MeetingOwnership::of(Some(OWNER), Some(COHOST))
    }

    fn target(user_id: &str, is_host: bool) -> CoHostTarget<'_> {
        CoHostTarget {
            user_id,
            is_self: false,
            is_guest: false,
            is_host,
            owner_holds_host: true,
        }
    }

    #[test]
    fn a_host_shown_as_host_is_offered_remove_host_role() {
        let mut transfer_target = target(ALICE, true);
        transfer_target.owner_holds_host = false;
        assert_eq!(
            co_host_menu_action(owner(), Some(OWNER), &transfer_target),
            Some(CoHostMenuAction::RemoveHostRole)
        );
        assert_eq!(CoHostMenuAction::RemoveHostRole.label(), "Remove host role");
        assert_eq!(
            co_host_menu_action(owner(), Some(OWNER), &target(ALICE, true)),
            Some(CoHostMenuAction::Remove)
        );
    }

    #[test]
    fn the_menu_toast_confirms_the_save_unconditionally() {
        let text = CoHostMenuAction::Make.success_text("Alice");
        assert_eq!(
            text,
            "Alice is now a co-host and saved for future meetings."
        );
        assert!(
            !text.contains("Meeting Options"),
            "no more how-to-save instructions: {text}"
        );
    }

    fn body(code: &str, message: &str) -> String {
        format!(r#"{{"success":false,"result":{{"code":"{code}","message":"{message}"}}}}"#)
    }

    fn server_error(status: u16, code: &str, message: &str) -> JoinError {
        JoinError::ServerError {
            status,
            body: body(code, message),
        }
    }

    fn entry(persistent: bool, designated: bool, suspended: bool) -> CoHostEntry {
        CoHostEntry {
            user_id: ALICE.to_string(),
            persistent,
            is_present_host: false,
            display_name: None,
            designated,
            suspended,
        }
    }

    #[test]
    fn ownership_is_the_creator_not_the_host_role() {
        assert!(MeetingOwnership::of(Some(OWNER), Some(OWNER)).is_owner());
        assert!(!MeetingOwnership::of(Some(OWNER), Some(COHOST)).is_owner());
        assert!(!MeetingOwnership::of(None, Some(COHOST)).is_owner());
        assert!(!MeetingOwnership::of(Some(OWNER), None).is_owner());
        assert!(!MeetingOwnership::of(Some(""), Some("")).is_owner());
        assert!(!MeetingOwnership::default().is_owner());
    }

    #[test]
    fn only_the_owner_is_offered_co_host_actions() {
        assert_eq!(
            co_host_menu_action(co_host_viewer(), Some(OWNER), &target(ALICE, false)),
            None
        );
        assert_eq!(
            co_host_menu_action(co_host_viewer(), Some(OWNER), &target(ALICE, true)),
            None
        );
        assert_eq!(
            co_host_menu_action(owner(), Some(OWNER), &target(ALICE, false)),
            Some(CoHostMenuAction::Make)
        );
        assert_eq!(
            co_host_menu_action(owner(), Some(OWNER), &target(ALICE, true)),
            Some(CoHostMenuAction::Remove)
        );
    }

    #[test]
    fn make_co_host_skips_self_guests_and_the_owner() {
        let mut me = target(ALICE, false);
        me.is_self = true;
        assert_eq!(co_host_menu_action(owner(), Some(OWNER), &me), None);

        let mut guest = target("guest:1234", false);
        assert_eq!(co_host_menu_action(owner(), Some(OWNER), &guest), None);
        guest.user_id = ALICE;
        guest.is_guest = true;
        assert_eq!(co_host_menu_action(owner(), Some(OWNER), &guest), None);

        assert_eq!(
            co_host_menu_action(owner(), Some(OWNER), &target(OWNER, true)),
            None
        );
        assert_eq!(
            co_host_menu_action(owner(), Some(OWNER), &target("", false)),
            None
        );
    }

    #[test]
    fn label_rule_truth_table() {
        use HostRole::{CoHost, Host};
        // (is_host, user, owner_holds_host) -> role
        let cases = [
            (false, OWNER, true, None),
            (false, ALICE, true, None),
            (true, OWNER, true, Some(Host)),
            (true, ALICE, true, Some(CoHost)),
            (true, OWNER, false, Some(Host)),
            (true, ALICE, false, Some(Host)),
        ];
        for (is_host, user, owner_holds, want) in cases {
            assert_eq!(
                host_role(is_host, user, Some(OWNER), owner_holds),
                want,
                "is_host={is_host} user={user} owner_holds_host={owner_holds}"
            );
        }
        assert_eq!(host_role(true, ALICE, None, false), Some(Host));
        assert_eq!(HostRole::CoHost.label(), "Co-host");
        assert_eq!(HostRole::Host.label(), "Host");
    }

    #[test]
    fn indicator_names_the_role() {
        assert_eq!(
            host_indicator(false, Some(HostRole::CoHost)),
            Some("(Co-host)")
        );
        assert_eq!(host_indicator(false, Some(HostRole::Host)), Some("(Host)"));
        assert_eq!(
            host_indicator(true, Some(HostRole::CoHost)),
            Some("(You/Co-host)")
        );
        assert_eq!(
            host_indicator(true, Some(HostRole::Host)),
            Some("(You/Host)")
        );
        assert_eq!(host_indicator(true, None), Some("(You)"));
        assert_eq!(host_indicator(false, None), None);
    }

    #[test]
    fn self_toast_is_role_neutral() {
        assert_eq!(host_change_toast_text(true), "You now have host controls");
        assert_eq!(
            host_change_toast_text(false),
            "You no longer have host controls"
        );
    }

    #[test]
    fn meeting_options_are_editable_by_the_owner_or_any_current_host() {
        // The owner, even without the live host role (e.g. after transferring
        // it away), can still edit options.
        assert!(can_edit_meeting_options(owner(), false));
        // A co-host currently holding the host role can edit options too.
        assert!(can_edit_meeting_options(co_host_viewer(), true));
        // Neither owner nor host: no.
        assert!(!can_edit_meeting_options(co_host_viewer(), false));
    }

    #[test]
    fn kick_is_hidden_from_a_co_host_on_hosts_and_the_owner() {
        assert!(!can_kick(co_host_viewer(), true, false));
        assert!(!can_kick(co_host_viewer(), false, true));
        assert!(!can_kick(co_host_viewer(), true, true));
        assert!(can_kick(co_host_viewer(), false, false));
        assert!(can_kick(owner(), true, false));

        let menu = peer_host_menu(true, co_host_viewer(), Some(OWNER), &target(OWNER, false));
        assert!(
            !menu.kick,
            "the owner, even without the role, is not kickable"
        );
        let menu = peer_host_menu(true, co_host_viewer(), Some(OWNER), &target(ALICE, true));
        assert!(!menu.kick);
        let menu = peer_host_menu(true, co_host_viewer(), Some(OWNER), &target(ALICE, false));
        assert!(menu.kick && menu.transfer);
        let menu = peer_host_menu(true, owner(), Some(OWNER), &target(ALICE, true));
        assert!(menu.kick);
    }

    #[test]
    fn mute_and_disable_video_are_not_offered_on_a_role_holder() {
        for viewer in [owner(), co_host_viewer()] {
            assert!(
                !peer_host_menu(true, viewer, Some(OWNER), &target(ALICE, true))
                    .mute_and_disable_video
            );
            assert!(
                !peer_host_menu(true, viewer, Some(OWNER), &target(OWNER, true))
                    .mute_and_disable_video
            );
            assert!(
                peer_host_menu(true, viewer, Some(OWNER), &target(ALICE, false))
                    .mute_and_disable_video
            );
        }
        assert!(
            !peer_host_menu(false, owner(), Some(OWNER), &target(ALICE, false))
                .mute_and_disable_video
        );
        assert_eq!(
            peer_host_menu(true, co_host_viewer(), Some(OWNER), &target(OWNER, true)),
            PeerHostMenu::default(),
            "a co-host has no host action on the owner"
        );
    }

    #[test]
    fn transfer_is_hidden_on_hosts_guests_and_self() {
        assert!(!peer_host_menu(true, owner(), Some(OWNER), &target(ALICE, true)).transfer);
        let mut guest = target(ALICE, false);
        guest.is_guest = true;
        assert!(!peer_host_menu(true, owner(), Some(OWNER), &guest).transfer);
        let mut me = target(ALICE, false);
        me.is_self = true;
        assert_eq!(
            peer_host_menu(true, owner(), Some(OWNER), &me),
            PeerHostMenu::default()
        );
        assert!(!peer_host_menu(false, co_host_viewer(), Some(OWNER), &target(ALICE, false)).kick);
    }

    #[test]
    fn every_server_code_maps_to_friendly_copy() {
        let cases = [
            (
                server_error(409, "LAST_PRESENT_HOST", "Cannot remove the only host."),
                CoHostErrorKind::LastPresentHost,
            ),
            (
                JoinError::NotFound(body("CO_HOST_NOT_FOUND", "'a' is not a co-host")),
                CoHostErrorKind::NotACoHost,
            ),
            (
                JoinError::NotFound(body("PARTICIPANT_NOT_FOUND", "Participant 'a' is not")),
                CoHostErrorKind::NotInMeeting,
            ),
            (
                JoinError::NotFound(body("MEETING_NOT_FOUND", "no meeting")),
                CoHostErrorKind::MeetingNotFound,
            ),
            (
                JoinError::Forbidden(body("NOT_OWNER", "not the owner")),
                CoHostErrorKind::NotOwner,
            ),
            (
                server_error(
                    400,
                    "BAD_REQUEST",
                    "an instance-only co-host requires an active meeting; set persist to save it",
                ),
                CoHostErrorKind::NeedsActiveMeeting,
            ),
            (
                server_error(
                    400,
                    "BAD_REQUEST",
                    "a meeting can have at most 100 co-hosts",
                ),
                CoHostErrorKind::LimitReached,
            ),
            (
                server_error(
                    400,
                    "BAD_REQUEST",
                    "a guest participant cannot be a co-host",
                ),
                CoHostErrorKind::GuestTarget,
            ),
            (
                server_error(400, "BAD_REQUEST", "the meeting owner is always a host"),
                CoHostErrorKind::OwnerTarget,
            ),
            (
                server_error(400, "BAD_REQUEST", "cannot kick yourself"),
                CoHostErrorKind::SelfTarget,
            ),
            (
                server_error(400, "BAD_REQUEST", "something new"),
                CoHostErrorKind::InvalidRequest,
            ),
            (
                JoinError::ServerError {
                    status: 500,
                    body: "not json".to_string(),
                },
                CoHostErrorKind::Unknown,
            ),
        ];
        for (error, want) in cases {
            let kind = classify_co_host_error(&error);
            assert_eq!(kind, want, "{error}");
            let copy = co_host_error_message(&error);
            assert!(!copy.contains("persist"), "raw API wording leaked: {copy}");
            assert!(copy.ends_with('.'), "{copy}");
        }
    }

    #[test]
    fn input_validation_mirrors_the_server() {
        assert_eq!(
            validate_co_host_input("  a@example.com ", Some(OWNER)),
            Ok("a@example.com".to_string())
        );
        assert_eq!(
            validate_co_host_input("   ", Some(OWNER)),
            Err(CoHostErrorKind::Empty)
        );
        assert_eq!(
            validate_co_host_input(OWNER, Some(OWNER)),
            Err(CoHostErrorKind::OwnerTarget)
        );
        assert_eq!(
            CoHostErrorKind::OwnerTarget.message(),
            "That's you, the meeting owner."
        );
        assert!(validate_co_host_input("guest:1", Some(OWNER)).is_err());
        assert!(validate_co_host_input(&"a".repeat(255), Some(OWNER)).is_err());
        assert!(validate_co_host_input(&"a".repeat(254), Some(OWNER)).is_ok());
    }

    #[test]
    fn persist_control_by_entry_state() {
        use PersistControl::*;
        assert_eq!(persist_control(true, &entry(true, true, false)), Switch);
        assert_eq!(persist_control(true, &entry(false, true, false)), Switch);
        assert_eq!(persist_control(false, &entry(false, true, false)), Switch);
        assert_eq!(persist_control(false, &entry(true, true, false)), Saved);
        assert_eq!(persist_control(true, &entry(true, true, true)), Saved);
        assert_eq!(
            persist_control(true, &entry(false, true, true)),
            ThisMeetingOnly
        );
        assert_eq!(persist_control(true, &entry(false, false, false)), Hidden);
    }

    #[test]
    fn removal_focuses_the_next_row_else_the_input() {
        let rows = vec![entry(true, true, false), {
            let mut e = entry(true, true, false);
            e.user_id = "bob@example.com".into();
            e
        }];
        assert_eq!(
            focus_after_removal("b", &rows, 0),
            "b-remove-bob@example.com"
        );
        assert_eq!(focus_after_removal("b", &rows, 1), "b-input");
    }
}
