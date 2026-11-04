// SPDX-License-Identifier: MIT OR Apache-2.0

//! Participant avatars (issue 3125): the circle drawn in a video-off tile, the
//! per-meeting peer avatar cache, and the signed-in user's own avatar.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::{Rc, Weak};

use dioxus::prelude::*;
use futures::channel::oneshot;
use futures::future::{select, Either, FutureExt, LocalBoxFuture, Shared};
use gloo_timers::future::TimeoutFuture;
use videocall_meeting_client::avatars::AvatarImage as AvatarBytes;
use videocall_meeting_client::{ApiError, AuthMode, MeetingApiClient};
use videocall_meeting_types::avatar::AvatarInfo;
use videocall_meeting_types::GUEST_USER_ID_PREFIX;

use crate::components::icons::peer::PeerIcon;

const DISPLAYABLE_AVATAR_TYPES: [&str; 3] = ["image/jpeg", "image/png", "image/webp"];

pub const MAX_PEER_AVATAR_ATTEMPTS: u8 = 2;

pub const MAX_AVATAR_FETCHES_IN_FLIGHT: usize = 4;

const RETIRED_URL_REVOKE_DELAY_MS: u32 = 1_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AvatarTimings {
    pub retry_ms: u32,
    pub retry_jitter_ms: u32,
    pub prune_grace_ms: u32,
    pub save_timeout_ms: u32,
    pub lookup_timeout_ms: u32,
    pub own_refresh_wait_ms: u32,
    pub refresh_jitter_ms: u32,
}

pub const AVATAR_TIMINGS: AvatarTimings = AvatarTimings {
    retry_ms: 30_000,
    retry_jitter_ms: 10_000,
    prune_grace_ms: 60_000,
    save_timeout_ms: 30_000,
    lookup_timeout_ms: 15_000,
    own_refresh_wait_ms: 30_000,
    refresh_jitter_ms: 1_500,
};

thread_local! {
    static OWN_REFRESH_WAITERS: RefCell<Vec<oneshot::Sender<()>>> = const { RefCell::new(Vec::new()) };
}

/// An `ApiError` for logs. Its `Display` can carry the request URL, and with it a peer's user id.
pub fn api_error_kind(error: &ApiError) -> String {
    match error {
        ApiError::NotAuthenticated => "401".to_string(),
        ApiError::Forbidden(_) => "403".to_string(),
        ApiError::NotFound(_) => "404".to_string(),
        ApiError::RateLimitExceeded => "429".to_string(),
        ApiError::ServerError { status, .. } => status.to_string(),
        ApiError::Network(_) => "network error".to_string(),
        ApiError::Config(_) => "configuration error".to_string(),
        _ => "unexpected response".to_string(),
    }
}

#[derive(Debug)]
enum LookupFailure {
    Api(ApiError),
    TimedOut,
}

impl LookupFailure {
    fn log_kind(&self) -> String {
        match self {
            LookupFailure::Api(error) => api_error_kind(error),
            LookupFailure::TimedOut => "timed out".to_string(),
        }
    }
}

type LookupResult = Result<Option<AvatarBytes>, LookupFailure>;

#[cfg(test)]
thread_local! {
    static TEST_TIMINGS: Cell<Option<AvatarTimings>> = const { Cell::new(None) };
}

#[cfg(not(test))]
pub(crate) fn avatar_timings() -> AvatarTimings {
    AVATAR_TIMINGS
}

#[cfg(test)]
pub(crate) fn avatar_timings() -> AvatarTimings {
    TEST_TIMINGS.with(Cell::get).unwrap_or(AVATAR_TIMINGS)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AvatarAction {
    Upload,
    Gravatar,
    Remove,
}

#[derive(Clone, PartialEq, Debug, Default)]
pub enum AvatarView {
    #[default]
    Default,
    Loading,
    Image(String),
}

/// The user id a tile may look an avatar up for, or `None` when it must not.
pub fn avatar_lookup_id<'a>(session_id: &str, user_id: Option<&'a str>) -> Option<&'a str> {
    if session_id.starts_with("mock-") {
        return None;
    }
    let user_id = user_id?;
    if matches!(user_id, "" | "." | "..") || user_id.starts_with(GUEST_USER_ID_PREFIX) {
        return None;
    }
    Some(user_id)
}

/// The MIME type a served avatar may be wrapped in, or `None` to refuse it.
pub fn displayable_avatar_type(content_type: &str) -> Option<&'static str> {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    DISPLAYABLE_AVATAR_TYPES
        .iter()
        .copied()
        .find(|allowed| *allowed == essence)
}

pub fn should_retry_peer_avatar(attempt: u8, failed: bool) -> bool {
    failed && attempt < MAX_PEER_AVATAR_ATTEMPTS
}

/// `base` moved by up to `jitter` either way; `unit` in `[0, 1]` picks the point.
pub fn jittered_delay_ms(base: u32, jitter: u32, unit: f64) -> u32 {
    let low = base.saturating_sub(jitter);
    let span = base.saturating_add(jitter) - low;
    low + (f64::from(span) * unit.clamp(0.0, 1.0)).round() as u32
}

fn image_fingerprint(image: &AvatarBytes) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    image.content_type.hash(&mut hasher);
    image.bytes.hash(&mut hasher);
    hasher.finish()
}

pub(crate) fn bytes_blob(bytes: &[u8], mime: &'static str) -> Option<web_sys::Blob> {
    let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(bytes));
    let options = web_sys::BlobPropertyBag::new();
    options.set_type(mime);
    web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &options).ok()
}

/// A Blob typed with the response's image type, or `None` for any other type.
pub fn avatar_blob(image: &AvatarBytes) -> Option<web_sys::Blob> {
    bytes_blob(&image.bytes, displayable_avatar_type(&image.content_type)?)
}

pub(crate) fn object_url_for(image: &AvatarBytes) -> Option<String> {
    avatar_blob(image).and_then(|blob| web_sys::Url::create_object_url_with_blob(&blob).ok())
}

pub(crate) fn retire_object_url(url: String) {
    wasm_bindgen_futures::spawn_local(async move {
        TimeoutFuture::new(RETIRED_URL_REVOKE_DELAY_MS).await;
        let _ = web_sys::Url::revoke_object_url(&url);
    });
}

/// The avatar circle, or the default glyph when there is no usable image.
#[component]
pub fn AvatarImage(
    view: AvatarView,
    #[props(default = "peer-avatar-img")] testid: &'static str,
    #[props(default = "")] extra_class: &'static str,
    children: Element,
) -> Element {
    let mut failed = use_signal(|| None::<String>);
    let src = match &view {
        AvatarView::Image(url) if failed.read().as_deref() != Some(url.as_str()) => {
            Some(url.clone())
        }
        _ => None,
    };
    let state = match (&src, &view) {
        (Some(_), _) => "image",
        (None, AvatarView::Loading) => "loading",
        _ => "default",
    };
    let class = if extra_class.is_empty() {
        "peer-avatar-slot".to_string()
    } else {
        format!("peer-avatar-slot {extra_class}")
    };
    rsx! {
        span { class: "{class}", "data-avatar-state": state,
            if let Some(src) = src {
                img {
                    class: "peer-avatar",
                    src: "{src}",
                    alt: "",
                    draggable: "false",
                    "decoding": "async",
                    "data-testid": testid,
                    onerror: {
                        let src = src.clone();
                        move |_| failed.set(Some(src.clone()))
                    },
                }
            } else {
                PeerIcon {}
            }
            {children}
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub enum PeerAvatarState {
    Loading,
    None,
    Image(String),
    Failed,
}

impl PeerAvatarState {
    fn view(&self) -> AvatarView {
        match self {
            PeerAvatarState::Loading => AvatarView::Loading,
            PeerAvatarState::Image(url) => AvatarView::Image(url.clone()),
            PeerAvatarState::None | PeerAvatarState::Failed => AvatarView::Default,
        }
    }

    fn is_set(&self) -> Option<bool> {
        match self {
            PeerAvatarState::Image(_) => Some(true),
            PeerAvatarState::None => Some(false),
            PeerAvatarState::Loading | PeerAvatarState::Failed => None,
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct AvatarCacheConfig {
    pub meeting_id: String,
    pub room_token: String,
    pub is_guest: bool,
    pub own_user_id: Option<String>,
}

type RefreshFuture = Shared<LocalBoxFuture<'static, Option<String>>>;

/// One user's avatar, shared by the tiles of all of that user's sessions.
struct UserSlot {
    id: u64,
    state: Signal<PeerAvatarState>,
    fingerprint: Option<u64>,
    writes: u64,
    fetching: bool,
    queued: bool,
    stale: bool,
    mounted: usize,
    idle: u64,
}

/// A mounted tile's claim on a user's slot: (cache epoch, user id).
type HeldUser = (u64, String);

struct CacheInner {
    owner: ScopeId,
    alive: Cell<bool>,
    epoch: Cell<u64>,
    next_slot: Cell<u64>,
    meeting_id: RefCell<String>,
    is_guest: Cell<bool>,
    own_user_id: RefCell<Option<String>>,
    prop_token: RefCell<String>,
    room_token: RefCell<String>,
    token_gen: Cell<u64>,
    users: RefCell<HashMap<String, UserSlot>>,
    sessions: RefCell<HashMap<String, String>>,
    urls: RefCell<HashSet<String>>,
    refreshing: RefCell<Option<RefreshFuture>>,
    in_flight: Cell<usize>,
    waiting: RefCell<VecDeque<oneshot::Sender<()>>>,
    own_hold: RefCell<Option<HeldUser>>,
    own_avatar: Cell<Option<OwnAvatarCtx>>,
    account_named: Cell<bool>,
    other_account: Signal<Option<(u64, String)>>,
    other_account_notices: Signal<u32>,
}

impl CacheInner {
    fn live(cache: &Weak<CacheInner>) -> Option<Rc<CacheInner>> {
        cache.upgrade().filter(|inner| inner.alive.get())
    }

    fn create_url(&self, image: &AvatarBytes) -> Option<String> {
        if !self.alive.get() {
            return None;
        }
        let url = object_url_for(image)?;
        self.urls.borrow_mut().insert(url.clone());
        Some(url)
    }

    fn retire(&self, state: &PeerAvatarState) {
        if let PeerAvatarState::Image(url) = state {
            if self.urls.borrow_mut().remove(url) {
                retire_object_url(url.clone());
            }
        }
    }

    fn slot_writes(&self, user_id: &str, slot_id: u64) -> Option<u64> {
        let users = self.users.borrow();
        users
            .get(user_id)
            .filter(|slot| slot.id == slot_id)
            .map(|slot| slot.writes)
    }

    fn capture_writes(&self, user_id: &str, slot_id: u64, dequeue: bool) -> Option<u64> {
        let mut users = self.users.borrow_mut();
        let slot = users.get_mut(user_id).filter(|slot| slot.id == slot_id)?;
        if dequeue {
            slot.queued = false;
        }
        Some(slot.writes)
    }

    fn write_state(&self, mut state: Signal<PeerAvatarState>, next: PeerAvatarState) {
        match state.try_write() {
            Ok(mut current) => {
                let old = std::mem::replace(&mut *current, next);
                drop(current);
                self.retire(&old);
            }
            Err(_) => self.retire(&next),
        }
    }

    /// Turn a `Failed` slot back into `Loading`; `false` when it holds anything else.
    fn reload_failed(&self, user_id: &str, slot_id: u64) -> bool {
        let Some(mut state) = self
            .users
            .borrow()
            .get(user_id)
            .filter(|slot| slot.id == slot_id)
            .map(|slot| slot.state)
        else {
            return false;
        };
        if !state
            .try_peek()
            .is_ok_and(|current| *current == PeerAvatarState::Failed)
        {
            return false;
        }
        let reloaded = match state.try_write() {
            Ok(mut current) => {
                *current = PeerAvatarState::Loading;
                true
            }
            Err(_) => false,
        };
        reloaded
    }

    /// Store one lookup's result; `true` when it failed and may be retried. A failed `confirm`
    /// lookup keeps the image the slot shows and may be retried too.
    fn store_result(
        &self,
        user_id: &str,
        slot_id: u64,
        writes: u64,
        result: LookupResult,
        confirm: bool,
    ) -> bool {
        let mut users = self.users.borrow_mut();
        let Some(slot) = users
            .get_mut(user_id)
            .filter(|slot| slot.id == slot_id && slot.writes == writes)
        else {
            return false;
        };
        let showing_image = slot
            .state
            .try_peek()
            .is_ok_and(|state| matches!(*state, PeerAvatarState::Image(_)));
        let resolved = showing_image
            || slot
                .state
                .try_peek()
                .is_ok_and(|state| *state == PeerAvatarState::None);
        let next = match result {
            Ok(Some(image)) => {
                let fingerprint = image_fingerprint(&image);
                if slot.fingerprint == Some(fingerprint) && showing_image {
                    return false;
                }
                let next = self
                    .create_url(&image)
                    .map_or(PeerAvatarState::None, PeerAvatarState::Image);
                slot.fingerprint = matches!(next, PeerAvatarState::Image(_)).then_some(fingerprint);
                next
            }
            Ok(None) => {
                slot.fingerprint = None;
                PeerAvatarState::None
            }
            Err(failure) => {
                log::warn!(
                    "avatar: a peer avatar lookup failed ({})",
                    failure.log_kind()
                );
                if resolved {
                    return confirm;
                }
                PeerAvatarState::Failed
            }
        };
        let failed = next == PeerAvatarState::Failed;
        slot.writes += 1;
        let state = slot.state;
        drop(users);
        self.write_state(state, next);
        failed
    }

    fn finish_fetch(&self, user_id: &str, slot_id: u64) {
        if let Ok(mut users) = self.users.try_borrow_mut() {
            if let Some(slot) = users.get_mut(user_id).filter(|slot| slot.id == slot_id) {
                slot.fetching = false;
            }
        }
    }

    fn prune(&self, user_id: &str, slot_id: u64, idle: u64) {
        let slot = {
            let mut users = self.users.borrow_mut();
            let unused = users
                .get(user_id)
                .is_some_and(|slot| slot.id == slot_id && slot.mounted == 0 && slot.idle == idle);
            if !unused {
                return;
            }
            users.remove(user_id)
        };
        let Some(slot) = slot else {
            return;
        };
        self.sessions.borrow_mut().retain(|_, user| user != user_id);
        if let Ok(state) = slot.state.try_peek() {
            self.retire(&state);
        }
        slot.state.manually_drop();
    }
}

/// One of the [`MAX_AVATAR_FETCHES_IN_FLIGHT`] lookups allowed on the network at once.
struct FetchPermit(Weak<CacheInner>);

impl FetchPermit {
    async fn acquire(cache: &Weak<CacheInner>) -> Option<Self> {
        let turn = {
            let inner = CacheInner::live(cache)?;
            if inner.in_flight.get() < MAX_AVATAR_FETCHES_IN_FLIGHT {
                inner.in_flight.set(inner.in_flight.get() + 1);
                return Some(Self(cache.clone()));
            }
            let (handoff, turn) = oneshot::channel();
            inner.waiting.borrow_mut().push_back(handoff);
            turn
        };
        turn.await.ok()?;
        let permit = Self(cache.clone());
        CacheInner::live(cache)?;
        Some(permit)
    }
}

impl Drop for FetchPermit {
    fn drop(&mut self) {
        let Some(inner) = self.0.upgrade() else {
            return;
        };
        loop {
            let next = inner.waiting.borrow_mut().pop_front();
            let Some(next) = next else {
                inner.in_flight.set(inner.in_flight.get().saturating_sub(1));
                return;
            };
            if next.send(()).is_ok() {
                return;
            }
        }
    }
}

struct FetchTask {
    cache: Weak<CacheInner>,
    user_id: String,
    slot_id: u64,
    confirm: bool,
    queued: bool,
    delay_ms: u32,
}

impl Drop for FetchTask {
    fn drop(&mut self) {
        if let Some(inner) = self.cache.upgrade() {
            inner.finish_fetch(&self.user_id, self.slot_id);
        }
    }
}

/// Avatars of the peers in one meeting: one image per user, shared by that user's sessions.
#[derive(Clone)]
pub struct PeerAvatarCacheCtx(Rc<CacheInner>);

impl PeerAvatarCacheCtx {
    fn new(config: &AvatarCacheConfig, owner: ScopeId) -> Self {
        Self(Rc::new(CacheInner {
            owner,
            alive: Cell::new(true),
            epoch: Cell::new(0),
            next_slot: Cell::new(0),
            meeting_id: RefCell::new(config.meeting_id.clone()),
            is_guest: Cell::new(config.is_guest),
            own_user_id: RefCell::new(config.own_user_id.clone()),
            prop_token: RefCell::new(config.room_token.clone()),
            room_token: RefCell::new(config.room_token.clone()),
            token_gen: Cell::new(0),
            users: RefCell::default(),
            sessions: RefCell::default(),
            urls: RefCell::default(),
            refreshing: RefCell::new(None),
            in_flight: Cell::new(0),
            waiting: RefCell::default(),
            own_hold: RefCell::new(None),
            own_avatar: Cell::new(None),
            account_named: Cell::new(false),
            other_account: Signal::new_in_scope(None, owner),
            other_account_notices: Signal::new_in_scope(0, owner),
        }))
    }

    pub fn is_guest(&self) -> bool {
        self.0.is_guest.get()
    }

    pub fn own_user_id(&self) -> Option<String> {
        self.0.own_user_id.borrow().clone()
    }

    /// The signed-in user this tab is in the meeting as, when known.
    pub fn meeting_user_id(&self) -> Option<String> {
        self.own_user_id()
            .filter(|id| !id.is_empty() && !self.is_guest())
    }

    fn identity_key(&self) -> Option<(u64, String)> {
        Some((self.0.epoch.get(), self.meeting_user_id()?))
    }

    fn seen_for_this_meeting(&self, seen: &Option<(u64, String)>) -> bool {
        let key = self.identity_key();
        key.is_some() && *seen == key
    }

    /// `true` once the browser's account was seen to be someone other than the meeting user.
    pub fn signed_in_elsewhere(&self) -> bool {
        self.0
            .other_account
            .try_read()
            .is_ok_and(|seen| self.seen_for_this_meeting(&seen))
    }

    /// Changes each time the signed-in-elsewhere notice is to be read out again.
    pub fn signed_in_elsewhere_notices(&self) -> u32 {
        self.0
            .other_account_notices
            .try_read()
            .map_or(0, |count| *count)
    }

    pub(crate) fn renotify_signed_in_elsewhere(&self) {
        let mut notices = self.0.other_account_notices;
        if let Ok(mut count) = notices.try_write() {
            *count += 1;
        };
    }

    pub(crate) fn set_signed_in_elsewhere(&self, other: bool) {
        let next = self.identity_key().filter(|_| other);
        let mut seen = self.0.other_account;
        if seen.try_peek().is_ok_and(|current| *current != next) {
            if let Ok(mut current) = seen.try_write() {
                *current = next;
            }
        }
    }

    /// Record which account the browser is signed in to (`account`, empty when the server does
    /// not say); `true` when it is not the meeting user.
    pub(crate) fn note_account(&self, account: &str) -> bool {
        if account.is_empty() {
            return false;
        }
        self.0.account_named.set(true);
        let Some(meeting_user) = self.meeting_user_id() else {
            return false;
        };
        let other = meeting_user != account;
        self.set_signed_in_elsewhere(other);
        other
    }

    /// `true` once a read of the browser's account has returned its user id.
    pub(crate) fn server_named_account(&self) -> bool {
        self.0.account_named.get()
    }

    fn sync_props(&self, config: &AvatarCacheConfig) {
        let inner = &self.0;
        let new_meeting = *inner.meeting_id.borrow() != config.meeting_id;
        if new_meeting {
            self.reset(&config.meeting_id);
        }
        let new_token = *inner.prop_token.borrow() != config.room_token;
        if new_token {
            inner.prop_token.replace(config.room_token.clone());
            inner.room_token.replace(config.room_token.clone());
            inner.token_gen.set(inner.token_gen.get() + 1);
        }
        inner.is_guest.set(config.is_guest);
        let new_user = inner.own_user_id.borrow().as_deref() != config.own_user_id.as_deref();
        if new_user {
            inner.own_user_id.replace(config.own_user_id.clone());
        }
    }

    /// Drop every avatar of the previous meeting.
    fn reset(&self, meeting_id: &str) {
        let inner = &self.0;
        inner.epoch.set(inner.epoch.get() + 1);
        inner.account_named.set(false);
        inner.meeting_id.replace(meeting_id.to_string());
        inner.refreshing.replace(None);
        inner.room_token.replace(inner.prop_token.borrow().clone());
        inner.token_gen.set(inner.token_gen.get() + 1);
        inner.sessions.borrow_mut().clear();
        let slots: Vec<UserSlot> = inner
            .users
            .borrow_mut()
            .drain()
            .map(|(_, slot)| slot)
            .collect();
        for slot in slots {
            inner.write_state(slot.state, PeerAvatarState::None);
            slot.state.manually_drop();
        }
    }

    fn new_slot(&self) -> UserSlot {
        let id = self.0.next_slot.get();
        self.0.next_slot.set(id + 1);
        UserSlot {
            id,
            state: Signal::new_in_scope(PeerAvatarState::Loading, self.0.owner),
            fingerprint: None,
            writes: 0,
            fetching: false,
            queued: false,
            stale: false,
            mounted: 0,
            idle: 0,
        }
    }

    fn lookup_user(&self, session_id: &str, user_id: Option<&str>) -> Option<String> {
        if !self.0.alive.get() {
            return None;
        }
        match user_id {
            None => self.0.sessions.borrow().get(session_id).cloned(),
            Some(_) => avatar_lookup_id(session_id, user_id).map(str::to_string),
        }
    }

    /// The state a tile of `session_id` shows; starts a lookup when one is due.
    fn slot_for(
        &self,
        session_id: &str,
        user_id: Option<&str>,
        held: &RefCell<Option<HeldUser>>,
    ) -> Option<Signal<PeerAvatarState>> {
        let inner = &self.0;
        let user = self.lookup_user(session_id, user_id);
        let new_session = user.as_ref().is_some_and(|user| {
            let new = inner.sessions.borrow().get(session_id) != Some(user);
            if new {
                inner
                    .sessions
                    .borrow_mut()
                    .insert(session_id.to_string(), user.clone());
            }
            new
        });
        self.claim(user, new_session, held)
    }

    /// The meeting user's own slot, read with the room token like any participant's; the cache
    /// holds it too once claimed.
    fn own_slot(&self, held: &RefCell<Option<HeldUser>>) -> Option<Signal<PeerAvatarState>> {
        let user = self.meeting_user_id().filter(|_| self.0.alive.get());
        self.claim(user.clone(), false, &self.0.own_hold);
        self.claim(user, false, held)
    }

    fn claim(
        &self,
        user: Option<String>,
        new_session: bool,
        held: &RefCell<Option<HeldUser>>,
    ) -> Option<Signal<PeerAvatarState>> {
        let inner = &self.0;
        let claim = user.as_ref().map(|user| (inner.epoch.get(), user.clone()));
        let claimed = *held.borrow() == claim;
        if !claimed {
            if let Some(old) = held.replace(claim) {
                self.release(old);
            }
        }
        let user = user?;
        let (state, lookup, stale) = {
            let mut users = inner.users.borrow_mut();
            let created = !users.contains_key(&user);
            let slot = users.entry(user.clone()).or_insert_with(|| self.new_slot());
            if !claimed {
                slot.mounted += 1;
            }
            let stale = std::mem::take(&mut slot.stale);
            if stale {
                slot.writes += 1;
                slot.queued = true;
            }
            let due = created || new_session;
            let lookup = (stale || (due && !slot.fetching)).then_some(slot.id);
            if lookup.is_some() {
                slot.fetching = true;
            }
            (slot.state, lookup, stale)
        };
        if let Some(slot_id) = lookup {
            inner.reload_failed(&user, slot_id);
            wasm_bindgen_futures::spawn_local(fetch_peer_avatar(FetchTask {
                cache: Rc::downgrade(inner),
                user_id: user,
                slot_id,
                confirm: stale,
                queued: stale,
                delay_ms: 0,
            }));
        }
        Some(state)
    }

    fn release(&self, (epoch, user_id): HeldUser) {
        let inner = &self.0;
        if !inner.alive.get() || inner.epoch.get() != epoch {
            return;
        }
        let (slot_id, idle) = {
            let mut users = inner.users.borrow_mut();
            let Some(slot) = users.get_mut(&user_id) else {
                return;
            };
            slot.mounted = slot.mounted.saturating_sub(1);
            if slot.mounted > 0 {
                return;
            }
            slot.idle += 1;
            (slot.id, slot.idle)
        };
        let cache = Rc::downgrade(inner);
        wasm_bindgen_futures::spawn_local(async move {
            TimeoutFuture::new(avatar_timings().prune_grace_ms).await;
            if let Some(inner) = CacheInner::live(&cache) {
                inner.prune(&user_id, slot_id, idle);
            }
        });
    }

    /// Show `image` (or the default icon for `None`) on every tile of `user_id`.
    pub fn set_user_image(&self, user_id: &str, image: Option<&AvatarBytes>) {
        let inner = &self.0;
        if !inner.alive.get() {
            return;
        }
        let mut users = inner.users.borrow_mut();
        let Some(slot) = users.get_mut(user_id) else {
            return;
        };
        let next = image
            .and_then(|image| inner.create_url(image))
            .map_or(PeerAvatarState::None, PeerAvatarState::Image);
        slot.fingerprint = image
            .filter(|_| matches!(next, PeerAvatarState::Image(_)))
            .map(image_fingerprint);
        slot.writes += 1;
        let state = slot.state;
        drop(users);
        inner.write_state(state, next);
    }

    pub(crate) fn reload_meeting_user_if_failed(&self) {
        let inner = &self.0;
        let Some(user_id) = self.meeting_user_id().filter(|_| inner.alive.get()) else {
            return;
        };
        let slot_id = inner.users.borrow().get(&user_id).map(|slot| slot.id);
        if slot_id.is_some_and(|slot_id| inner.reload_failed(&user_id, slot_id)) {
            self.reload_meeting_user();
        }
    }

    pub(crate) fn reload_meeting_user(&self) {
        if let Some(user_id) = self.meeting_user_id() {
            self.refresh_slot(&user_id, false);
        }
    }

    /// Read `user_id`'s avatar again now that it changed; the meeting user's own profile too.
    pub fn refresh_user(&self, user_id: &str) {
        let own = self.meeting_user_id().as_deref() == Some(user_id);
        self.refresh_slot(user_id, !own);
        if let Some(own_avatar) = self
            .0
            .own_avatar
            .get()
            .filter(|_| own && self.0.alive.get())
        {
            let cache = self.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let _ = recheck_own_avatar(own_avatar, Some(cache)).await;
            });
        }
    }

    fn refresh_slot(&self, user_id: &str, jitter: bool) {
        let inner = &self.0;
        if !inner.alive.get() {
            return;
        }
        let slot_id = {
            let mut users = inner.users.borrow_mut();
            let Some(slot) = users.get_mut(user_id).filter(|slot| !slot.queued) else {
                return;
            };
            if slot.mounted == 0 {
                slot.stale = true;
                return;
            }
            slot.writes += 1;
            slot.queued = true;
            slot.fetching = true;
            slot.id
        };
        let delay_ms = if jitter {
            jittered_delay_ms(
                0,
                avatar_timings().refresh_jitter_ms,
                js_sys::Math::random(),
            )
        } else {
            0
        };
        wasm_bindgen_futures::spawn_local(fetch_peer_avatar(FetchTask {
            cache: Rc::downgrade(inner),
            user_id: user_id.to_string(),
            slot_id,
            confirm: true,
            queued: true,
            delay_ms,
        }));
    }

    fn teardown(&self) {
        let inner = &self.0;
        inner.alive.set(false);
        inner.users.borrow_mut().clear();
        inner.sessions.borrow_mut().clear();
        inner.waiting.borrow_mut().clear();
        inner.refreshing.replace(None);
        for url in inner.urls.borrow_mut().drain() {
            let _ = web_sys::Url::revoke_object_url(&url);
        }
    }
}

/// One lookup with the current room token, and the token generation it was sent with.
async fn fetch_once(cache: &Weak<CacheInner>, user_id: &str) -> Option<(u64, LookupResult)> {
    let (meeting_id, room_token, token_gen) = {
        let inner = CacheInner::live(cache)?;
        let meeting_id = inner.meeting_id.borrow().clone();
        let room_token = inner.room_token.borrow().clone();
        (meeting_id, room_token, inner.token_gen.get())
    };
    let base_url = match crate::constants::meeting_api_base_url() {
        Ok(url) => url,
        Err(e) => {
            return Some((token_gen, Err(LookupFailure::Api(ApiError::Config(e)))));
        }
    };
    let client = MeetingApiClient::new(&base_url, AuthMode::Bearer(room_token));
    let lookup = std::pin::pin!(client.get_meeting_avatar_image(&meeting_id, user_id));
    let deadline = TimeoutFuture::new(avatar_timings().lookup_timeout_ms);
    let result = match select(lookup, deadline).await {
        Either::Left((result, _)) => result.map_err(LookupFailure::Api),
        Either::Right(_) => Err(LookupFailure::TimedOut),
    };
    Some((token_gen, result))
}

async fn refresh_room_token_for(
    cache: Weak<CacheInner>,
    meeting_id: String,
    epoch: u64,
) -> Option<String> {
    let token = crate::meeting_api::refresh_room_token(&meeting_id)
        .await
        .ok();
    let inner = CacheInner::live(&cache).filter(|inner| inner.epoch.get() == epoch)?;
    inner.refreshing.replace(None);
    let token = token?;
    inner.room_token.replace(token.clone());
    inner.token_gen.set(inner.token_gen.get() + 1);
    Some(token)
}

async fn refreshed_room_token(cache: &Weak<CacheInner>) -> Option<String> {
    let shared = {
        let inner = CacheInner::live(cache)?;
        if inner.is_guest.get() {
            return None;
        }
        let mut slot = inner.refreshing.borrow_mut();
        match slot.as_ref() {
            Some(in_flight) => in_flight.clone(),
            None => {
                let meeting_id = inner.meeting_id.borrow().clone();
                let fresh = refresh_room_token_for(cache.clone(), meeting_id, inner.epoch.get())
                    .boxed_local()
                    .shared();
                *slot = Some(fresh.clone());
                fresh
            }
        }
    };
    shared.await
}

/// Whether a lookup refused with 401 may be sent once more: with a newer token, or after a
/// refresh that answers within the lookup deadline.
async fn may_resend(cache: &Weak<CacheInner>, sent_with: u64, resent: &mut bool) -> bool {
    if std::mem::replace(resent, true) {
        return false;
    }
    let newer = CacheInner::live(cache).is_some_and(|inner| inner.token_gen.get() != sent_with);
    if newer {
        return true;
    }
    let refresh = std::pin::pin!(refreshed_room_token(cache));
    let deadline = TimeoutFuture::new(avatar_timings().lookup_timeout_ms);
    matches!(select(refresh, deadline).await, Either::Left((Some(_), _)))
}

async fn fetch_peer_avatar(task: FetchTask) {
    let FetchTask {
        cache,
        user_id,
        slot_id,
        confirm,
        queued,
        delay_ms,
    } = &task;
    if *delay_ms > 0 {
        TimeoutFuture::new(*delay_ms).await;
    }
    let mut attempt: u8 = 1;
    let mut resent = false;
    let mut dequeue = *queued;
    loop {
        let Some(permit) = FetchPermit::acquire(cache).await else {
            return;
        };
        let Some(writes) = CacheInner::live(cache).and_then(|inner| {
            inner.capture_writes(user_id, *slot_id, std::mem::take(&mut dequeue))
        }) else {
            return;
        };
        let Some((sent_with, mut result)) = fetch_once(cache, user_id).await else {
            return;
        };
        if matches!(result, Err(LookupFailure::Api(ApiError::NotAuthenticated)))
            && may_resend(cache, sent_with, &mut resent).await
        {
            let Some((_, retried)) = fetch_once(cache, user_id).await else {
                return;
            };
            result = retried;
        }
        drop(permit);
        let Some(inner) = CacheInner::live(cache) else {
            return;
        };
        let failed = inner.store_result(user_id, *slot_id, writes, result, *confirm);
        drop(inner);
        if !should_retry_peer_avatar(attempt, failed) {
            return;
        }
        let timings = avatar_timings();
        let delay = jittered_delay_ms(
            timings.retry_ms,
            timings.retry_jitter_ms,
            js_sys::Math::random(),
        );
        TimeoutFuture::new(delay).await;
        attempt += 1;
        let again = CacheInner::live(cache).is_some_and(|inner| {
            (*confirm && inner.slot_writes(user_id, *slot_id) == Some(writes))
                || inner.reload_failed(user_id, *slot_id)
        });
        if !again {
            return;
        }
    }
}

/// Provide the peer avatar cache to `children`; revokes every blob URL on unmount.
pub fn use_provide_peer_avatar_cache(config: AvatarCacheConfig) -> PeerAvatarCacheCtx {
    let cache = use_hook(|| PeerAvatarCacheCtx::new(&config, dioxus::core::current_scope_id()));
    use_context_provider(|| cache.clone());
    cache.sync_props(&config);
    let is_guest = config.is_guest;
    let own = try_use_context::<OwnAvatarCtx>();
    cache.0.own_avatar.set(own);
    use_effect({
        let cache = cache.clone();
        move || {
            if let Some(own) = own.filter(|_| !is_guest) {
                refresh_own_avatar(own, Some(cache.clone()));
            }
        }
    });
    use_drop({
        let cache = cache.clone();
        move || cache.teardown()
    });
    cache
}

#[component]
pub fn AvatarCacheProvider(
    meeting_id: String,
    room_token: String,
    #[props(default)] is_guest: bool,
    #[props(default)] own_user_id: Option<String>,
    children: Element,
) -> Element {
    use_provide_peer_avatar_cache(AvatarCacheConfig {
        meeting_id,
        room_token,
        is_guest,
        own_user_id,
    });
    children
}

/// The call client's `on_avatar_changed` handler: [`PeerAvatarCacheCtx::refresh_user`] on
/// `cache`, which it holds weakly.
pub fn avatar_change_listener(
    cache: Option<&PeerAvatarCacheCtx>,
) -> Option<videocall_client::Callback<String>> {
    let cache = Rc::downgrade(&cache?.0);
    Some(videocall_client::Callback::from(move |user_id: String| {
        if let Some(inner) = CacheInner::live(&cache) {
            PeerAvatarCacheCtx(inner).refresh_user(&user_id);
        }
    }))
}

/// The holder of a component's claim on one cache slot, released when the component unmounts.
fn use_slot_claim(cache: Option<PeerAvatarCacheCtx>) -> Rc<RefCell<Option<HeldUser>>> {
    let held = use_hook(|| Rc::new(RefCell::new(None::<HeldUser>)));
    use_drop({
        let held = held.clone();
        move || {
            let claim = held.borrow_mut().take();
            if let (Some(cache), Some(claim)) = (cache, claim) {
                cache.release(claim);
            }
        }
    });
    held
}

/// A peer tile's avatar; reads only this user's cache slot.
#[component]
pub fn PeerAvatar(session_id: String, user_id: Option<String>, children: Element) -> Element {
    let cache = try_use_context::<PeerAvatarCacheCtx>();
    let held = use_slot_claim(cache.clone());
    let state = cache
        .as_ref()
        .and_then(|cache| cache.slot_for(&session_id, user_id.as_deref(), &held));
    let view = state
        .and_then(|state| state.try_read().ok().map(|state| state.view()))
        .unwrap_or_default();
    rsx! {
        AvatarImage { view, {children} }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum OwnAvatarLoad {
    #[default]
    NotLoaded,
    Loading,
    Loaded,
    Failed,
}

#[derive(Clone, PartialEq, Debug, Default)]
pub struct OwnAvatar {
    pub load: OwnAvatarLoad,
    pub info: Option<AvatarInfo>,
    pub image_url: Option<String>,
    pub gravatar_enabled: bool,
    pub refreshing: bool,
    pub busy: Option<AvatarAction>,
}

impl OwnAvatar {
    pub fn view(&self) -> AvatarView {
        match (&self.image_url, self.load) {
            (Some(url), _) => AvatarView::Image(url.clone()),
            (None, OwnAvatarLoad::Loading) => AvatarView::Loading,
            _ => AvatarView::Default,
        }
    }
}

#[derive(Clone, Copy)]
pub struct OwnAvatarCtx(pub Signal<OwnAvatar>);

pub(crate) fn user_client() -> Result<MeetingApiClient, ApiError> {
    crate::constants::meeting_api_client().map_err(ApiError::Config)
}

pub(crate) async fn with_user_refresh<T, F, Fut>(op: F) -> Result<T, ApiError>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, ApiError>>,
{
    match op().await {
        Err(ApiError::NotAuthenticated) if crate::constants::is_pkce_flow() => {
            if crate::meeting_api::refresh_token_single_flight()
                .await
                .is_ok()
            {
                op().await
            } else {
                Err(ApiError::NotAuthenticated)
            }
        }
        other => other,
    }
}

pub(crate) async fn fetch_my_avatar_image() -> Result<Option<AvatarBytes>, ApiError> {
    with_user_refresh(|| async { user_client()?.get_my_avatar_image().await }).await
}

pub(crate) fn set_own_avatar(
    mut own: Signal<OwnAvatar>,
    info: Option<AvatarInfo>,
    image: Option<&AvatarBytes>,
) {
    let url = image.and_then(object_url_for);
    let Ok(mut current) = own.try_write() else {
        if let Some(url) = url {
            let _ = web_sys::Url::revoke_object_url(&url);
        }
        return;
    };
    current.load = OwnAvatarLoad::Loaded;
    current.info = info;
    let old = std::mem::replace(&mut current.image_url, url);
    drop(current);
    if let Some(old) = old {
        retire_object_url(old);
    }
}

/// What a save changes in the own avatar: its image URL and its etag.
type OwnVersion = (Option<String>, Option<String>);

impl OwnAvatar {
    fn version(&self) -> OwnVersion {
        (
            self.image_url.clone(),
            self.info.as_ref().map(|info| info.etag.clone()),
        )
    }
}

fn begin_own_refresh(ctx: OwnAvatarCtx) -> Option<OwnVersion> {
    let mut own = ctx.0;
    let started = match own.try_write() {
        Ok(mut current) if !current.refreshing => {
            current.refreshing = true;
            if current.load != OwnAvatarLoad::Loaded {
                current.load = OwnAvatarLoad::Loading;
            }
            Some(current.version())
        }
        _ => None,
    };
    started
}

/// Reload the own avatar from the server; in `meeting`, only if it is the meeting user's.
pub fn refresh_own_avatar(ctx: OwnAvatarCtx, meeting: Option<PeerAvatarCacheCtx>) {
    if let Some(started_with) = begin_own_refresh(ctx) {
        wasm_bindgen_futures::spawn_local(async move {
            let _ = finish_own_refresh(ctx, started_with, meeting, false).await;
        });
    }
}

/// Reload the own avatar from the server and show it on this user's meeting tiles too; as
/// [`reread_own_avatar`].
pub(crate) async fn resync_own_avatar(
    ctx: OwnAvatarCtx,
    cache: Option<PeerAvatarCacheCtx>,
) -> Option<Option<AvatarInfo>> {
    reread_own_avatar(ctx, cache, true).await.ok()
}

/// Check the own avatar again: the meeting user's image is looked up again if that failed, and
/// the server is read as [`reread_own_avatar`].
pub(crate) async fn recheck_own_avatar(
    ctx: OwnAvatarCtx,
    cache: Option<PeerAvatarCacheCtx>,
) -> OwnRead {
    if let Some(cache) = &cache {
        cache.reload_meeting_user_if_failed();
    }
    reread_own_avatar(ctx, cache, false).await
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum OwnReadFailure {
    Unreadable,
    OtherAccount,
}

/// The server's avatar for the browser's account, or why it was not taken.
pub(crate) type OwnRead = Result<Option<AvatarInfo>, OwnReadFailure>;

/// Read the own avatar from the server once a refresh already running has finished, or after
/// `own_refresh_wait_ms` either way.
async fn reread_own_avatar(
    ctx: OwnAvatarCtx,
    cache: Option<PeerAvatarCacheCtx>,
    paint_tiles: bool,
) -> OwnRead {
    let mut deadline = TimeoutFuture::new(avatar_timings().own_refresh_wait_ms);
    let started_with = loop {
        if let Some(started_with) = begin_own_refresh(ctx) {
            break started_with;
        }
        let Ok(current) = ctx.0.try_peek().map(|current| current.version()) else {
            return Err(OwnReadFailure::Unreadable);
        };
        let (finished_tx, finished) = oneshot::channel();
        OWN_REFRESH_WAITERS.with(|waiters| waiters.borrow_mut().push(finished_tx));
        if let Either::Right(_) = select(finished, &mut deadline).await {
            break current;
        }
    };
    finish_own_refresh(ctx, started_with, cache, paint_tiles).await
}

async fn finish_own_refresh(
    ctx: OwnAvatarCtx,
    started_with: OwnVersion,
    meeting: Option<PeerAvatarCacheCtx>,
    paint_tiles: bool,
) -> OwnRead {
    let server = read_own_avatar(ctx, started_with, meeting, paint_tiles).await;
    for waiter in OWN_REFRESH_WAITERS.with(|waiters| std::mem::take(&mut *waiters.borrow_mut())) {
        let _ = waiter.send(());
    }
    server
}

async fn read_own_avatar(
    ctx: OwnAvatarCtx,
    started_with: OwnVersion,
    meeting: Option<PeerAvatarCacheCtx>,
    paint_tiles: bool,
) -> OwnRead {
    let mut own = ctx.0;
    let read = std::pin::pin!(with_user_refresh(|| async {
        user_client()?.get_my_avatar().await
    }));
    let deadline = TimeoutFuture::new(avatar_timings().lookup_timeout_ms);
    let meta = match select(read, deadline).await {
        Either::Left((meta, _)) => meta.map_err(|e| api_error_kind(&e)),
        Either::Right(_) => Err("timed out".to_string()),
    };
    let saved_meanwhile = |own: Signal<OwnAvatar>| {
        own.try_peek()
            .map_or(true, |current| current.version() != started_with)
    };
    let fail = |mut own: Signal<OwnAvatar>| {
        if let Ok(mut current) = own.try_write() {
            current.refreshing = false;
            if current.load != OwnAvatarLoad::Loaded {
                current.load = OwnAvatarLoad::Failed;
            }
        }
    };
    let response = match meta {
        Ok(response) => response,
        Err(kind) => {
            log::warn!("avatar: loading your avatar failed ({kind})");
            fail(own);
            return Err(OwnReadFailure::Unreadable);
        }
    };
    if meeting
        .as_ref()
        .is_some_and(|meeting| meeting.note_account(&response.user_id))
    {
        log::warn!("avatar: the browser is signed in to another account; not showing its avatar");
        fail(own);
        return Err(OwnReadFailure::OtherAccount);
    }
    let server = response.avatar.clone();
    if let Ok(mut current) = own.try_write() {
        current.gravatar_enabled = response.gravatar_enabled;
    }
    let image_from_meeting = meeting
        .as_ref()
        .is_some_and(|meeting| meeting.meeting_user_id().is_some());
    let unchanged = !image_from_meeting
        && own.try_peek().is_ok_and(|current| {
            current.image_url.is_some()
                && current.info.as_ref().map(|i| &i.etag)
                    == response.avatar.as_ref().map(|i| &i.etag)
        });
    if unchanged {
        if let Ok(mut current) = own.try_write() {
            current.refreshing = false;
            current.load = OwnAvatarLoad::Loaded;
            current.info = response.avatar;
        }
        return Ok(server);
    }
    let image = match response.avatar {
        Some(_) if !image_from_meeting => match fetch_my_avatar_image().await {
            Ok(image) => image,
            Err(e) => {
                log::warn!(
                    "avatar: loading your avatar image failed ({})",
                    api_error_kind(&e)
                );
                fail(own);
                return Err(OwnReadFailure::Unreadable);
            }
        },
        _ => None,
    };
    if !saved_meanwhile(own) {
        let info = response
            .avatar
            .filter(|_| image_from_meeting || image.is_some());
        set_own_avatar(own, info, image.as_ref());
        if let Some(cache) = meeting.as_ref().filter(|_| paint_tiles) {
            cache.reload_meeting_user();
        }
    }
    if let Ok(mut current) = own.try_write() {
        current.refreshing = false;
    };
    Ok(server)
}

/// The own avatar's image, and whether one is set (`None` until that is known): in a meeting,
/// the meeting user's, read with the room token so another account signed in to this browser
/// cannot replace it; otherwise the browser account's.
pub fn use_own_avatar() -> (AvatarView, Option<bool>) {
    let own = try_use_context::<OwnAvatarCtx>();
    let cache = try_use_context::<PeerAvatarCacheCtx>();
    let held = use_slot_claim(cache.clone());
    match cache {
        Some(cache) if cache.meeting_user_id().is_some() => cache
            .own_slot(&held)
            .and_then(|state| {
                let state = state.try_read().ok()?;
                Some((state.view(), state.is_set()))
            })
            .unwrap_or_default(),
        _ => own
            .map(|own| {
                let own = own.0.read();
                let is_set = (own.load == OwnAvatarLoad::Loaded).then_some(own.info.is_some());
                (own.view(), is_set)
            })
            .unwrap_or_default(),
    }
}

pub fn use_own_avatar_view() -> AvatarView {
    use_own_avatar().0
}

fn use_self_avatar_view() -> AvatarView {
    let signed_in = try_use_context::<PeerAvatarCacheCtx>().is_some_and(|c| !c.is_guest());
    let view = use_own_avatar_view();
    if signed_in {
        view
    } else {
        AvatarView::Default
    }
}

/// `true` when the current meeting's user may set an avatar (signed in, not a guest).
pub fn avatar_editing_allowed() -> bool {
    try_consume_context::<PeerAvatarCacheCtx>().is_some_and(|c| !c.is_guest())
}

/// The self tile's camera-off avatar.
#[component]
pub fn SelfAvatar(#[props(default)] extra_class: &'static str) -> Element {
    let view = use_self_avatar_view();
    rsx! {
        AvatarImage { view, testid: "self-avatar-img", extra_class }
    }
}

/// The own avatar when one is set, otherwise `children`.
#[component]
pub fn SelfAvatarOrIcon(children: Element) -> Element {
    let view = use_self_avatar_view();
    if matches!(view, AvatarView::Image(_)) {
        rsx! {
            AvatarImage { view, testid: "self-avatar-img" }
        }
    } else {
        children
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_skips_identities_that_are_not_verified_accounts() {
        assert_eq!(avatar_lookup_id("11", None), None);
        assert_eq!(avatar_lookup_id("11", Some("")), None);
        assert_eq!(avatar_lookup_id("11", Some(".")), None);
        assert_eq!(avatar_lookup_id("11", Some("..")), None);
        assert_eq!(avatar_lookup_id("11", Some("guest:abc")), None);
        assert_eq!(avatar_lookup_id("mock-3", Some("ann@example.com")), None);
    }

    #[test]
    fn lookup_keeps_real_user_ids_byte_for_byte() {
        for id in [
            "ann@example.com",
            "a/b+c@x %?#&",
            "auth0|42",
            "...",
            "Guest:x",
        ] {
            assert_eq!(avatar_lookup_id("11", Some(id)), Some(id));
        }
    }

    #[test]
    fn only_raster_image_types_may_back_a_blob() {
        assert_eq!(displayable_avatar_type("image/jpeg"), Some("image/jpeg"));
        assert_eq!(displayable_avatar_type("image/png"), Some("image/png"));
        assert_eq!(displayable_avatar_type("IMAGE/WEBP"), Some("image/webp"));
        assert_eq!(
            displayable_avatar_type("image/jpeg; charset=binary"),
            Some("image/jpeg")
        );
        for refused in [
            "",
            "text/html",
            "image/svg+xml",
            "image/gif",
            "application/octet-stream",
            "image/jpegx",
        ] {
            assert_eq!(displayable_avatar_type(refused), None, "{refused}");
        }
    }

    #[test]
    fn avatar_error_logs_never_carry_the_response_body_or_url() {
        let leaky = "http://api.test/api/v1/meetings/m1/avatar?user_id=ann%40example.com";
        let cases = [
            (ApiError::NotFound(leaky.into()), "404"),
            (ApiError::Forbidden(leaky.into()), "403"),
            (
                ApiError::ServerError {
                    status: 502,
                    body: leaky.into(),
                },
                "502",
            ),
            (ApiError::Config(leaky.into()), "configuration error"),
            (ApiError::NotAuthenticated, "401"),
            (ApiError::RateLimitExceeded, "429"),
        ];
        for (error, kind) in cases {
            assert_eq!(api_error_kind(&error), kind, "{error:?}");
        }
        assert_eq!(LookupFailure::TimedOut.log_kind(), "timed out");
    }

    #[test]
    fn a_failed_peer_fetch_is_retried_once() {
        assert!(should_retry_peer_avatar(1, true));
        assert!(!should_retry_peer_avatar(2, true));
        assert!(!should_retry_peer_avatar(1, false));
    }

    #[test]
    fn the_retry_delay_spreads_over_the_jitter_window() {
        assert_eq!(jittered_delay_ms(30_000, 10_000, 0.0), 20_000);
        assert_eq!(jittered_delay_ms(30_000, 10_000, 0.5), 30_000);
        assert_eq!(jittered_delay_ms(30_000, 10_000, 1.0), 40_000);
        assert_eq!(jittered_delay_ms(30_000, 10_000, 7.0), 40_000);
        assert_eq!(jittered_delay_ms(30_000, 10_000, f64::NAN), 20_000);
        assert_eq!(jittered_delay_ms(50, 0, 0.9), 50);
        assert_eq!(jittered_delay_ms(5, 10, 0.0), 0);
    }

    #[test]
    fn own_view_follows_the_image_then_the_load_state() {
        let mut own = OwnAvatar::default();
        assert_eq!(own.view(), AvatarView::Default);
        own.load = OwnAvatarLoad::Loading;
        assert_eq!(own.view(), AvatarView::Loading);
        own.image_url = Some("blob:x".into());
        assert_eq!(own.view(), AvatarView::Image("blob:x".into()));
        own.load = OwnAvatarLoad::Failed;
        own.image_url = None;
        assert_eq!(own.view(), AvatarView::Default);
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
pub(crate) mod dom_support {
    use dioxus::prelude::*;
    use gloo_timers::future::TimeoutFuture;
    use wasm_bindgen::{JsCast, JsValue};
    use wasm_bindgen_futures::JsFuture;

    pub fn js(src: &str) -> JsValue {
        js_sys::Function::new_no_args(src)
            .call0(&JsValue::NULL)
            .unwrap()
    }

    pub fn inject_config() {
        js(
            "window.__APP_CONFIG = Object.freeze({ apiBaseUrl: 'http://avatar.test', \
            wsUrl: 'ws://avatar.test', webTransportHost: 'https://avatar.test:4433', \
            oauthEnabled: 'false', e2eeEnabled: 'false', webTransportEnabled: 'false', \
            firefoxEnabled: 'false', usersAllowedToStream: '', serverElectionPeriodMs: 2000, \
            vadThreshold: 0.02 });",
        );
        crate::constants::reset_config_cache_for_test();
    }

    /// Replace `fetch` with `route(call)`; `json(status, body)` and `jpeg(color)` build replies,
    /// and `hold(reply)` keeps a reply back until [`release_held`].
    pub fn install_fetch(route: &str) {
        inject_config();
        js(&format!(
            r#"
            window.__avatarOrigFetch = window.__avatarOrigFetch || window.fetch;
            window.__avatarOrigCreate = window.__avatarOrigCreate || URL.createObjectURL;
            window.__avatarOrigRevoke = window.__avatarOrigRevoke || URL.revokeObjectURL;
            window.__avatarCalls = [];
            window.__avatarUrls = 0;
            window.__avatarRevoked = [];
            window.__avatarHeld = [];
            window.__avatarPending = 0;
            window.__avatarMaxPending = 0;
            window.__avatarHold = false;
            window.__freshWorks = false;
            window.__holdUpload = false;
            window.__holdDelete = false;
            window.__holdGravatar = false;
            window.__holdMeta = false;
            window.__avatarGone = false;
            window.__lastPutBody = null;
            URL.createObjectURL = function(blob) {{
                window.__avatarUrls += 1;
                return window.__avatarOrigCreate.call(URL, blob);
            }};
            URL.revokeObjectURL = function(url) {{
                window.__avatarRevoked.push(url);
                return window.__avatarOrigRevoke.call(URL, url);
            }};
            var json = function(status, body) {{
                return new Response(JSON.stringify(body), {{
                    status: status, headers: {{ 'content-type': 'application/json' }} }});
            }};
            var jpeg = function(color) {{
                return new Promise(function(resolve) {{
                    var c = document.createElement('canvas');
                    c.width = 4; c.height = 4;
                    var g = c.getContext('2d');
                    if (color) {{ g.fillStyle = color; }}
                    g.fillRect(0, 0, 4, 4);
                    c.toBlob(function(b) {{
                        resolve(new Response(b, {{ status: 200,
                            headers: {{ 'content-type': 'image/jpeg' }} }}));
                    }}, 'image/jpeg');
                }});
            }};
            var hold = function(reply) {{
                return new Promise(function(resolve) {{
                    window.__avatarHeld.push(function() {{ resolve(reply()); }});
                }});
            }};
            var route = function(call) {{ {route} }};
            window.fetch = async function(input, init) {{
                var req = typeof input === 'string' ? new Request(input, init) : input;
                var url = new URL(req.url);
                var body = req.method === 'PUT'
                    ? new Uint8Array(await req.clone().arrayBuffer()) : new Uint8Array(0);
                var type = req.headers.get('content-type');
                if (req.method === 'PUT') {{ window.__lastPutBody = body; }}
                var call = {{ method: req.method, host: url.host, path: url.pathname,
                    user_id: url.searchParams.get('user_id'),
                    expected: url.searchParams.get('expected_user_id'),
                    auth: req.headers.get('authorization'), type: type, size: body.length,
                    body: /json/.test(type || '') ? new TextDecoder().decode(body) : null }};
                window.__avatarCalls.push(call);
                window.__avatarPending += 1;
                window.__avatarMaxPending =
                    Math.max(window.__avatarMaxPending, window.__avatarPending);
                try {{
                    var resp = await route(call);
                }} finally {{
                    window.__avatarPending -= 1;
                }}
                Object.defineProperty(resp, 'url', {{ value: req.url }});
                return resp;
            }};
            "#
        ));
    }

    pub fn restore_fetch() {
        js(
            "if (window.__avatarOrigFetch) { window.fetch = window.__avatarOrigFetch; } \
            if (window.__avatarOrigCreate) { URL.createObjectURL = window.__avatarOrigCreate; } \
            if (window.__avatarOrigRevoke) { URL.revokeObjectURL = window.__avatarOrigRevoke; }",
        );
    }

    pub fn revoked_urls() -> Vec<String> {
        let text = js("return JSON.stringify(window.__avatarRevoked || []);")
            .as_string()
            .unwrap();
        serde_json::from_str(&text).unwrap()
    }

    pub fn calls() -> Vec<serde_json::Value> {
        let text = js("return JSON.stringify(window.__avatarCalls || []);")
            .as_string()
            .unwrap();
        serde_json::from_str(&text).unwrap()
    }

    pub fn calls_to(path: &str) -> Vec<serde_json::Value> {
        calls()
            .into_iter()
            .filter(|call| call["path"].as_str().is_some_and(|p| p.ends_with(path)))
            .collect()
    }

    pub fn object_urls_created() -> u32 {
        js("return window.__avatarUrls || 0;").as_f64().unwrap() as u32
    }

    /// Answer every request `hold` kept back so far; returns how many there were.
    pub fn release_held() -> u32 {
        js(
            "var held = window.__avatarHeld || []; window.__avatarHeld = []; \
            held.forEach(function(reply) { reply(); }); return held.length;",
        )
        .as_f64()
        .unwrap() as u32
    }

    /// The most requests that were waiting for a reply at the same time.
    pub fn max_pending() -> u32 {
        js("return window.__avatarMaxPending || 0;")
            .as_f64()
            .unwrap() as u32
    }

    /// Cache timings for one test; production values come back on drop.
    pub struct TimingsGuard;

    impl TimingsGuard {
        pub fn set(timings: super::AvatarTimings) -> Self {
            super::TEST_TIMINGS.with(|t| t.set(Some(timings)));
            Self
        }
    }

    impl Drop for TimingsGuard {
        fn drop(&mut self) {
            super::TEST_TIMINGS.with(|t| t.set(None));
        }
    }

    /// Retries after 50 ms with no jitter; everything else as in production.
    pub fn fast_retry() -> super::AvatarTimings {
        super::AvatarTimings {
            retry_ms: 50,
            retry_jitter_ms: 0,
            ..super::AVATAR_TIMINGS
        }
    }

    /// A blob: URL of a real 4x4 JPEG, created outside the code under test.
    pub async fn real_jpeg_url() -> String {
        let promise: js_sys::Promise = js("return new Promise(function(resolve) { \
                var c = document.createElement('canvas'); c.width = 4; c.height = 4; \
                c.getContext('2d').fillRect(0, 0, 4, 4); \
                c.toBlob(function(b) { \
                    resolve((window.__avatarOrigCreate || URL.createObjectURL).call(URL, b)); \
                }, 'image/jpeg'); });")
        .unchecked_into();
        JsFuture::from(promise).await.unwrap().as_string().unwrap()
    }

    pub fn mount(root: fn() -> Element) -> web_sys::Element {
        let document = gloo_utils::document();
        let mount = document.create_element("div").unwrap();
        document.body().unwrap().append_child(&mount).unwrap();
        dioxus::web::launch::launch_virtual_dom(
            VirtualDom::new(root),
            dioxus::web::Config::new().rootelement(mount.clone()),
        );
        mount
    }

    pub fn query(mount: &web_sys::Element, selector: &str) -> Option<web_sys::Element> {
        mount.query_selector(selector).unwrap()
    }

    pub fn attr(mount: &web_sys::Element, selector: &str, name: &str) -> Option<String> {
        query(mount, selector).and_then(|el| el.get_attribute(name))
    }

    pub fn click(mount: &web_sys::Element, selector: &str) {
        query(mount, selector)
            .unwrap_or_else(|| panic!("{selector} is not rendered"))
            .unchecked_into::<web_sys::HtmlElement>()
            .click();
    }

    pub fn active_element_id() -> String {
        gloo_utils::document()
            .active_element()
            .map(|el| el.id())
            .unwrap_or_default()
    }

    pub async fn wait_until(mut ready: impl FnMut() -> bool) -> bool {
        for _ in 0..150 {
            if ready() {
                return true;
            }
            TimeoutFuture::new(20).await;
        }
        ready()
    }

    pub async fn settle() {
        TimeoutFuture::new(80).await;
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod dom_tests {
    use super::dom_support::*;
    use super::*;
    use crate::components::canvas_generator::{PinnedTile, TileMode};
    use crate::components::decode_budget::TileRenderMode;
    use crate::components::peer_tile::{camera_tiles, CameraTiles};
    use crate::context::{
        AppearanceSettings, AppearanceSettingsCtx, MeetingTime, PeerAudioLivenessMap,
        PeerSignalHistoryMap, SignalPopupStateMap,
    };
    use std::collections::HashMap;
    use videocall_client::VideoCallClient;
    use wasm_bindgen_test::wasm_bindgen_test;

    thread_local! {
        static DECODE_REQUESTS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
        static REMOUNT: RefCell<Option<Signal<u32>>> = const { RefCell::new(None) };
        static IN_MEETING: RefCell<Option<Signal<bool>>> = const { RefCell::new(None) };
    }

    #[allow(non_snake_case)]
    fn MeetingScope() -> Element {
        use_provide_peer_avatar_cache(meeting_cache(false));
        rsx! {
            div { id: "pa-end",
                PeerAvatar { session_id: "31".to_string(), user_id: Some("dee@example.com".to_string()) }
            }
        }
    }

    #[allow(non_snake_case)]
    fn MeetingThatEnds() -> Element {
        let in_meeting = use_signal(|| true);
        IN_MEETING.with(|s| *s.borrow_mut() = Some(in_meeting));
        rsx! {
            if in_meeting() {
                MeetingScope {}
            }
        }
    }

    #[wasm_bindgen_test]
    async fn leaving_the_meeting_revokes_every_avatar_url() {
        install_fetch("return jpeg();");
        let mount = mount(MeetingThatEnds);
        let shown = wait_until(|| src_of(&mount, "pa-end").starts_with("blob:")).await;
        let url = src_of(&mount, "pa-end");
        let revoked_while_shown = revoked_urls().contains(&url);
        let mut in_meeting = IN_MEETING.with(|s| s.borrow().unwrap());
        in_meeting.set(false);
        settle().await;
        let revoked = revoked_urls();
        restore_fetch();
        mount.remove();
        assert!(shown);
        assert!(!revoked_while_shown, "a URL in use is not revoked");
        assert!(
            revoked.contains(&url),
            "teardown revokes {url}: {revoked:?}"
        );
    }

    fn meeting_cache(is_guest: bool) -> AvatarCacheConfig {
        AvatarCacheConfig {
            meeting_id: "m1".to_string(),
            room_token: "room-token".to_string(),
            is_guest,
            own_user_id: Some("me@example.com".to_string()),
        }
    }

    fn state_of(mount: &web_sys::Element, id: &str) -> Option<String> {
        attr(
            mount,
            &format!("#{id} .peer-avatar-slot"),
            "data-avatar-state",
        )
    }

    fn src_of(mount: &web_sys::Element, id: &str) -> String {
        attr(mount, &format!("#{id} img.peer-avatar"), "src").unwrap_or_default()
    }

    #[wasm_bindgen_test]
    fn a_blob_is_typed_with_the_served_image_type_or_not_built() {
        let image = |content_type: &str| AvatarBytes {
            bytes: vec![1, 2, 3],
            content_type: content_type.to_string(),
        };
        assert!(avatar_blob(&image("text/html")).is_none());
        assert!(avatar_blob(&image("")).is_none());
        assert!(avatar_blob(&image("image/svg+xml")).is_none());
        let blob = avatar_blob(&image("image/jpeg")).expect("a jpeg blob");
        assert_eq!(blob.type_(), "image/jpeg");
        assert_eq!(blob.size(), 3.0);
    }

    #[allow(non_snake_case)]
    fn TypedResponses() -> Element {
        use_provide_peer_avatar_cache(meeting_cache(false));
        rsx! {
            div { id: "pa-html",
                PeerAvatar { session_id: "1".to_string(), user_id: Some("html@example.com".to_string()) }
            }
            div { id: "pa-untyped",
                PeerAvatar { session_id: "2".to_string(), user_id: Some("untyped@example.com".to_string()) }
            }
            div { id: "pa-jpeg",
                PeerAvatar { session_id: "3".to_string(), user_id: Some("jpeg@example.com".to_string()) }
            }
        }
    }

    #[wasm_bindgen_test]
    async fn only_an_image_typed_response_becomes_an_object_url() {
        install_fetch(
            "if (call.user_id === 'html@example.com') { return new Response('<script>x</script>', \
               { status: 200, headers: { 'content-type': 'text/html' } }); } \
             if (call.user_id === 'untyped@example.com') { \
               return new Response(new Uint8Array([255, 216, 255]), { status: 200 }); } \
             return jpeg();",
        );
        let mount = mount(TypedResponses);
        let shown = wait_until(|| src_of(&mount, "pa-jpeg").starts_with("blob:")).await;
        settle().await;
        let html = state_of(&mount, "pa-html");
        let untyped = state_of(&mount, "pa-untyped");
        let fetched = calls_to("/meetings/m1/avatar").len();
        let created = object_urls_created();
        let html_img = query(&mount, "#pa-html img").is_some();
        restore_fetch();
        mount.remove();
        assert!(shown, "the image/jpeg avatar must render from a blob: URL");
        assert_eq!(fetched, 3, "all three avatars were fetched");
        assert_eq!(html.as_deref(), Some("default"));
        assert_eq!(untyped.as_deref(), Some("default"));
        assert!(!html_img, "a text/html body must never reach an <img>");
        assert_eq!(
            created, 1,
            "only the image/jpeg response may get an object URL"
        );
    }

    #[allow(non_snake_case)]
    fn UnverifiedIdentities() -> Element {
        use_provide_peer_avatar_cache(meeting_cache(false));
        let ids: [(&str, Option<&str>); 7] = [
            ("1", None),
            ("2", Some("")),
            ("3", Some(".")),
            ("4", Some("..")),
            ("5", Some("guest:abc")),
            ("mock-6", Some("ann@example.com")),
            ("7", Some("cy@example.com")),
        ];
        rsx! {
            for (session, user) in ids {
                div { key: "{session}", id: "pa-{session}",
                    PeerAvatar { session_id: session.to_string(), user_id: user.map(str::to_string) }
                }
            }
        }
    }

    #[wasm_bindgen_test]
    async fn no_avatar_is_requested_for_an_unverified_identity() {
        install_fetch("return jpeg();");
        let mount = mount(UnverifiedIdentities);
        let control = wait_until(|| src_of(&mount, "pa-7").starts_with("blob:")).await;
        settle().await;
        let fetched = calls_to("/meetings/m1/avatar");
        let states: Vec<_> = ["1", "2", "3", "4", "5", "mock-6"]
            .iter()
            .map(|s| state_of(&mount, &format!("pa-{s}")))
            .collect();
        restore_fetch();
        mount.remove();
        assert!(
            control,
            "the verified peer's avatar renders, so fetching works"
        );
        assert_eq!(
            fetched.len(),
            1,
            "only the verified account is looked up: {fetched:?}"
        );
        assert_eq!(fetched[0]["user_id"], "cy@example.com");
        assert_eq!(fetched[0]["auth"], "Bearer room-token");
        assert!(
            states.iter().all(|s| s.as_deref() == Some("default")),
            "{states:?}"
        );
    }

    #[allow(non_snake_case)]
    fn Remountable() -> Element {
        use_provide_peer_avatar_cache(meeting_cache(false));
        let generation = use_signal(|| 0u32);
        REMOUNT.with(|r| *r.borrow_mut() = Some(generation));
        let session = match generation() {
            0 | 1 => "8",
            2 => "9",
            _ => "10",
        };
        rsx! {
            for generation in [generation()] {
                div { key: "{generation}", id: "pa-none",
                    PeerAvatar { session_id: session.to_string(), user_id: Some("none@example.com".to_string()) }
                }
            }
        }
    }

    fn bump_remount() {
        let mut generation = REMOUNT.with(|r| r.borrow().unwrap());
        generation += 1;
    }

    #[wasm_bindgen_test]
    async fn every_new_session_is_looked_up_once_and_a_remount_is_not() {
        install_fetch(
            "return json(404, { success: false, \
               result: { code: 'AVATAR_NOT_FOUND', message: 'm' } });",
        );
        let mount = mount(Remountable);
        let first = wait_until(|| calls_to("/meetings/m1/avatar").len() == 1).await;
        settle().await;
        bump_remount();
        settle().await;
        let after_remount = calls_to("/meetings/m1/avatar").len();
        bump_remount();
        let second_session = wait_until(|| calls_to("/meetings/m1/avatar").len() == 2).await;
        settle().await;
        let state = state_of(&mount, "pa-none");
        bump_remount();
        let third_session = wait_until(|| calls_to("/meetings/m1/avatar").len() == 3).await;
        settle().await;
        let lookups = calls_to("/meetings/m1/avatar").len();
        restore_fetch();
        mount.remove();
        assert!(first);
        assert_eq!(
            after_remount, 1,
            "a remounted tile of the same session reuses the cached None"
        );
        assert!(
            second_session,
            "a new session is looked up even moments after the last answer"
        );
        assert_eq!(state.as_deref(), Some("default"));
        assert!(third_session);
        assert_eq!(lookups, 3, "one lookup per session");
    }

    thread_local! {
        static SESSION: RefCell<Option<Signal<&'static str>>> = const { RefCell::new(None) };
        static SHOWN: RefCell<Option<Signal<bool>>> = const { RefCell::new(None) };
        static MEETING: RefCell<Option<Signal<(&'static str, &'static str)>>> =
            const { RefCell::new(None) };
        static EXPIRING: Cell<(&'static str, bool)> = const { Cell::new(("m1", false)) };
        static REJOIN_CACHE: RefCell<Option<PeerAvatarCacheCtx>> = const { RefCell::new(None) };
    }

    #[allow(non_snake_case)]
    fn OneUserManySessions() -> Element {
        use_provide_peer_avatar_cache(meeting_cache(false));
        rsx! {
            for session in ["51", "52", "53"] {
                div { key: "{session}", id: "pa-{session}",
                    PeerAvatar { session_id: session.to_string(), user_id: Some("multi@example.com".to_string()) }
                }
            }
            div { id: "pa-54",
                PeerAvatar { session_id: "54".to_string(), user_id: Some("other@example.com".to_string()) }
            }
        }
    }

    #[wasm_bindgen_test]
    async fn every_session_of_one_user_shares_one_lookup_and_one_url() {
        install_fetch("return jpeg();");
        let mount = mount(OneUserManySessions);
        let ids = ["pa-51", "pa-52", "pa-53", "pa-54"];
        let shown =
            wait_until(|| ids.iter().all(|id| src_of(&mount, id).starts_with("blob:"))).await;
        settle().await;
        let srcs: Vec<String> = ids[..3].iter().map(|id| src_of(&mount, id)).collect();
        let lookups: Vec<String> = calls_to("/meetings/m1/avatar")
            .iter()
            .map(|c| c["user_id"].as_str().unwrap_or_default().to_string())
            .collect();
        let created = object_urls_created();
        restore_fetch();
        mount.remove();
        assert!(shown);
        assert_eq!(
            lookups.iter().filter(|u| *u == "multi@example.com").count(),
            1,
            "{lookups:?}"
        );
        assert_eq!(lookups.len(), 2, "{lookups:?}");
        assert!(srcs.iter().all(|s| *s == srcs[0]), "{srcs:?}");
        assert_eq!(created, 2, "one object URL per user");
    }

    #[allow(non_snake_case)]
    fn Rejoining() -> Element {
        let cache = use_provide_peer_avatar_cache(meeting_cache(false));
        REJOIN_CACHE.with(|c| *c.borrow_mut() = Some(cache));
        let session = use_signal(|| "61");
        SESSION.with(|s| *s.borrow_mut() = Some(session));
        rsx! {
            for id in [session()] {
                div { key: "{id}", id: "pa-rejoin",
                    PeerAvatar { session_id: id.to_string(), user_id: Some("back@example.com".to_string()) }
                }
            }
        }
    }

    fn set_session(id: &'static str) {
        let mut session = SESSION.with(|s| s.borrow().unwrap());
        session.set(id);
    }

    fn pending_settled() -> bool {
        js("return window.__avatarPending;").as_f64() == Some(0.0)
    }

    #[wasm_bindgen_test]
    async fn a_rejoined_user_keeps_the_avatar_while_it_is_revalidated() {
        install_fetch("return window.__avatarHold ? hold(jpeg) : jpeg();");
        let mount = mount(Rejoining);
        let shown = wait_until(|| src_of(&mount, "pa-rejoin").starts_with("blob:")).await;
        let first = src_of(&mount, "pa-rejoin");
        js("window.__avatarHold = true;");
        set_session("62");
        let revalidating = wait_until(|| calls_to("/meetings/m1/avatar").len() == 2).await;
        settle().await;
        let state_during = state_of(&mount, "pa-rejoin");
        let src_during = src_of(&mount, "pa-rejoin");
        release_held();
        let answered = wait_until(pending_settled).await;
        settle().await;
        let after = src_of(&mount, "pa-rejoin");
        let created = object_urls_created();
        restore_fetch();
        mount.remove();
        assert!(shown);
        assert!(
            revalidating,
            "the new session is looked up, however recent the last answer"
        );
        assert_eq!(state_during.as_deref(), Some("image"), "no loading flicker");
        assert_eq!(
            src_during, first,
            "the new session shows the known image at once"
        );
        assert!(answered);
        assert_eq!(after, first, "an unchanged image keeps its URL");
        assert_eq!(created, 1);
    }

    #[wasm_bindgen_test]
    async fn an_avatar_removed_before_a_quick_rejoin_is_dropped_from_the_tile() {
        install_fetch(
            "if (window.__avatarGone) { return json(404, { success: false, \
               result: { code: 'AVATAR_NOT_FOUND', message: 'm' } }); } \
             return jpeg();",
        );
        let mount = mount(Rejoining);
        let shown = wait_until(|| src_of(&mount, "pa-rejoin").starts_with("blob:")).await;
        let url = src_of(&mount, "pa-rejoin");
        js("window.__avatarGone = true;");
        set_session("62");
        let looked_up = wait_until(|| calls_to("/meetings/m1/avatar").len() == 2).await;
        let dropped =
            wait_until(|| state_of(&mount, "pa-rejoin").as_deref() == Some("default")).await;
        let released = wait_until(|| revoked_urls().contains(&url)).await;
        restore_fetch();
        mount.remove();
        assert!(shown);
        assert!(looked_up, "the rejoin is looked up at once");
        assert!(
            dropped,
            "AVATAR_NOT_FOUND turns the tile back to the default icon"
        );
        assert!(released, "the removed image's URL is released");
    }

    #[wasm_bindgen_test]
    async fn a_lookup_answered_after_an_own_save_does_not_overwrite_it() {
        install_fetch("return window.__avatarHold ? hold(jpeg) : jpeg();");
        let mount = mount(Rejoining);
        let shown = wait_until(|| src_of(&mount, "pa-rejoin").starts_with("blob:")).await;
        js("window.__avatarHold = true;");
        set_session("62");
        let held = wait_until(|| calls_to("/meetings/m1/avatar").len() == 2).await;
        let cache = REJOIN_CACHE.with(|c| c.borrow().clone().unwrap());
        cache.set_user_image("back@example.com", None);
        let removed =
            wait_until(|| state_of(&mount, "pa-rejoin").as_deref() == Some("default")).await;
        release_held();
        let answered = wait_until(pending_settled).await;
        settle().await;
        settle().await;
        let after = state_of(&mount, "pa-rejoin");
        restore_fetch();
        mount.remove();
        assert!(shown);
        assert!(held, "a revalidation is in flight while the user saves");
        assert!(removed);
        assert!(answered);
        assert_eq!(
            after.as_deref(),
            Some("default"),
            "the older answer must not bring the removed image back"
        );
    }

    #[allow(non_snake_case)]
    fn Leaving() -> Element {
        use_provide_peer_avatar_cache(meeting_cache(false));
        let shown = use_signal(|| true);
        SHOWN.with(|s| *s.borrow_mut() = Some(shown));
        rsx! {
            if shown() {
                div { id: "pa-leave",
                    PeerAvatar { session_id: "71".to_string(), user_id: Some("leaver@example.com".to_string()) }
                }
            }
        }
    }

    fn show_leaver(on: bool) {
        let mut shown = SHOWN.with(|s| s.borrow().unwrap());
        shown.set(on);
    }

    #[wasm_bindgen_test]
    async fn a_departed_users_image_is_released_after_the_grace_period() {
        let _timings = TimingsGuard::set(super::AvatarTimings {
            prune_grace_ms: 300,
            ..AVATAR_TIMINGS
        });
        install_fetch("return jpeg();");
        let mount = mount(Leaving);
        let shown = wait_until(|| src_of(&mount, "pa-leave").starts_with("blob:")).await;
        let url = src_of(&mount, "pa-leave");
        show_leaver(false);
        settle().await;
        show_leaver(true);
        let back_at_once = wait_until(|| src_of(&mount, "pa-leave") == url).await;
        let lookups_after_flap = calls_to("/meetings/m1/avatar").len();
        show_leaver(false);
        settle().await;
        let revoked_early = revoked_urls().contains(&url);
        let released = wait_until(|| revoked_urls().contains(&url)).await;
        show_leaver(true);
        let looked_up = wait_until(|| calls_to("/meetings/m1/avatar").len() == 2).await;
        let reshown = wait_until(|| {
            let src = src_of(&mount, "pa-leave");
            src.starts_with("blob:") && src != url
        })
        .await;
        restore_fetch();
        mount.remove();
        assert!(shown);
        assert!(
            back_at_once,
            "a tile back within the grace period keeps the image"
        );
        assert_eq!(lookups_after_flap, 1);
        assert!(
            !revoked_early,
            "nothing is released inside the grace period"
        );
        assert!(released, "the image is released once no tile shows it");
        assert!(looked_up, "a user who comes back later is looked up again");
        assert!(reshown);
    }

    #[wasm_bindgen_test]
    async fn a_tile_back_within_the_grace_period_keeps_its_image_past_it() {
        let _timings = TimingsGuard::set(super::AvatarTimings {
            prune_grace_ms: 300,
            ..AVATAR_TIMINGS
        });
        install_fetch("return jpeg();");
        let mount = mount(Leaving);
        let shown = wait_until(|| src_of(&mount, "pa-leave").starts_with("blob:")).await;
        let url = src_of(&mount, "pa-leave");
        show_leaver(false);
        settle().await;
        show_leaver(true);
        let back = wait_until(|| src_of(&mount, "pa-leave") == url).await;
        TimeoutFuture::new(1_800).await;
        let revoked = revoked_urls().contains(&url);
        let src = src_of(&mount, "pa-leave");
        let lookups = calls_to("/meetings/m1/avatar").len();
        restore_fetch();
        mount.remove();
        assert!(shown && back);
        assert!(
            !revoked,
            "a tile that came back keeps its image past the grace period"
        );
        assert_eq!(src, url);
        assert_eq!(lookups, 1);
    }

    #[wasm_bindgen_test]
    async fn a_tile_hidden_again_gets_a_full_grace_period_from_the_last_hide() {
        let _timings = TimingsGuard::set(super::AvatarTimings {
            prune_grace_ms: 1_500,
            ..AVATAR_TIMINGS
        });
        install_fetch("return jpeg();");
        let mount = mount(Leaving);
        let shown = wait_until(|| src_of(&mount, "pa-leave").starts_with("blob:")).await;
        let url = src_of(&mount, "pa-leave");
        show_leaver(false);
        TimeoutFuture::new(100).await;
        show_leaver(true);
        TimeoutFuture::new(500).await;
        show_leaver(false);
        TimeoutFuture::new(1_200).await;
        show_leaver(true);
        let back = wait_until(|| src_of(&mount, "pa-leave") == url).await;
        TimeoutFuture::new(800).await;
        let revoked = revoked_urls().contains(&url);
        let src = src_of(&mount, "pa-leave");
        let lookups = calls_to("/meetings/m1/avatar").len();
        restore_fetch();
        mount.remove();
        assert!(shown);
        assert!(
            back,
            "the first hide's timer must not prune a slot that was hidden again later"
        );
        assert!(!revoked);
        assert_eq!(src, url);
        assert_eq!(lookups, 1);
    }

    #[allow(non_snake_case)]
    fn ManyPeers() -> Element {
        use_provide_peer_avatar_cache(meeting_cache(false));
        rsx! {
            for n in 81..86 {
                div { key: "{n}", id: "pa-{n}",
                    PeerAvatar { session_id: n.to_string(), user_id: Some(format!("p{n}@example.com")) }
                }
            }
        }
    }

    #[wasm_bindgen_test]
    async fn no_more_than_four_avatar_lookups_are_in_flight() {
        install_fetch("return hold(jpeg);");
        let mount = mount(ManyPeers);
        let four = wait_until(|| calls_to("/meetings/m1/avatar").len() == 4).await;
        settle().await;
        let while_busy = calls_to("/meetings/m1/avatar").len();
        release_held();
        let fifth = wait_until(|| calls_to("/meetings/m1/avatar").len() == 5).await;
        release_held();
        let all = wait_until(|| {
            (81..86).all(|n| src_of(&mount, &format!("pa-{n}")).starts_with("blob:"))
        })
        .await;
        let max = max_pending();
        restore_fetch();
        mount.remove();
        assert!(four);
        assert_eq!(while_busy, 4, "the fifth lookup waits for a free slot");
        assert!(fifth, "the fifth lookup starts once a slot frees up");
        assert!(all);
        assert_eq!(max, 4);
    }

    #[wasm_bindgen_test]
    async fn hung_lookups_give_up_their_slots_after_the_deadline() {
        let _timings = TimingsGuard::set(super::AvatarTimings {
            lookup_timeout_ms: 300,
            ..fast_retry()
        });
        install_fetch("return hold(jpeg);");
        let mount = mount(ManyPeers);
        let users_looked_up = || {
            calls_to("/meetings/m1/avatar")
                .iter()
                .filter_map(|c| c["user_id"].as_str().map(str::to_string))
                .collect::<HashSet<_>>()
                .len()
        };
        let four = wait_until(|| calls_to("/meetings/m1/avatar").len() == 4).await;
        let while_hung = users_looked_up();
        let fifth = wait_until(|| users_looked_up() == 5).await;
        let bounded = wait_until(|| calls_to("/meetings/m1/avatar").len() == 10).await;
        TimeoutFuture::new(400).await;
        let total = calls_to("/meetings/m1/avatar").len();
        let states: Vec<_> = (81..86)
            .map(|n| state_of(&mount, &format!("pa-{n}")))
            .collect();
        restore_fetch();
        mount.remove();
        assert!(four);
        assert_eq!(
            while_hung, 4,
            "the fifth tile waits while four lookups hang"
        );
        assert!(fifth, "a hung lookup frees its slot at the deadline");
        assert!(bounded);
        assert_eq!(
            total, 10,
            "a timed-out lookup is retried once, like any failure"
        );
        assert!(
            states.iter().all(|s| s.as_deref() == Some("default")),
            "{states:?}"
        );
    }

    #[allow(non_snake_case)]
    fn StuckRefreshPeers() -> Element {
        use_provide_peer_avatar_cache(AvatarCacheConfig {
            meeting_id: "m-stuck".to_string(),
            ..meeting_cache(false)
        });
        rsx! {
            for n in 71..76 {
                div { key: "{n}", id: "pa-s{n}",
                    PeerAvatar { session_id: format!("s{n}"), user_id: Some(format!("s{n}@example.com")) }
                }
            }
        }
    }

    #[wasm_bindgen_test]
    async fn a_hung_room_token_refresh_frees_the_lookup_slots_at_the_deadline() {
        let _timings = TimingsGuard::set(super::AvatarTimings {
            lookup_timeout_ms: 300,
            ..fast_retry()
        });
        install_fetch(
            "if (/\\/status$/.test(call.path)) { return hold(function() { return json(500, {}); }); } \
             return json(401, { success: false, result: { code: 'UNAUTHORIZED', message: 'm' } });",
        );
        let mount = mount(StuckRefreshPeers);
        let lookups = || calls_to("/meetings/m-stuck/avatar");
        let users_looked_up = || {
            lookups()
                .iter()
                .filter_map(|c| c["user_id"].as_str().map(str::to_string))
                .collect::<HashSet<_>>()
                .len()
        };
        let four = wait_until(|| lookups().len() == 4).await;
        let refreshing = wait_until(|| calls_to("/meetings/m-stuck/status").len() == 1).await;
        let while_stuck = users_looked_up();
        let fifth = wait_until(|| users_looked_up() == 5).await;
        let bounded = wait_until(|| lookups().len() == 10).await;
        TimeoutFuture::new(400).await;
        let total = lookups().len();
        let refreshes = calls_to("/meetings/m-stuck/status").len();
        restore_fetch();
        mount.remove();
        assert!(four && refreshing);
        assert_eq!(
            while_stuck, 4,
            "the fifth tile waits while four lookups wait on the refresh"
        );
        assert!(
            fifth,
            "a refresh that never answers frees the lookup slots at the deadline"
        );
        assert!(bounded);
        assert_eq!(total, 10, "two attempts per tile, then nothing more");
        assert_eq!(refreshes, 1, "the hung refresh is shared, not repeated");
    }

    const EXPIRED_TOKEN_ROUTES: &str = "\
        if (/\\/status$/.test(call.path)) { return json(200, { success: true, \
            result: { user_id: 'me@example.com', status: 'admitted', is_host: false, \
            joined_at: 1, room_token: 'fresh' } }); } \
        if (call.auth === 'Bearer fresh' && window.__freshWorks) { return jpeg(); } \
        return json(401, { success: false, result: { code: 'UNAUTHORIZED', message: 'm' } });";

    fn expiring(meeting_id: &'static str, is_guest: bool) {
        EXPIRING.with(|e| e.set((meeting_id, is_guest)));
    }

    #[allow(non_snake_case)]
    fn ExpiringPeers() -> Element {
        let (meeting_id, is_guest) = EXPIRING.with(Cell::get);
        use_provide_peer_avatar_cache(AvatarCacheConfig {
            meeting_id: meeting_id.to_string(),
            ..meeting_cache(is_guest)
        });
        rsx! {
            for n in 91..94 {
                div { key: "{n}", id: "pa-{n}",
                    PeerAvatar { session_id: n.to_string(), user_id: Some(format!("t{n}@example.com")) }
                }
            }
        }
    }

    fn lookups_with(lookups: &[serde_json::Value], token: &str) -> usize {
        let auth = format!("Bearer {token}");
        lookups
            .iter()
            .filter(|c| c["auth"] == auth.as_str())
            .count()
    }

    #[wasm_bindgen_test]
    async fn an_expired_room_token_is_refreshed_once_for_every_tile() {
        expiring("m-refresh", false);
        install_fetch(EXPIRED_TOKEN_ROUTES);
        js("window.__freshWorks = true;");
        let mount = mount(ExpiringPeers);
        let shown = wait_until(|| {
            (91..94).all(|n| src_of(&mount, &format!("pa-{n}")).starts_with("blob:"))
        })
        .await;
        settle().await;
        let refreshes = calls_to("/meetings/m-refresh/status");
        let lookups = calls_to("/meetings/m-refresh/avatar");
        restore_fetch();
        mount.remove();
        assert!(shown, "every tile shows its avatar after the refresh");
        assert_eq!(
            refreshes.len(),
            1,
            "one refresh serves every tile: {refreshes:?}"
        );
        assert_eq!(refreshes[0]["path"], "/api/v1/meetings/m-refresh/status");
        assert_eq!(lookups_with(&lookups, "room-token"), 3, "{lookups:?}");
        assert_eq!(
            lookups_with(&lookups, "fresh"),
            3,
            "each resent lookup carries the new token: {lookups:?}"
        );
    }

    #[wasm_bindgen_test]
    async fn a_guest_never_refreshes_its_room_token() {
        let _timings = TimingsGuard::set(fast_retry());
        expiring("m-guest", true);
        install_fetch(EXPIRED_TOKEN_ROUTES);
        js("window.__freshWorks = true;");
        let mount = mount(ExpiringPeers);
        let retried = wait_until(|| calls_to("/meetings/m-guest/avatar").len() == 6).await;
        TimeoutFuture::new(300).await;
        let lookups = calls_to("/meetings/m-guest/avatar").len();
        let refreshes = calls_to("/meetings/m-guest/status").len();
        let every_refresh: Vec<_> = calls_to("/status")
            .iter()
            .map(|c| c["path"].clone())
            .collect();
        let states: Vec<_> = (91..94)
            .map(|n| state_of(&mount, &format!("pa-{n}")))
            .collect();
        restore_fetch();
        mount.remove();
        assert_eq!(
            refreshes, 0,
            "a guest's room token is never swapped: {every_refresh:?}"
        );
        assert_eq!(lookups, 6, "two attempts per tile, then nothing more");
        assert!(retried);
        assert!(
            states.iter().all(|s| s.as_deref() == Some("default")),
            "{states:?}"
        );
    }

    #[wasm_bindgen_test]
    async fn a_rejected_room_token_is_retried_a_bounded_number_of_times() {
        let _timings = TimingsGuard::set(fast_retry());
        expiring("m-bounded", false);
        install_fetch(EXPIRED_TOKEN_ROUTES);
        let mount = mount(ExpiringPeers);
        let settled = wait_until(|| calls_to("/meetings/m-bounded/avatar").len() >= 9).await;
        TimeoutFuture::new(500).await;
        let lookups = calls_to("/meetings/m-bounded/avatar");
        let refreshes = calls_to("/meetings/m-bounded/status").len();
        restore_fetch();
        mount.remove();
        assert!(settled);
        assert_eq!(
            lookups.len(),
            9,
            "per tile: a lookup, one resend, one retry: {lookups:?}"
        );
        for n in 91..94 {
            let user = format!("t{n}@example.com");
            assert_eq!(
                lookups
                    .iter()
                    .filter(|c| c["user_id"] == user.as_str())
                    .count(),
                3,
                "{user}"
            );
        }
        assert_eq!(refreshes, 1, "one refresh for the whole meeting");
    }

    #[allow(non_snake_case)]
    fn SwitchingMeeting() -> Element {
        let meeting = use_signal(|| ("m1", "room-token"));
        MEETING.with(|m| *m.borrow_mut() = Some(meeting));
        let (meeting_id, room_token) = meeting();
        rsx! {
            AvatarCacheProvider {
                meeting_id: meeting_id.to_string(),
                room_token: room_token.to_string(),
                own_user_id: Some("me@example.com".to_string()),
                div { id: "pa-switch",
                    PeerAvatar { session_id: "41".to_string(), user_id: Some("fay@example.com".to_string()) }
                }
            }
        }
    }

    #[wasm_bindgen_test]
    async fn a_new_meeting_drops_a_token_refreshed_for_the_old_one() {
        install_fetch(
            "if (/\\/status$/.test(call.path)) { return json(200, { success: true, \
               result: { user_id: 'me@example.com', status: 'admitted', is_host: false, \
               joined_at: 1, room_token: 'fresh' } }); } \
             if (call.path.indexOf('/meetings/m1/') >= 0 && call.auth !== 'Bearer fresh') { \
               return json(401, { success: false, result: { code: 'UNAUTHORIZED', message: 'm' } }); } \
             return jpeg();",
        );
        let mount = mount(SwitchingMeeting);
        let refreshed = wait_until(|| src_of(&mount, "pa-switch").starts_with("blob:")).await;
        let mut meeting = MEETING.with(|m| m.borrow().unwrap());
        meeting.set(("m6", "room-token"));
        let moved = wait_until(|| calls_to("/meetings/m6/avatar").len() == 1).await;
        let lookup = calls_to("/meetings/m6/avatar");
        restore_fetch();
        mount.remove();
        assert!(
            refreshed,
            "the first meeting's lookup succeeds after a refresh"
        );
        assert!(moved);
        assert_eq!(
            lookup[0]["auth"], "Bearer room-token",
            "the new meeting uses its own room token, not the old meeting's refresh"
        );
    }

    #[wasm_bindgen_test]
    async fn the_cache_follows_a_new_meeting_and_room_token() {
        install_fetch("return jpeg();");
        let mount = mount(SwitchingMeeting);
        let shown = wait_until(|| src_of(&mount, "pa-switch").starts_with("blob:")).await;
        let first = src_of(&mount, "pa-switch");
        let mut meeting = MEETING.with(|m| m.borrow().unwrap());
        meeting.set(("m2", "token-2"));
        let moved = wait_until(|| calls_to("/meetings/m2/avatar").len() == 1).await;
        let reshown = wait_until(|| {
            let src = src_of(&mount, "pa-switch");
            src.starts_with("blob:") && src != first
        })
        .await;
        let released = wait_until(|| revoked_urls().contains(&first)).await;
        let second = calls_to("/meetings/m2/avatar");
        let first_meeting = calls_to("/meetings/m1/avatar").len();
        restore_fetch();
        mount.remove();
        assert!(shown);
        assert!(moved, "the lookup goes to the new meeting");
        assert_eq!(second[0]["auth"], "Bearer token-2");
        assert_eq!(first_meeting, 1);
        assert!(reshown);
        assert!(released, "the previous meeting's image is released");
    }

    #[allow(non_snake_case)]
    fn VideoOffTiles() -> Element {
        let client = use_hook(|| {
            let client = VideoCallClient::new_for_test("me@example.com");
            client.insert_peer_for_test(11, "ann@example.com");
            client.set_peer_media_for_test(11, false, false);
            client.insert_peer_for_test(12, "bea@example.com");
            client.set_peer_media_for_test(12, true, false);
            client
        });
        use_context_provider(|| client.clone());
        let history_map: PeerSignalHistoryMap = use_signal(HashMap::new);
        use_context_provider(|| history_map);
        let liveness_map: PeerAudioLivenessMap = use_signal(HashMap::new);
        use_context_provider(|| liveness_map);
        let popup_map: SignalPopupStateMap = use_signal(HashMap::new);
        use_context_provider(|| popup_map);
        let appearance = use_signal(AppearanceSettings::default);
        use_context_provider(|| AppearanceSettingsCtx(appearance));
        let meeting_time = use_signal(MeetingTime::default);
        use_context_provider(|| meeting_time);
        use_provide_peer_avatar_cache(meeting_cache(false));
        let pin: EventHandler<PinnedTile> = use_callback(|_: PinnedTile| {});
        let request: EventHandler<String> =
            use_callback(|sid: String| DECODE_REQUESTS.with(|r| r.borrow_mut().push(sid)));
        let tiles = vec![
            ("11".to_string(), TileRenderMode::Decoded),
            ("12".to_string(), TileRenderMode::Avatar),
            ("99".to_string(), TileRenderMode::Decoded),
        ];
        let ranks = HashMap::new();
        let no_bleed = |_: &str| false;
        rsx! {
            div {
                id: "grid-container",
                style: "display: flex; width: 900px; height: 400px; --tile-w: 300px; --tile-h: 200px;",
                {camera_tiles(CameraTiles {
                    tiles: &tiles,
                    pin_rank: &ranks,
                    enlarged: &Default::default(),
                    on_toggle_enlarge: pin,
                    full_bleed: &no_bleed,
                    host_user_id: &Some("ann@example.com".to_string()),
                    render_mode: &TileMode::Full,
                    my_session_id: &None,
                    room_id: "m1",
                    is_host: false,
                    on_toggle_pin: pin,
                    on_request_decode: request,
                    mock_on_toggle_pin: pin,
                    mock_on_request_decode: request,
                })}
            }
        }
    }

    #[wasm_bindgen_test]
    async fn a_video_off_tile_shows_the_avatar_under_app_drawn_chrome() {
        install_fetch("return jpeg();");
        DECODE_REQUESTS.with(|r| r.borrow_mut().clear());
        let mount = mount(VideoOffTiles);
        let off = "#peer-video-11-div";
        let paused = "#peer-video-12-div";
        let both = wait_until(|| {
            src_of(&mount, "peer-video-11-div").starts_with("blob:")
                && src_of(&mount, "peer-video-12-div").starts_with("blob:")
        })
        .await;
        let img_alt = attr(
            &mount,
            &format!("{off} .placeholder-content img.peer-avatar"),
            "alt",
        );
        let img_title = attr(&mount, &format!("{off} img.peer-avatar"), "title");
        let label = query(
            &mount,
            &format!("{off} .placeholder-content .placeholder-text"),
        )
        .and_then(|el| el.text_content());
        let crown = query(&mount, &format!("{off} .floating-name .host-indicator")).is_some();
        let name =
            query(&mount, &format!("{off} .floating-name-text")).and_then(|e| e.text_content());
        let play_in_slot = query(
            &mount,
            &format!("{paused} .placeholder-content--paused .peer-avatar-slot [data-testid='decode-play-btn']"),
        )
        .is_some();
        let paused_label =
            query(&mount, &format!("{paused} .placeholder-text")).and_then(|e| e.text_content());
        click(&mount, &format!("{paused} [data-testid='decode-play-btn']"));
        settle().await;
        let decodes = DECODE_REQUESTS.with(|r| r.borrow().clone());
        let unknown = attr(
            &mount,
            "#peer-video-99-div .placeholder-content .peer-avatar-slot",
            "data-avatar-state",
        );
        let fetched: Vec<_> = calls_to("/meetings/m1/avatar")
            .iter()
            .map(|c| c["user_id"].as_str().unwrap_or_default().to_string())
            .collect();
        restore_fetch();
        mount.remove();
        assert!(both, "both video-off tiles render their avatar");
        assert_eq!(img_alt.as_deref(), Some(""), "the avatar is decorative");
        assert_eq!(img_title, None, "no tooltip on the avatar");
        assert_eq!(
            label.as_deref(),
            Some("Video Disabled"),
            "the state label stays"
        );
        assert!(crown, "the host crown still renders beside the name");
        assert_eq!(name.as_deref(), Some("ann@example.com"));
        assert!(
            play_in_slot,
            "PLAY sits on the avatar slot of a budget-paused tile"
        );
        assert_eq!(paused_label.as_deref(), Some("Video paused"));
        assert_eq!(decodes, ["12"], "PLAY still asks for the tile's video");
        assert_eq!(
            unknown.as_deref(),
            Some("default"),
            "a tile without a server-asserted user id shows the default icon"
        );
        assert_eq!(fetched.len(), 2, "no lookup by session key: {fetched:?}");
        assert!(fetched.contains(&"ann@example.com".to_string()));
        assert!(fetched.contains(&"bea@example.com".to_string()));
    }

    thread_local! {
        static SELF_URL: RefCell<String> = const { RefCell::new(String::new()) };
        static SELF_GUEST: Cell<bool> = const { Cell::new(false) };
    }

    #[allow(non_snake_case)]
    fn SelfViews() -> Element {
        let own = use_signal(|| OwnAvatar {
            load: OwnAvatarLoad::Loaded,
            image_url: Some(SELF_URL.with(|u| u.borrow().clone())),
            ..OwnAvatar::default()
        });
        use_context_provider(|| OwnAvatarCtx(own));
        use_provide_peer_avatar_cache(meeting_cache(SELF_GUEST.with(Cell::get)));
        rsx! {
            div { id: "self-grid", SelfAvatar {} }
            div { id: "self-corner",
                SelfAvatarOrIcon { svg { class: "camera-off-glyph" } }
            }
        }
    }

    thread_local! {
        static SELF_SHOWN: RefCell<Option<Signal<bool>>> = const { RefCell::new(None) };
    }

    #[allow(non_snake_case)]
    fn HideableSelfView() -> Element {
        use_provide_peer_avatar_cache(meeting_cache(false));
        let shown = use_signal(|| true);
        SELF_SHOWN.with(|s| *s.borrow_mut() = Some(shown));
        rsx! {
            if shown() {
                div { id: "self-hideable", SelfAvatar {} }
            }
        }
    }

    fn show_self_view(on: bool) {
        let mut shown = SELF_SHOWN.with(|s| s.borrow().unwrap());
        shown.set(on);
    }

    #[wasm_bindgen_test]
    async fn the_own_avatar_is_kept_while_the_self_view_is_hidden_past_the_grace_period() {
        let _timings = TimingsGuard::set(super::AvatarTimings {
            prune_grace_ms: 300,
            ..AVATAR_TIMINGS
        });
        install_fetch("return jpeg();");
        let mount = mount(HideableSelfView);
        let shown = wait_until(|| src_of(&mount, "self-hideable").starts_with("blob:")).await;
        let url = src_of(&mount, "self-hideable");
        show_self_view(false);
        TimeoutFuture::new(1_500).await;
        let released = revoked_urls().contains(&url);
        show_self_view(true);
        let back = wait_until(|| query(&mount, "#self-hideable .peer-avatar-slot").is_some()).await;
        let state = state_of(&mount, "self-hideable");
        let src = src_of(&mount, "self-hideable");
        let lookups = meeting_lookups();
        restore_fetch();
        mount.remove();
        assert!(shown && back);
        assert!(
            !released,
            "the own image is not released while it is hidden"
        );
        assert_eq!(
            state.as_deref(),
            Some("image"),
            "no default icon or loading state on return"
        );
        assert_eq!(src, url);
        assert_eq!(lookups, vec!["me@example.com".to_string()]);
    }

    fn meeting_lookups() -> Vec<String> {
        calls()
            .into_iter()
            .filter(|call| {
                call["path"]
                    .as_str()
                    .is_some_and(|p| p.contains("/meetings/"))
            })
            .filter_map(|call| call["user_id"].as_str().map(str::to_string))
            .collect()
    }

    #[wasm_bindgen_test]
    async fn the_self_view_shows_the_meeting_users_avatar_only_for_a_signed_in_user() {
        install_fetch("return call.path.indexOf('/meetings/') >= 0 ? jpeg() : json(500, {});");
        let browser_account_url = real_jpeg_url().await;
        SELF_URL.with(|u| *u.borrow_mut() = browser_account_url.clone());
        SELF_GUEST.with(|g| g.set(false));
        let signed_in = mount(SelfViews);
        let grid_src = || {
            attr(
                &signed_in,
                "#self-grid [data-testid='self-avatar-img']",
                "src",
            )
            .unwrap_or_default()
        };
        let shown = wait_until(|| grid_src().starts_with("blob:")).await;
        let grid = grid_src();
        let corner = attr(
            &signed_in,
            "#self-corner [data-testid='self-avatar-img']",
            "src",
        );
        let lookups = meeting_lookups();
        signed_in.remove();
        SELF_GUEST.with(|g| g.set(true));
        let guest = mount(SelfViews);
        settle().await;
        let guest_grid = query(&guest, "#self-grid img").is_some();
        let guest_corner = query(&guest, "#self-corner .camera-off-glyph").is_some();
        restore_fetch();
        guest.remove();
        assert!(shown);
        assert_ne!(
            grid, browser_account_url,
            "the browser account's image is not the meeting user's"
        );
        assert_eq!(corner.as_deref(), Some(grid.as_str()));
        assert_eq!(lookups, vec!["me@example.com".to_string()]);
        assert!(!guest_grid, "a guest's self view keeps the default icon");
        assert!(
            guest_corner,
            "a guest's corner tile keeps the camera-off glyph"
        );
    }

    thread_local! {
        static CHANGE_CACHE: RefCell<Option<PeerAvatarCacheCtx>> = const { RefCell::new(None) };
        static RAY_SHOWN: RefCell<Option<Signal<bool>>> = const { RefCell::new(None) };
    }

    /// Each lookup answers with `jpeg(window.__avatarColor)`, or 503 while `__avatarDown`, as
    /// those stood when it was sent; held while `__avatarHold`.
    const SERVED_AS_SENT: &str = "\
        var color = window.__avatarColor; var down = window.__avatarDown; \
        var reply = function() { return down ? json(503, {}) : jpeg(color); }; \
        return window.__avatarHold ? hold(reply) : reply();";

    fn serve_as_sent() {
        install_fetch(SERVED_AS_SENT);
        js("window.__avatarColor = undefined; window.__avatarDown = false;");
    }

    fn no_refresh_jitter() -> TimingsGuard {
        TimingsGuard::set(super::AvatarTimings {
            refresh_jitter_ms: 0,
            ..AVATAR_TIMINGS
        })
    }

    /// Deliver an avatar change for `user_id` the way the call client does.
    fn announce_change(user_id: &str) {
        let cache = CHANGE_CACHE.with(|c| c.borrow().clone().unwrap());
        avatar_change_listener(Some(&cache))
            .unwrap()
            .emit(user_id.to_string());
    }

    #[allow(non_snake_case)]
    fn ChangingPeer() -> Element {
        let cache = use_provide_peer_avatar_cache(meeting_cache(false));
        CHANGE_CACHE.with(|c| *c.borrow_mut() = Some(cache));
        let shown = use_signal(|| true);
        RAY_SHOWN.with(|s| *s.borrow_mut() = Some(shown));
        rsx! {
            if shown() {
                div { id: "pa-ray",
                    PeerAvatar { session_id: "21".to_string(), user_id: Some("ray@example.com".to_string()) }
                }
            }
        }
    }

    fn show_ray(on: bool) {
        let mut shown = RAY_SHOWN.with(|s| s.borrow().unwrap());
        shown.set(on);
    }

    fn ray_lookups() -> usize {
        calls_to("/meetings/m1/avatar").len()
    }

    fn pending() -> f64 {
        js("return window.__avatarPending;")
            .as_f64()
            .unwrap_or(-1.0)
    }

    fn release_first_held() -> bool {
        js("var reply = (window.__avatarHeld || []).shift(); if (reply) { reply(); } return !!reply;")
            .as_bool()
            .unwrap_or(false)
    }

    #[wasm_bindgen_test]
    async fn an_announced_avatar_change_replaces_the_image_a_tile_shows() {
        let _timings = no_refresh_jitter();
        serve_as_sent();
        let mount = mount(ChangingPeer);
        let shown = wait_until(|| src_of(&mount, "pa-ray").starts_with("blob:")).await;
        let old = src_of(&mount, "pa-ray");
        js("window.__avatarColor = 'red';");
        settle().await;
        let unannounced = (ray_lookups(), src_of(&mount, "pa-ray"));
        announce_change("ray@example.com");
        let replaced = wait_until(|| {
            let src = src_of(&mount, "pa-ray");
            src.starts_with("blob:") && src != old
        })
        .await;
        let released = wait_until(|| revoked_urls().contains(&old)).await;
        let lookups = ray_lookups();
        restore_fetch();
        mount.remove();
        assert!(shown);
        assert_eq!(unannounced, (1, old), "nothing re-reads it unannounced");
        assert!(replaced, "the announced change is read and shown");
        assert!(released, "the replaced image's URL is released");
        assert_eq!(lookups, 2);
    }

    #[wasm_bindgen_test]
    async fn a_change_announced_while_the_tile_is_hidden_is_read_when_it_returns() {
        let _timings = no_refresh_jitter();
        serve_as_sent();
        let mount = mount(ChangingPeer);
        let shown = wait_until(|| src_of(&mount, "pa-ray").starts_with("blob:")).await;
        let old = src_of(&mount, "pa-ray");
        show_ray(false);
        settle().await;
        js("window.__avatarColor = 'red';");
        announce_change("ray@example.com");
        settle().await;
        settle().await;
        let while_hidden = ray_lookups();
        show_ray(true);
        let reread = wait_until(|| ray_lookups() == 2).await;
        let replaced = wait_until(|| {
            let src = src_of(&mount, "pa-ray");
            src.starts_with("blob:") && src != old
        })
        .await;
        restore_fetch();
        mount.remove();
        assert!(shown);
        assert_eq!(while_hidden, 1, "nothing is fetched for a tile no one sees");
        assert!(reread, "the returning tile reads the changed avatar");
        assert!(replaced);
    }

    #[wasm_bindgen_test]
    async fn a_tile_back_from_hidden_still_reads_later_changes() {
        let _timings = no_refresh_jitter();
        serve_as_sent();
        let mount = mount(ChangingPeer);
        let shown = wait_until(|| src_of(&mount, "pa-ray").starts_with("blob:")).await;
        let old = src_of(&mount, "pa-ray");
        show_ray(false);
        settle().await;
        js("window.__avatarColor = 'red';");
        announce_change("ray@example.com");
        show_ray(true);
        let returned = wait_until(|| {
            let src = src_of(&mount, "pa-ray");
            src.starts_with("blob:") && src != old
        })
        .await;
        let red = src_of(&mount, "pa-ray");
        js("window.__avatarColor = 'blue';");
        announce_change("ray@example.com");
        let reread = wait_until(|| ray_lookups() == 3).await;
        let replaced = wait_until(|| {
            let src = src_of(&mount, "pa-ray");
            src.starts_with("blob:") && src != red
        })
        .await;
        restore_fetch();
        mount.remove();
        assert!(shown && returned);
        assert!(
            reread,
            "a change announced after the tile returned is read too"
        );
        assert!(replaced, "and its image replaces the one read on return");
    }

    #[wasm_bindgen_test]
    async fn a_returning_tiles_failed_reread_is_retried_until_the_change_lands() {
        let _timings = TimingsGuard::set(super::AvatarTimings {
            refresh_jitter_ms: 0,
            ..fast_retry()
        });
        serve_as_sent();
        let mount = mount(ChangingPeer);
        let shown = wait_until(|| src_of(&mount, "pa-ray").starts_with("blob:")).await;
        let old = src_of(&mount, "pa-ray");
        show_ray(false);
        settle().await;
        js("window.__avatarColor = 'red'; window.__avatarDown = true; window.__avatarHold = true;");
        announce_change("ray@example.com");
        show_ray(true);
        let reread = wait_until(|| ray_lookups() == 2).await;
        js("window.__avatarDown = false; window.__avatarHold = false;");
        let failed = release_first_held();
        let retried = wait_until(|| ray_lookups() == 3).await;
        let replaced = wait_until(|| {
            let src = src_of(&mount, "pa-ray");
            src.starts_with("blob:") && src != old
        })
        .await;
        restore_fetch();
        mount.remove();
        assert!(shown && reread && failed);
        assert!(
            retried,
            "the returning tile's failed re-read is tried again"
        );
        assert!(replaced, "and the changed avatar is shown");
    }

    #[wasm_bindgen_test]
    async fn a_change_announced_while_hidden_outranks_a_reread_still_in_flight() {
        let _timings = no_refresh_jitter();
        serve_as_sent();
        let mount = mount(ChangingPeer);
        let shown = wait_until(|| src_of(&mount, "pa-ray").starts_with("blob:")).await;
        let old = src_of(&mount, "pa-ray");
        js("window.__avatarHold = true; window.__avatarColor = 'red';");
        announce_change("ray@example.com");
        let first_sent = wait_until(|| ray_lookups() == 2).await;
        show_ray(false);
        settle().await;
        js("window.__avatarColor = 'blue';");
        announce_change("ray@example.com");
        show_ray(true);
        let reread = wait_until(|| ray_lookups() == 3).await;
        let earlier_released = release_first_held();
        let earlier_answered = wait_until(|| pending() == 1.0).await;
        settle().await;
        settle().await;
        let after_earlier = src_of(&mount, "pa-ray");
        let latest_released = release_first_held();
        let replaced = wait_until(|| {
            let src = src_of(&mount, "pa-ray");
            src.starts_with("blob:") && src != old
        })
        .await;
        restore_fetch();
        mount.remove();
        assert!(shown && first_sent);
        assert!(
            reread,
            "the returning tile reads again although an older read is still in flight"
        );
        assert!(earlier_released && earlier_answered && latest_released);
        assert_eq!(after_earlier, old, "the older read is not shown");
        assert!(replaced, "the read made on return is shown");
    }

    #[wasm_bindgen_test]
    async fn changes_announced_before_the_reread_is_sent_share_one_lookup() {
        let _timings = no_refresh_jitter();
        serve_as_sent();
        let mount = mount(ChangingPeer);
        let shown = wait_until(|| src_of(&mount, "pa-ray").starts_with("blob:")).await;
        let old = src_of(&mount, "pa-ray");
        js("window.__avatarColor = 'red';");
        announce_change("ray@example.com");
        announce_change("ray@example.com");
        let replaced = wait_until(|| {
            let src = src_of(&mount, "pa-ray");
            src.starts_with("blob:") && src != old
        })
        .await;
        settle().await;
        settle().await;
        let lookups = ray_lookups();
        restore_fetch();
        mount.remove();
        assert!(shown && replaced);
        assert_eq!(lookups, 2, "one re-read serves both announcements");
    }

    #[wasm_bindgen_test]
    async fn a_change_announced_while_a_reread_is_in_flight_is_read_again_and_wins() {
        let _timings = no_refresh_jitter();
        serve_as_sent();
        let mount = mount(ChangingPeer);
        let shown = wait_until(|| src_of(&mount, "pa-ray").starts_with("blob:")).await;
        let old = src_of(&mount, "pa-ray");
        js("window.__avatarHold = true; window.__avatarColor = 'red';");
        announce_change("ray@example.com");
        let first_sent = wait_until(|| ray_lookups() == 2).await;
        js("window.__avatarColor = 'blue';");
        announce_change("ray@example.com");
        let second_sent = wait_until(|| ray_lookups() == 3).await;
        let earlier_released = release_first_held();
        let earlier_answered = wait_until(|| pending() == 1.0).await;
        settle().await;
        settle().await;
        let after_earlier = src_of(&mount, "pa-ray");
        let latest_released = release_first_held();
        let replaced = wait_until(|| {
            let src = src_of(&mount, "pa-ray");
            src.starts_with("blob:") && src != old
        })
        .await;
        let lookups = ray_lookups();
        restore_fetch();
        mount.remove();
        assert!(shown && first_sent);
        assert!(
            second_sent,
            "a re-read already sent may hold the older image, so the newer change is read too"
        );
        assert!(earlier_released && earlier_answered && latest_released);
        assert_eq!(
            after_earlier, old,
            "the answer read before the latest change is not shown"
        );
        assert!(replaced, "the latest change is shown");
        assert_eq!(lookups, 3);
    }

    #[wasm_bindgen_test]
    async fn a_failed_reread_keeps_the_image_the_tile_shows() {
        let _timings = no_refresh_jitter();
        serve_as_sent();
        let mount = mount(ChangingPeer);
        let shown = wait_until(|| src_of(&mount, "pa-ray").starts_with("blob:")).await;
        let old = src_of(&mount, "pa-ray");
        js("window.__avatarDown = true;");
        announce_change("ray@example.com");
        let failed = wait_until(|| ray_lookups() == 2 && pending() == 0.0).await;
        settle().await;
        settle().await;
        let state = state_of(&mount, "pa-ray");
        let src = src_of(&mount, "pa-ray");
        restore_fetch();
        mount.remove();
        assert!(shown && failed);
        assert_eq!(state.as_deref(), Some("image"));
        assert_eq!(src, old, "a failed re-read keeps the image shown");
    }

    #[allow(non_snake_case)]
    fn ChangingPeerThatEnds() -> Element {
        let own = use_signal(OwnAvatar::default);
        use_context_provider(|| OwnAvatarCtx(own));
        let in_meeting = use_signal(|| true);
        IN_MEETING.with(|s| *s.borrow_mut() = Some(in_meeting));
        rsx! {
            if in_meeting() {
                ChangingPeer {}
            }
        }
    }

    #[wasm_bindgen_test]
    async fn an_announcement_for_no_tile_or_after_the_meeting_fetches_nothing() {
        let _timings = no_refresh_jitter();
        serve_as_sent();
        let mount = mount(ChangingPeerThatEnds);
        let shown = wait_until(|| src_of(&mount, "pa-ray").starts_with("blob:")).await;
        settle().await;
        let before = calls().len();
        announce_change("nobody@example.com");
        settle().await;
        settle().await;
        let after_unknown = calls().len();
        let cache = CHANGE_CACHE.with(|c| c.borrow_mut().take().unwrap());
        let listener = avatar_change_listener(Some(&cache)).unwrap();
        let mut in_meeting = IN_MEETING.with(|s| s.borrow().unwrap());
        in_meeting.set(false);
        settle().await;
        listener.emit("ray@example.com".to_string());
        listener.emit("me@example.com".to_string());
        cache.refresh_user("ray@example.com");
        cache.refresh_user("me@example.com");
        settle().await;
        settle().await;
        let after_teardown = calls().len();
        restore_fetch();
        mount.remove();
        assert!(shown);
        assert_eq!(after_unknown, before, "no tile, no lookup");
        assert_eq!(
            after_teardown, before,
            "nothing is read once the meeting is gone"
        );
    }

    #[allow(non_snake_case)]
    fn JitteredPeers() -> Element {
        let cache = use_provide_peer_avatar_cache(meeting_cache(false));
        CHANGE_CACHE.with(|c| *c.borrow_mut() = Some(cache));
        let fifth = use_signal(|| false);
        RAY_SHOWN.with(|s| *s.borrow_mut() = Some(fifth));
        rsx! {
            for n in 1..5 {
                div { key: "{n}", id: "pa-j{n}",
                    PeerAvatar { session_id: format!("j{n}"), user_id: Some(format!("j{n}@example.com")) }
                }
            }
            div { id: "pa-me",
                PeerAvatar { session_id: "j0".to_string(), user_id: Some("me@example.com".to_string()) }
            }
            if fifth() {
                div { id: "pa-j5",
                    PeerAvatar { session_id: "j5".to_string(), user_id: Some("j5@example.com".to_string()) }
                }
            }
        }
    }

    fn looked_up_users() -> Vec<String> {
        calls_to("/meetings/m1/avatar")
            .iter()
            .map(|c| c["user_id"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    #[wasm_bindgen_test]
    async fn a_peers_reread_waits_out_its_jitter_without_holding_a_lookup_slot() {
        let _timings = TimingsGuard::set(super::AvatarTimings {
            refresh_jitter_ms: 800,
            ..AVATAR_TIMINGS
        });
        serve_as_sent();
        js("window.__origRandom = window.__origRandom || Math.random; \
            Math.random = function() { return 1; };");
        let mount = mount(JitteredPeers);
        let ids = ["pa-j1", "pa-j2", "pa-j3", "pa-j4", "pa-me"];
        let shown =
            wait_until(|| ids.iter().all(|id| src_of(&mount, id).starts_with("blob:"))).await;
        let initial = looked_up_users().len();
        for user in [
            "j1@example.com",
            "j2@example.com",
            "j3@example.com",
            "j4@example.com",
            "me@example.com",
        ] {
            announce_change(user);
        }
        show_ray(true);
        let unjittered = wait_until(|| {
            let later = &looked_up_users()[initial..];
            later.contains(&"me@example.com".to_string())
                && later.contains(&"j5@example.com".to_string())
        })
        .await;
        let mut early = looked_up_users()[initial..].to_vec();
        let all = wait_until(|| looked_up_users().len() == initial + 6).await;
        js("Math.random = window.__origRandom;");
        restore_fetch();
        mount.remove();
        early.sort();
        assert!(shown);
        assert_eq!(initial, 5);
        assert!(unjittered);
        assert_eq!(
            early,
            ["j5@example.com", "me@example.com"],
            "the own re-read and a new tile go out while four peer re-reads wait out their jitter"
        );
        assert!(all, "every jittered re-read is sent after its wait");
    }

    #[allow(non_snake_case)]
    fn ChangingPeerWithSecondSession() -> Element {
        let cache = use_provide_peer_avatar_cache(meeting_cache(false));
        CHANGE_CACHE.with(|c| *c.borrow_mut() = Some(cache));
        let second = use_signal(|| false);
        RAY_SHOWN.with(|s| *s.borrow_mut() = Some(second));
        rsx! {
            for n in 1..=MAX_AVATAR_FETCHES_IN_FLIGHT {
                div { key: "{n}", id: "pa-busy{n}",
                    PeerAvatar { session_id: format!("b{n}"), user_id: Some(format!("busy{n}@example.com")) }
                }
            }
            div { id: "pa-kim",
                PeerAvatar { session_id: "41".to_string(), user_id: Some("kim@example.com".to_string()) }
            }
            if second() {
                div { id: "pa-kim2",
                    PeerAvatar { session_id: "42".to_string(), user_id: Some("kim@example.com".to_string()) }
                }
            }
        }
    }

    fn lookups_for(user_id: &str) -> usize {
        looked_up_users()
            .iter()
            .filter(|user| *user == user_id)
            .count()
    }

    #[wasm_bindgen_test]
    async fn a_new_session_mounted_while_a_reread_is_queued_shares_its_lookup() {
        let _timings = no_refresh_jitter();
        serve_as_sent();
        let mount = mount(ChangingPeerWithSecondSession);
        let busy: Vec<String> = (1..=MAX_AVATAR_FETCHES_IN_FLIGHT)
            .map(|n| format!("busy{n}@example.com"))
            .collect();
        let shown = wait_until(|| {
            src_of(&mount, "pa-kim").starts_with("blob:")
                && (1..=MAX_AVATAR_FETCHES_IN_FLIGHT)
                    .all(|n| src_of(&mount, &format!("pa-busy{n}")).starts_with("blob:"))
        })
        .await;
        let old = src_of(&mount, "pa-kim");
        let busy_old: Vec<String> = (1..=MAX_AVATAR_FETCHES_IN_FLIGHT)
            .map(|n| src_of(&mount, &format!("pa-busy{n}")))
            .collect();
        js("window.__avatarHold = true; window.__avatarColor = 'red';");
        for user in &busy {
            announce_change(user);
        }
        let slots_taken = wait_until(|| busy.iter().all(|user| lookups_for(user) == 2)).await;
        announce_change("kim@example.com");
        show_ray(true);
        let second_shown = wait_until(|| src_of(&mount, "pa-kim2") == old).await;
        settle().await;
        let while_queued = lookups_for("kim@example.com");
        js("window.__avatarHold = false;");
        release_held();
        let replaced = wait_until(|| {
            let (first, second) = (src_of(&mount, "pa-kim"), src_of(&mount, "pa-kim2"));
            first.starts_with("blob:") && first != old && second == first
        })
        .await;
        let others_answered = wait_until(|| {
            busy_old.iter().enumerate().all(|(i, old)| {
                let src = src_of(&mount, &format!("pa-busy{}", i + 1));
                src.starts_with("blob:") && src != *old
            })
        })
        .await;
        settle().await;
        settle().await;
        let lookups = lookups_for("kim@example.com");
        restore_fetch();
        mount.remove();
        assert!(shown, "every tile shows its first image");
        assert!(
            slots_taken,
            "held re-reads of other peers take every lookup slot"
        );
        assert!(
            second_shown,
            "the new session's tile shows the image its user's slot holds"
        );
        assert_eq!(
            while_queued, 1,
            "the re-read is still queued when the new session mounts"
        );
        assert!(
            replaced,
            "the queued re-read's image is shown on both of the user's tiles"
        );
        assert!(
            others_answered,
            "the held re-reads are answered and give their lookup slots back"
        );
        assert_eq!(
            lookups, 2,
            "one lookup serves both the re-read and the new session"
        );
    }
}
