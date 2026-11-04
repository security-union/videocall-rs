// SPDX-License-Identifier: MIT OR Apache-2.0

//! Setting your own avatar (issue 3125): the Settings "Profile" panel, the
//! pre-join avatar button and the dialog it opens, and the self tile's avatar button.

use std::cell::RefCell;
use std::rc::Rc;

use dioxus::prelude::*;
use futures::channel::oneshot;
use futures::future::{select, Either};
use gloo_timers::callback::Timeout;
use gloo_timers::future::TimeoutFuture;
use videocall_meeting_client::avatars::AvatarImage as AvatarBytes;
use videocall_meeting_client::ApiError;
use videocall_meeting_types::avatar::{
    AvatarInfo, AvatarSource, ALLOWED_AVATAR_UPLOAD_CONTENT_TYPES, AVATAR_CLIENT_MAX_FILE_BYTES,
    AVATAR_OUTPUT_DIMENSION, MIN_AVATAR_DIMENSION,
};
use videocall_meeting_types::{APIError, APIResponse};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

use crate::components::attendants::action_bar_announce_text;
use crate::components::avatar::{
    avatar_editing_allowed, avatar_timings, bytes_blob, fetch_my_avatar_image, recheck_own_avatar,
    refresh_own_avatar, resync_own_avatar, retire_object_url, set_own_avatar, use_own_avatar,
    user_client, with_user_refresh, AvatarAction, AvatarImage, AvatarView, OwnAvatar, OwnAvatarCtx,
    OwnAvatarLoad, OwnRead, OwnReadFailure, PeerAvatarCacheCtx, SelfAvatar,
};
use crate::components::pre_join_settings_card::PREVIEW_CAMERA_TOGGLE_TESTID;

pub const UPLOAD_INPUT_ID: &str = "avatar-upload-input";
const REMOVE_BUTTON_ID: &str = "avatar-remove-button";
pub const PREJOIN_AVATAR_BUTTON_TESTID: &str = "prejoin-avatar-button";
pub const SELF_AVATAR_EDIT_BUTTON_TESTID: &str = "self-avatar-edit-button";
const ENCODE_QUALITY: f64 = 0.9;
const FOCUSABLE: &str = "button:not([disabled]), input:not([disabled]), [tabindex=\"0\"]";

pub const COPY_TOO_LARGE: &str = "That image is larger than 2 MB. Choose a smaller file.";
pub const COPY_UNSUPPORTED: &str = "That file type isn't supported. Use a PNG, JPEG or WebP image.";
pub const COPY_UNREADABLE: &str = "That image couldn't be read. Try a different file.";
pub const COPY_TOO_SMALL: &str = "That image is too small. Use one at least 16×16 px.";
pub const COPY_GRAVATAR_INVALID: &str =
    "Enter your Gravatar email or a link like https://gravatar.com/avatar/…";
pub const COPY_GRAVATAR_NOT_FOUND: &str = "No Gravatar image was found for that email or link.";
pub const COPY_GRAVATAR_UNUSABLE: &str =
    "That Gravatar image can't be used. Upload a PNG, JPEG or WebP image instead.";
pub const COPY_NETWORK: &str = "Couldn't reach the server. Check your connection and try again.";
pub const COPY_UNCONFIRMED: &str =
    "Couldn't confirm your change. If your avatar doesn't update in a moment, try again.";
pub const COPY_SAVE_FAILED: &str = "Couldn't save your avatar. Try again.";
pub const COPY_REMOVE_FAILED: &str = "Couldn't remove your avatar. Try again.";
pub const COPY_RATE_LIMITED: &str =
    "You've changed your avatar several times. Try again in a minute.";
pub const COPY_SIGNED_OUT: &str = "Your sign-in has expired. Sign in again to change your avatar.";
pub const COPY_SIGNED_IN_ELSEWHERE: &str = "This browser is now signed in to a different account, \
    so you can't change your avatar here. In another tab, sign in again with the account you're \
    using in this meeting, then select Check again.";
const SIGNED_IN_ELSEWHERE_ID: &str = "avatar-signed-in-elsewhere";
const CHECK_AGAIN_TESTID: &str = "avatar-check-again";

const ANNOUNCE_UNLOCKED: &str = "You can change your avatar again.";
const ANNOUNCE_UPLOADED: &str = "Avatar updated.";
const ANNOUNCE_UPLOADED_SMALL: &str =
    "Avatar updated. This image is smaller than 256×256 px and may look blurry.";
const ANNOUNCE_GRAVATAR: &str = "Gravatar image saved.";
const ANNOUNCE_REMOVED: &str = "Avatar removed. Others will see the default icon.";

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum AvatarFailure {
    TooLarge,
    TooSmall,
    UnsupportedType,
    Unreadable,
    Unauthorized,
    RateLimited,
    Network,
    Unconfirmed,
    SignedInElsewhere,
    Server(Option<String>),
}

pub fn classify_api_error(err: &ApiError) -> AvatarFailure {
    match err {
        ApiError::NotAuthenticated => AvatarFailure::Unauthorized,
        ApiError::RateLimitExceeded => AvatarFailure::RateLimited,
        ApiError::IdentityMismatch => AvatarFailure::SignedInElsewhere,
        ApiError::Network(_) => AvatarFailure::Network,
        ApiError::ServerError { body, .. }
        | ApiError::NotFound(body)
        | ApiError::Forbidden(body) => AvatarFailure::Server(
            serde_json::from_str::<APIResponse<APIError>>(body)
                .ok()
                .map(|envelope| envelope.result.code),
        ),
        _ => AvatarFailure::Server(None),
    }
}

pub fn avatar_error_copy(action: AvatarAction, failure: &AvatarFailure) -> &'static str {
    let code = match failure {
        AvatarFailure::TooLarge => return COPY_TOO_LARGE,
        AvatarFailure::TooSmall => return COPY_TOO_SMALL,
        AvatarFailure::UnsupportedType => return COPY_UNSUPPORTED,
        AvatarFailure::Unreadable => return COPY_UNREADABLE,
        AvatarFailure::Unauthorized => return COPY_SIGNED_OUT,
        AvatarFailure::RateLimited => return COPY_RATE_LIMITED,
        AvatarFailure::Network => return COPY_NETWORK,
        AvatarFailure::Unconfirmed => return COPY_UNCONFIRMED,
        AvatarFailure::SignedInElsewhere => return COPY_SIGNED_IN_ELSEWHERE,
        AvatarFailure::Server(code) => code.as_deref().unwrap_or_default(),
    };
    match (action, code) {
        (AvatarAction::Remove, _) => COPY_REMOVE_FAILED,
        (AvatarAction::Upload, "UNSUPPORTED_MEDIA_TYPE") => COPY_UNSUPPORTED,
        (AvatarAction::Upload, "INVALID_IMAGE" | "PAYLOAD_TOO_LARGE") => COPY_UNREADABLE,
        (AvatarAction::Gravatar, "INVALID_GRAVATAR" | "INVALID_INPUT") => COPY_GRAVATAR_INVALID,
        (AvatarAction::Gravatar, "GRAVATAR_NOT_FOUND") => COPY_GRAVATAR_NOT_FOUND,
        (AvatarAction::Gravatar, "INVALID_IMAGE") => COPY_GRAVATAR_UNUSABLE,
        _ => COPY_SAVE_FAILED,
    }
}

/// Type and size checks on a picked file, before it is decoded.
pub fn validate_avatar_file(mime: &str, name: &str, size: f64) -> Result<(), AvatarFailure> {
    let mime = mime.trim().to_ascii_lowercase();
    let supported = if mime.is_empty() {
        let name = name.to_ascii_lowercase();
        [".png", ".jpg", ".jpeg", ".webp"]
            .iter()
            .any(|ext| name.ends_with(ext))
    } else {
        ALLOWED_AVATAR_UPLOAD_CONTENT_TYPES.contains(&mime.as_str())
    };
    if !supported {
        return Err(AvatarFailure::UnsupportedType);
    }
    if size > AVATAR_CLIENT_MAX_FILE_BYTES as f64 {
        return Err(AvatarFailure::TooLarge);
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CropPlan {
    pub sx: u32,
    pub sy: u32,
    pub side: u32,
    pub out: u32,
}

/// The centered square to cut from a `width`×`height` image, and its output side.
pub fn crop_plan(width: u32, height: u32) -> Option<CropPlan> {
    let side = width.min(height);
    if side == 0 {
        return None;
    }
    Some(CropPlan {
        sx: (width - side) / 2,
        sy: (height - side) / 2,
        side,
        out: side.min(AVATAR_OUTPUT_DIMENSION),
    })
}

pub fn avatar_source_text(own: &OwnAvatar) -> &'static str {
    match own.load {
        OwnAvatarLoad::NotLoaded | OwnAvatarLoad::Loading => "Loading your avatar…",
        OwnAvatarLoad::Failed => "Couldn't load your avatar.",
        OwnAvatarLoad::Loaded => match own.info.as_ref().map(|info| info.source) {
            None => "No avatar set. Others see the default icon.",
            Some(AvatarSource::Upload) => "Using an uploaded image.",
            Some(AvatarSource::Gravatar) => "Using your Gravatar image.",
        },
    }
}

pub fn is_tab_switch_key(key: &Key) -> bool {
    matches!(
        key,
        Key::ArrowLeft | Key::ArrowRight | Key::ArrowUp | Key::ArrowDown
    )
}

struct ProcessedAvatar {
    bytes: Vec<u8>,
    mime: &'static str,
    small: bool,
}

async fn canvas_blob(canvas: &web_sys::HtmlCanvasElement, mime: &str) -> Option<web_sys::Blob> {
    let mut requested: Result<(), JsValue> = Ok(());
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        requested = canvas.to_blob_with_type_and_encoder_options(
            &resolve,
            mime,
            &JsValue::from_f64(ENCODE_QUALITY),
        );
    });
    requested.ok()?;
    JsFuture::from(promise).await.ok()?.dyn_into().ok()
}

async fn encode_square(
    bitmap: &web_sys::ImageBitmap,
    plan: CropPlan,
) -> Result<ProcessedAvatar, AvatarFailure> {
    let canvas: web_sys::HtmlCanvasElement = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.create_element("canvas").ok())
        .and_then(|el| el.dyn_into().ok())
        .ok_or(AvatarFailure::Unreadable)?;
    canvas.set_width(plan.out);
    canvas.set_height(plan.out);
    let context: web_sys::CanvasRenderingContext2d = canvas
        .get_context("2d")
        .ok()
        .flatten()
        .and_then(|c| c.dyn_into().ok())
        .ok_or(AvatarFailure::Unreadable)?;
    let _ = js_sys::Reflect::set(&context, &"imageSmoothingQuality".into(), &"high".into());
    context.set_fill_style_str(crate::theme::color::PREVIEW_AVATAR_BG);
    context.fill_rect(0.0, 0.0, f64::from(plan.out), f64::from(plan.out));
    context
        .draw_image_with_image_bitmap_and_sw_and_sh_and_dx_and_dy_and_dw_and_dh(
            bitmap,
            f64::from(plan.sx),
            f64::from(plan.sy),
            f64::from(plan.side),
            f64::from(plan.side),
            0.0,
            0.0,
            f64::from(plan.out),
            f64::from(plan.out),
        )
        .map_err(|_| AvatarFailure::Unreadable)?;
    for mime in ["image/webp", "image/jpeg"] {
        let Some(blob) = canvas_blob(&canvas, mime).await else {
            continue;
        };
        if blob.type_() != mime {
            continue;
        }
        let buffer = JsFuture::from(blob.array_buffer())
            .await
            .map_err(|_| AvatarFailure::Unreadable)?;
        return Ok(ProcessedAvatar {
            bytes: js_sys::Uint8Array::new(&buffer).to_vec(),
            mime,
            small: plan.side < AVATAR_OUTPUT_DIMENSION,
        });
    }
    Err(AvatarFailure::Unreadable)
}

async fn process_avatar_file(file: &web_sys::File) -> Result<ProcessedAvatar, AvatarFailure> {
    let promise = web_sys::window()
        .ok_or(AvatarFailure::Unreadable)?
        .create_image_bitmap_with_blob(file)
        .map_err(|_| AvatarFailure::Unreadable)?;
    let bitmap: web_sys::ImageBitmap = JsFuture::from(promise)
        .await
        .ok()
        .and_then(|value| value.dyn_into().ok())
        .ok_or(AvatarFailure::Unreadable)?;
    let processed = match crop_plan(bitmap.width(), bitmap.height()) {
        Some(plan) if plan.side < MIN_AVATAR_DIMENSION => Err(AvatarFailure::TooSmall),
        Some(plan) => encode_square(&bitmap, plan).await,
        None => Err(AvatarFailure::Unreadable),
    };
    bitmap.close();
    processed
}

/// `request`'s result, or `Unconfirmed` when it has not settled within the save timeout.
async fn within_save_timeout<T>(
    request: impl std::future::Future<Output = Result<T, ApiError>>,
) -> Result<T, AvatarFailure> {
    let request = std::pin::pin!(request);
    let timeout = TimeoutFuture::new(avatar_timings().save_timeout_ms);
    match select(request, timeout).await {
        Either::Left((result, _)) => result.map_err(|e| classify_api_error(&e)),
        Either::Right(_) => Err(AvatarFailure::Unconfirmed),
    }
}

/// Whether the server's avatar shows that `action` took effect; `before_etag` is the avatar
/// the user had when they started it.
pub fn change_landed(
    action: AvatarAction,
    before_etag: Option<&str>,
    server: Option<&AvatarInfo>,
) -> bool {
    let replaced_with = |source: AvatarSource| {
        server.is_some_and(|info| info.source == source && Some(info.etag.as_str()) != before_etag)
    };
    match action {
        AvatarAction::Upload => replaced_with(AvatarSource::Upload),
        AvatarAction::Gravatar => replaced_with(AvatarSource::Gravatar),
        AvatarAction::Remove => server.is_none(),
    }
}

/// A save whose outcome may be unknown: what was tried, and where to report that it landed.
struct Attempt {
    action: AvatarAction,
    before_etag: Option<String>,
    success: &'static str,
    landed: oneshot::Sender<&'static str>,
}

/// After a save whose outcome is unknown, show whatever the server now holds, and send the
/// attempt's success message when that shows the change landed.
async fn resync_if_unconfirmed(
    unconfirmed: bool,
    own: Option<OwnAvatarCtx>,
    cache: Option<PeerAvatarCacheCtx>,
    attempt: Attempt,
) {
    let Some(own) = own.filter(|_| unconfirmed) else {
        return;
    };
    let Some(server) = resync_own_avatar(own, cache).await else {
        return;
    };
    if change_landed(
        attempt.action,
        attempt.before_etag.as_deref(),
        server.as_ref(),
    ) {
        let _ = attempt.landed.send(attempt.success);
    }
}

fn described_by(ids: &[Option<&str>]) -> String {
    ids.iter().flatten().copied().collect::<Vec<_>>().join(" ")
}

fn meeting_user(cache: &Option<PeerAvatarCacheCtx>) -> Option<String> {
    cache.as_ref().and_then(PeerAvatarCacheCtx::meeting_user_id)
}

fn signed_in_elsewhere(cache: &Option<PeerAvatarCacheCtx>) -> bool {
    cache
        .as_ref()
        .is_some_and(PeerAvatarCacheCtx::signed_in_elsewhere)
}

fn note_refused_write<T>(cache: &Option<PeerAvatarCacheCtx>, outcome: &Result<T, AvatarFailure>) {
    if let (Some(cache), Err(AvatarFailure::SignedInElsewhere)) = (cache, outcome) {
        cache.set_signed_in_elsewhere(true);
        cache.renotify_signed_in_elsewhere();
    }
}

fn own_etag(own: Signal<OwnAvatar>) -> Option<String> {
    own.try_peek()
        .ok()
        .and_then(|current| current.info.as_ref().map(|info| info.etag.clone()))
}

/// Publish a saved avatar to the own view and to this user's tiles in the meeting; `named` is
/// the user the write named as `expected_user_id`.
async fn commit_saved_avatar(
    own: Option<OwnAvatarCtx>,
    cache: Option<PeerAvatarCacheCtx>,
    named: Option<String>,
    info: AvatarInfo,
    uploaded: Option<AvatarBytes>,
) {
    let image = match cache.filter(|cache| cache.meeting_user_id().is_some()) {
        Some(cache) => {
            if let (Some(user_id), Some(image)) = (named.as_deref(), uploaded.as_ref()) {
                paint_named_user(&cache, user_id, Some(image));
            }
            cache.reload_meeting_user();
            None
        }
        None => match within_save_timeout(fetch_my_avatar_image()).await {
            Ok(Some(image)) => Some(image),
            _ => uploaded,
        },
    };
    if let Some(own) = own {
        set_own_avatar(own.0, Some(info), image.as_ref());
    }
}

/// Show `image` on the tiles of `user_id`, whom a successful write named, once a read of the
/// browser's account has returned its user id; `false` when it was not shown.
fn paint_named_user(
    cache: &PeerAvatarCacheCtx,
    user_id: &str,
    image: Option<&AvatarBytes>,
) -> bool {
    if !cache.server_named_account() {
        return false;
    }
    cache.set_user_image(user_id, image);
    true
}

fn id_has_focus(id: &str) -> bool {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|document| document.active_element())
        .is_some_and(|active| active.id() == id)
}

fn element_has_focus(document: &web_sys::Document, element: &web_sys::Element) -> bool {
    document
        .active_element()
        .is_some_and(|active| active.is_same_node(Some(element.as_ref())))
}

/// Focus `#id`; when that does not take, focus the first `fallback` match.
pub fn focus_with_fallback(id: &str, fallback: &str) {
    focus_first_taker(&[&format!("#{id}"), fallback]);
}

/// Try each selector's first DOM match in order; stop at the first that takes focus.
pub(crate) fn focus_first_taker(selectors: &[&str]) {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    for selector in selectors {
        let Some(element) = document.query_selector(selector).ok().flatten() else {
            continue;
        };
        if let Some(html) = element.dyn_ref::<web_sys::HtmlElement>() {
            let _ = html.focus();
        }
        if element_has_focus(&document, &element) {
            return;
        }
    }
}

/// `true` when focus is on Check again or on the page body.
fn focus_still_on_check_again() -> bool {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return false;
    };
    match document.active_element() {
        None => true,
        Some(active) => {
            document
                .body()
                .is_some_and(|body| active.is_same_node(Some(body.as_ref())))
                || active.get_attribute("data-testid").as_deref() == Some(CHECK_AGAIN_TESTID)
        }
    }
}

pub fn focus_prejoin_avatar_button() {
    focus_with_fallback(
        PREJOIN_AVATAR_BUTTON_TESTID,
        &format!("[data-testid='{PREVIEW_CAMERA_TOGGLE_TESTID}']"),
    );
}

fn set_busy(mut own: Signal<OwnAvatar>, busy: Option<AvatarAction>) {
    if let Ok(mut current) = own.try_write() {
        current.busy = busy;
    }
}

/// When `landed` reports that an unconfirmed change did land, drop the alert shown as
/// `shown` (unless a newer one replaced it) and announce the success message instead.
async fn announce_if_landed(
    landed: oneshot::Receiver<&'static str>,
    mut error: Signal<Option<&'static str>>,
    error_seq: Signal<u32>,
    shown: u32,
    mut announce: impl FnMut(&'static str),
) -> bool {
    let Ok(message) = landed.await else {
        return false;
    };
    if *error_seq.peek() != shown {
        return false;
    }
    error.set(None);
    announce(message);
    true
}

fn trap_tab_within(container: &str, shift: bool) -> bool {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return false;
    };
    let Some(scope) = document.query_selector(container).ok().flatten() else {
        return false;
    };
    let Ok(nodes) = scope.query_selector_all(FOCUSABLE) else {
        return false;
    };
    let count = nodes.length();
    let (Some(first), Some(last)) = (
        nodes
            .item(0)
            .and_then(|n| n.dyn_into::<web_sys::HtmlElement>().ok()),
        nodes
            .item(count.saturating_sub(1))
            .and_then(|n| n.dyn_into::<web_sys::HtmlElement>().ok()),
    ) else {
        return false;
    };
    let inside = document
        .active_element()
        .is_some_and(|active| scope.contains(Some(active.as_ref())));
    if shift && (!inside || element_has_focus(&document, first.as_ref())) {
        let _ = last.focus();
        return true;
    }
    if !shift && (!inside || element_has_focus(&document, last.as_ref())) {
        let _ = first.focus();
        return true;
    }
    false
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AvatarPanelVariant {
    InMeeting,
    Dialog,
}

#[component]
pub fn AvatarSettingsPanel(variant: AvatarPanelVariant, can_edit: bool) -> Element {
    let own_ctx = try_use_context::<OwnAvatarCtx>();
    let cache_ctx = try_use_context::<PeerAvatarCacheCtx>();
    let local_own = use_signal(OwnAvatar::default);
    let own_state = own_ctx.map_or(local_own, |own| own.0);
    let mut optimistic = use_signal(|| None::<String>);
    let mut upload_error = use_signal(|| None::<&'static str>);
    let mut upload_error_seq = use_signal(|| 0u32);
    let mut gravatar_input = use_signal(|| {
        own_ctx
            .and_then(|own| own.0.peek().info.as_ref()?.gravatar_url.clone())
            .unwrap_or_default()
    });
    let mut gravatar_error = use_signal(|| None::<&'static str>);
    let mut gravatar_error_seq = use_signal(|| 0u32);
    let mut announcement = use_signal(String::new);
    let mut announcement_nonce = use_signal(|| 0u32);
    let mut checking = use_signal(|| false);
    let mut was_elsewhere = use_hook(|| CopyValue::new(signed_in_elsewhere(&cache_ctx)));
    let mut focus_upload_when_clear = use_hook(|| CopyValue::new(false));

    use_effect({
        let cache_ctx = cache_ctx.clone();
        move || {
            let Some(own) = own_ctx.filter(|_| can_edit) else {
                return;
            };
            if meeting_user(&cache_ctx).is_some() {
                let cache = cache_ctx.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let _ = recheck_own_avatar(own, cache).await;
                });
            } else if matches!(
                own.0.peek().load,
                OwnAvatarLoad::NotLoaded | OwnAvatarLoad::Failed
            ) {
                refresh_own_avatar(own, cache_ctx.clone());
            }
        }
    });

    let mut announce = move |message: &'static str| {
        announcement.set(message.to_string());
        announcement_nonce += 1;
    };
    let mut show_upload_error = move |message: &'static str| {
        upload_error.set(Some(message));
        upload_error_seq += 1;
    };
    let fallback_focus = match variant {
        AvatarPanelVariant::InMeeting => "#settings-tab-profile",
        AvatarPanelVariant::Dialog => DIALOG_SELECTOR,
    };

    use_effect({
        let cache_ctx = cache_ctx.clone();
        move || {
            let now = signed_in_elsewhere(&cache_ctx);
            let before = std::mem::replace(&mut *was_elsewhere.write(), now);
            if before && !now {
                let unconfirmed = *upload_error.peek() == Some(COPY_UNCONFIRMED)
                    || *gravatar_error.peek() == Some(COPY_UNCONFIRMED);
                if let Some(cache) = cache_ctx.as_ref().filter(|_| unconfirmed) {
                    cache.reload_meeting_user();
                }
                upload_error.set(None);
                gravatar_error.set(None);
                announce(ANNOUNCE_UNLOCKED);
                if std::mem::take(&mut *focus_upload_when_clear.write())
                    && focus_still_on_check_again()
                {
                    focus_with_fallback(UPLOAD_INPUT_ID, fallback_focus);
                }
            }
        }
    });

    let (own_view, own_is_set) = use_own_avatar();
    let own = own_state.read().clone();
    let busy_now = own.busy;
    let elsewhere = signed_in_elsewhere(&cache_ctx);
    let notices = cache_ctx
        .as_ref()
        .map_or(0, PeerAvatarCacheCtx::signed_in_elsewhere_notices);
    let elsewhere_id = elsewhere.then_some(SIGNED_IN_ELSEWHERE_ID);
    let locked = busy_now.is_some() || elsewhere;
    let has_avatar = own_is_set == Some(true);
    let mut refocus_upload = use_hook(|| CopyValue::new(false));
    if !has_avatar && id_has_focus(REMOVE_BUTTON_ID) {
        refocus_upload.set(true);
    }
    use_effect(use_reactive!(|has_avatar| {
        let refocus = std::mem::take(&mut *refocus_upload.write());
        if refocus && !has_avatar {
            focus_with_fallback(UPLOAD_INPUT_ID, fallback_focus);
        }
    }));
    let upload_error_shown = upload_error().filter(|_| !elsewhere);
    let gravatar_error_shown = gravatar_error().filter(|_| !elsewhere);
    let preview_view = match optimistic() {
        Some(url) => AvatarView::Image(url),
        None => own_view,
    };
    let preview_class = if busy_now.is_some() {
        "avatar-circle avatar-preview avatar-preview--busy"
    } else {
        "avatar-circle avatar-preview"
    };
    let upload_label = match (busy_now, own_is_set) {
        (Some(AvatarAction::Upload), _) => "Uploading…",
        (_, Some(false)) => "Upload image",
        _ => "Replace image",
    };
    let remove_label = if busy_now == Some(AvatarAction::Remove) {
        "Removing…"
    } else {
        "Remove"
    };
    let gravatar_label = if busy_now == Some(AvatarAction::Gravatar) {
        "Checking…"
    } else {
        "Use Gravatar"
    };
    let upload_describedby = described_by(&[
        Some("avatar-upload-help"),
        upload_error_shown
            .is_some()
            .then_some("avatar-upload-error"),
        elsewhere_id,
    ]);
    let gravatar_describedby = described_by(&[
        Some("avatar-gravatar-help"),
        gravatar_error_shown
            .is_some()
            .then_some("avatar-gravatar-error"),
        elsewhere_id,
    ]);
    let show_gravatar = own.load == OwnAvatarLoad::Loaded && own.gravatar_enabled;
    let gravatar_blank = gravatar_input.read().trim().is_empty();
    let try_again = {
        let cache_ctx = cache_ctx.clone();
        move |_: MouseEvent| {
            if let Some(own) = own_ctx {
                let cache = cache_ctx.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let _ = recheck_own_avatar(own, cache).await;
                });
            }
            focus_with_fallback(UPLOAD_INPUT_ID, fallback_focus);
        }
    };
    let check_again = {
        let cache_ctx = cache_ctx.clone();
        move |_: MouseEvent| {
            let Some(own) = own_ctx.filter(|_| !*checking.peek()) else {
                return;
            };
            checking.set(true);
            focus_upload_when_clear.set(true);
            let (done_tx, done_rx) = oneshot::channel::<OwnRead>();
            let cache = cache_ctx.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let _ = done_tx.send(recheck_own_avatar(own, cache).await);
            });
            let cache = cache_ctx.clone();
            spawn(async move {
                let outcome = done_rx.await;
                checking.set(false);
                if let Some(cache) = cache.filter(PeerAvatarCacheCtx::signed_in_elsewhere) {
                    focus_upload_when_clear.set(false);
                    if outcome == Ok(Err(OwnReadFailure::OtherAccount)) {
                        cache.renotify_signed_in_elsewhere();
                    }
                }
            });
        }
    };

    let on_file = {
        let cache_ctx = cache_ctx.clone();
        move |evt: Event<FormData>| {
            let Some(input) = evt
                .data()
                .downcast::<web_sys::Event>()
                .and_then(|e| e.target())
                .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
            else {
                return;
            };
            let file = input.files().and_then(|files| files.get(0));
            input.set_value("");
            let Some(file) = file else {
                return;
            };
            if own_state.peek().busy.is_some() || signed_in_elsewhere(&cache_ctx) {
                return;
            }
            if let Err(failure) = validate_avatar_file(&file.type_(), &file.name(), file.size()) {
                show_upload_error(avatar_error_copy(AvatarAction::Upload, &failure));
                return;
            }
            upload_error.set(None);
            set_busy(own_state, Some(AvatarAction::Upload));
            let before_etag = own_etag(own_state);
            let (preview_tx, preview_rx) = oneshot::channel::<String>();
            let (done_tx, done_rx) = oneshot::channel::<Result<bool, AvatarFailure>>();
            let (landed_tx, landed_rx) = oneshot::channel::<&'static str>();
            let cache = cache_ctx.clone();
            let expected = meeting_user(&cache);
            wasm_bindgen_futures::spawn_local(async move {
                let processed = match process_avatar_file(&file).await {
                    Ok(processed) => processed,
                    Err(failure) => {
                        set_busy(own_state, None);
                        let _ = done_tx.send(Err(failure));
                        return;
                    }
                };
                let preview = bytes_blob(&processed.bytes, processed.mime)
                    .and_then(|blob| web_sys::Url::create_object_url_with_blob(&blob).ok());
                if let Some(url) = &preview {
                    let _ = preview_tx.send(url.clone());
                }
                let mime = processed.mime;
                let bytes = processed.bytes;
                let result = within_save_timeout(with_user_refresh(|| {
                    let bytes = bytes.clone();
                    let expected = expected.clone();
                    async move {
                        user_client()?
                            .upload_avatar(bytes, mime, expected.as_deref())
                            .await
                    }
                }))
                .await;
                note_refused_write(&cache, &result);
                let outcome = match result {
                    Ok(info) => {
                        let fallback = AvatarBytes {
                            bytes,
                            content_type: mime.to_string(),
                        };
                        commit_saved_avatar(own_ctx, cache.clone(), expected, info, Some(fallback))
                            .await;
                        Ok(processed.small)
                    }
                    Err(failure) => Err(failure),
                };
                let unconfirmed = outcome == Err(AvatarFailure::Unconfirmed);
                set_busy(own_state, None);
                let _ = done_tx.send(outcome);
                if let Some(url) = preview {
                    retire_object_url(url);
                }
                let attempt = Attempt {
                    action: AvatarAction::Upload,
                    before_etag,
                    success: if processed.small {
                        ANNOUNCE_UPLOADED_SMALL
                    } else {
                        ANNOUNCE_UPLOADED
                    },
                    landed: landed_tx,
                };
                resync_if_unconfirmed(unconfirmed, own_ctx, cache, attempt).await;
            });
            spawn(async move {
                if let Ok(url) = preview_rx.await {
                    optimistic.set(Some(url));
                }
                let outcome = done_rx.await;
                optimistic.set(None);
                match outcome {
                    Ok(Ok(true)) => announce(ANNOUNCE_UPLOADED_SMALL),
                    Ok(Ok(false)) => announce(ANNOUNCE_UPLOADED),
                    Ok(Err(AvatarFailure::SignedInElsewhere)) => {}
                    Ok(Err(failure)) => {
                        show_upload_error(avatar_error_copy(AvatarAction::Upload, &failure));
                        let shown = *upload_error_seq.peek();
                        announce_if_landed(
                            landed_rx,
                            upload_error,
                            upload_error_seq,
                            shown,
                            announce,
                        )
                        .await;
                    }
                    Err(_) => {}
                }
            });
        }
    };

    let on_remove = {
        let cache_ctx = cache_ctx.clone();
        move |_| {
            if own_state.peek().busy.is_some() || signed_in_elsewhere(&cache_ctx) {
                return;
            }
            upload_error.set(None);
            set_busy(own_state, Some(AvatarAction::Remove));
            let (landed_tx, landed_rx) = oneshot::channel::<&'static str>();
            let attempt = Attempt {
                action: AvatarAction::Remove,
                before_etag: own_etag(own_state),
                success: ANNOUNCE_REMOVED,
                landed: landed_tx,
            };
            let (done_tx, done_rx) = oneshot::channel::<Result<(), AvatarFailure>>();
            let cache = cache_ctx.clone();
            let expected = meeting_user(&cache);
            wasm_bindgen_futures::spawn_local(async move {
                let result = within_save_timeout(with_user_refresh(|| {
                    let expected = expected.clone();
                    async move { user_client()?.delete_avatar(expected.as_deref()).await }
                }))
                .await;
                note_refused_write(&cache, &result);
                let outcome = match result {
                    Ok(()) => {
                        if let Some(own) = own_ctx {
                            set_own_avatar(own.0, None, None);
                        }
                        if let (Some(cache), Some(user_id)) = (&cache, expected.as_deref()) {
                            if !paint_named_user(cache, user_id, None) {
                                cache.reload_meeting_user();
                            }
                        }
                        Ok(())
                    }
                    Err(failure) => Err(failure),
                };
                let unconfirmed = outcome == Err(AvatarFailure::Unconfirmed);
                set_busy(own_state, None);
                let _ = done_tx.send(outcome);
                resync_if_unconfirmed(unconfirmed, own_ctx, cache, attempt).await;
            });
            spawn(async move {
                let outcome = done_rx.await;
                match outcome {
                    Ok(Ok(())) => {
                        gravatar_input.set(String::new());
                        announce(ANNOUNCE_REMOVED);
                        focus_with_fallback(UPLOAD_INPUT_ID, fallback_focus);
                    }
                    Ok(Err(AvatarFailure::SignedInElsewhere)) => {}
                    Ok(Err(failure)) => {
                        show_upload_error(avatar_error_copy(AvatarAction::Remove, &failure));
                        let shown = *upload_error_seq.peek();
                        let landed = announce_if_landed(
                            landed_rx,
                            upload_error,
                            upload_error_seq,
                            shown,
                            announce,
                        )
                        .await;
                        if landed {
                            gravatar_input.set(String::new());
                            focus_with_fallback(UPLOAD_INPUT_ID, fallback_focus);
                        }
                    }
                    Err(_) => {}
                }
            });
        }
    };

    let mut submit_gravatar = {
        let cache_ctx = cache_ctx.clone();
        move || {
            let value = gravatar_input.peek().trim().to_string();
            if own_state.peek().busy.is_some()
                || value.is_empty()
                || signed_in_elsewhere(&cache_ctx)
            {
                return;
            }
            gravatar_error.set(None);
            set_busy(own_state, Some(AvatarAction::Gravatar));
            let (landed_tx, landed_rx) = oneshot::channel::<&'static str>();
            let attempt = Attempt {
                action: AvatarAction::Gravatar,
                before_etag: own_etag(own_state),
                success: ANNOUNCE_GRAVATAR,
                landed: landed_tx,
            };
            let (done_tx, done_rx) = oneshot::channel::<Result<(), AvatarFailure>>();
            let cache = cache_ctx.clone();
            let expected = meeting_user(&cache);
            wasm_bindgen_futures::spawn_local(async move {
                let result = within_save_timeout(with_user_refresh(|| {
                    let value = value.clone();
                    let expected = expected.clone();
                    async move {
                        user_client()?
                            .set_gravatar_avatar(&value, expected.as_deref())
                            .await
                    }
                }))
                .await;
                note_refused_write(&cache, &result);
                let outcome = match result {
                    Ok(info) => {
                        commit_saved_avatar(own_ctx, cache.clone(), expected, info, None).await;
                        Ok(())
                    }
                    Err(failure) => Err(failure),
                };
                let unconfirmed = outcome == Err(AvatarFailure::Unconfirmed);
                set_busy(own_state, None);
                let _ = done_tx.send(outcome);
                resync_if_unconfirmed(unconfirmed, own_ctx, cache, attempt).await;
            });
            spawn(async move {
                let outcome = done_rx.await;
                match outcome {
                    Ok(Ok(())) => announce(ANNOUNCE_GRAVATAR),
                    Ok(Err(AvatarFailure::SignedInElsewhere)) => {}
                    Ok(Err(failure)) => {
                        gravatar_error
                            .set(Some(avatar_error_copy(AvatarAction::Gravatar, &failure)));
                        gravatar_error_seq += 1;
                        let shown = *gravatar_error_seq.peek();
                        announce_if_landed(
                            landed_rx,
                            gravatar_error,
                            gravatar_error_seq,
                            shown,
                            announce,
                        )
                        .await;
                    }
                    Err(_) => {}
                }
            });
        }
    };
    let mut submit_on_enter = submit_gravatar.clone();

    rsx! {
        div {
            class: "appearance-settings-panel avatar-settings",
            "data-busy": if busy_now.is_some() { "true" } else { "false" },
            div { class: "appearance-content-column",
                section { class: "appearance-section",
                    if variant == AvatarPanelVariant::InMeeting {
                        div { class: "appearance-section-header",
                            h3 { class: "appearance-section-title", "Avatar" }
                        }
                    }
                    p { class: "appearance-section-helper",
                        "Shown on your tile when your camera is off, in every meeting."
                    }
                    if !can_edit {
                        div { class: "avatar-editor", "data-testid": "avatar-guest-note",
                            AvatarImage {
                                view: AvatarView::Default,
                                extra_class: "avatar-circle avatar-preview",
                                testid: "settings-avatar-preview",
                            }
                            div { class: "avatar-editor-body",
                                p { class: "avatar-source", "Sign in to set an avatar." }
                                p { class: "avatar-help", "Guests appear with the default icon." }
                            }
                        }
                    } else {
                        div { class: "avatar-editor",
                            AvatarImage {
                                view: preview_view,
                                extra_class: preview_class,
                                testid: "settings-avatar-preview",
                            }
                            div { class: "avatar-editor-body",
                                if !elsewhere {
                                    p { class: "avatar-source", "data-testid": "avatar-source-label",
                                        {avatar_source_text(&own)}
                                    }
                                }
                                if elsewhere {
                                    for notice in [notices] {
                                        p {
                                            key: "{notice}",
                                            id: SIGNED_IN_ELSEWHERE_ID,
                                            class: "avatar-error",
                                            role: "alert",
                                            "data-testid": SIGNED_IN_ELSEWHERE_ID,
                                            {COPY_SIGNED_IN_ELSEWHERE}
                                        }
                                    }
                                    button {
                                        r#type: "button",
                                        class: "avatar-link-btn",
                                        "data-testid": CHECK_AGAIN_TESTID,
                                        "aria-busy": if checking() { "true" } else { "false" },
                                        onclick: check_again,
                                        if checking() {
                                            "Checking…"
                                        } else {
                                            "Check again"
                                        }
                                    }
                                } else if own.load == OwnAvatarLoad::Failed {
                                    button {
                                        r#type: "button",
                                        class: "avatar-link-btn",
                                        "data-testid": "avatar-retry-button",
                                        onclick: try_again,
                                        "Try again"
                                    }
                                }
                                div { class: "avatar-actions",
                                    label { class: "avatar-btn",
                                        "{upload_label}"
                                        input {
                                            id: UPLOAD_INPUT_ID,
                                            "data-testid": "avatar-upload-input",
                                            r#type: "file",
                                            class: "visually-hidden",
                                            accept: "image/png,image/jpeg,image/webp",
                                            "aria-describedby": upload_describedby,
                                            "aria-disabled": if locked { "true" } else { "false" },
                                            onclick: move |evt: MouseEvent| {
                                                if elsewhere || own_state.peek().busy.is_some() {
                                                    evt.prevent_default();
                                                }
                                            },
                                            onchange: on_file,
                                        }
                                    }
                                    if has_avatar {
                                        button {
                                            id: REMOVE_BUTTON_ID,
                                            "data-testid": "avatar-remove-button",
                                            r#type: "button",
                                            class: "avatar-link-btn",
                                            "aria-disabled": if locked { "true" } else { "false" },
                                            "aria-describedby": elsewhere_id,
                                            onclick: on_remove,
                                            "{remove_label}"
                                        }
                                    }
                                }
                                p { id: "avatar-upload-help", class: "avatar-help",
                                    "PNG, JPEG or WebP up to 2 MB. Square images at least 256×256 px look best."
                                }
                                if let Some(message) = upload_error_shown {
                                    p {
                                        key: "{upload_error_seq}",
                                        id: "avatar-upload-error",
                                        class: "avatar-error",
                                        role: "alert",
                                        "data-testid": "avatar-upload-error",
                                        "{message}"
                                    }
                                }
                            }
                        }
                        if show_gravatar {
                            hr { class: "appearance-section-divider" }
                            div { class: "avatar-gravatar-form",
                                label {
                                    r#for: "avatar-gravatar-input",
                                    class: "avatar-field-label",
                                    "Gravatar email or link"
                                }
                                div { class: "avatar-gravatar-row",
                                    input {
                                        id: "avatar-gravatar-input",
                                        "data-testid": "avatar-gravatar-input",
                                        class: if gravatar_error_shown.is_some() { "input-apple input-apple--invalid avatar-gravatar-input" } else { "input-apple avatar-gravatar-input" },
                                        r#type: "text",
                                        autocomplete: "off",
                                        autocapitalize: "off",
                                        spellcheck: "false",
                                        maxlength: "2048",
                                        placeholder: "you@example.com or https://gravatar.com/avatar/…",
                                        "aria-invalid": if gravatar_error_shown.is_some() { "true" } else { "false" },
                                        "aria-describedby": gravatar_describedby,
                                        value: "{gravatar_input}",
                                        oninput: move |evt: FormEvent| {
                                            gravatar_input.set(evt.value());
                                            gravatar_error.set(None);
                                        },
                                        onkeydown: move |evt: KeyboardEvent| {
                                            let key = evt.key();
                                            if is_tab_switch_key(&key) {
                                                evt.stop_propagation();
                                            } else if key == Key::Enter {
                                                evt.prevent_default();
                                                submit_on_enter();
                                            }
                                        },
                                    }
                                    button {
                                        r#type: "button",
                                        class: "avatar-btn",
                                        "data-testid": "avatar-gravatar-save",
                                        "aria-disabled": if locked || gravatar_blank { "true" } else { "false" },
                                        "aria-describedby": elsewhere_id,
                                        onclick: move |_| submit_gravatar(),
                                        "{gravatar_label}"
                                    }
                                }
                                p { id: "avatar-gravatar-help", class: "avatar-help",
                                    "Enter your Gravatar email or paste your Gravatar image link. The image is copied when you save, so save again after changing it on Gravatar."
                                }
                                if let Some(message) = gravatar_error_shown {
                                    p {
                                        key: "{gravatar_error_seq}",
                                        id: "avatar-gravatar-error",
                                        class: "avatar-error",
                                        role: "alert",
                                        "data-testid": "avatar-gravatar-error",
                                        "{message}"
                                    }
                                }
                            }
                        }
                    }
                    span {
                        class: "visually-hidden",
                        role: "status",
                        "aria-live": "polite",
                        "aria-atomic": "true",
                        "data-testid": "avatar-status",
                        {action_bar_announce_text(&announcement.read(), announcement_nonce())}
                    }
                }
            }
        }
    }
}

fn avatar_edit_label(has_avatar: bool) -> &'static str {
    if has_avatar {
        "Change avatar"
    } else {
        "Add avatar"
    }
}

#[component]
fn AvatarEditBadge(class: &'static str) -> Element {
    rsx! {
        span { class, "aria-hidden": "true",
            svg {
                view_box: "0 0 24 24",
                fill: "none",
                stroke: "currentColor",
                stroke_width: "2",
                stroke_linecap: "round",
                stroke_linejoin: "round",
                path { d: "M12 20h9" }
                path { d: "M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4Z" }
            }
        }
    }
}

/// The own avatar's image and the edit label that matches it; in a meeting both come from the
/// meeting user's slot.
fn use_own_avatar_and_label() -> (AvatarView, &'static str) {
    let (view, is_set) = use_own_avatar();
    (view, avatar_edit_label(is_set != Some(false)))
}

/// The pre-join camera-off avatar, as the button that opens [`AvatarDialog`].
#[component]
pub fn PrejoinAvatarButton(on_click: EventHandler<()>) -> Element {
    let (view, label) = use_own_avatar_and_label();
    rsx! {
        button {
            id: PREJOIN_AVATAR_BUTTON_TESTID,
            r#type: "button",
            class: "prejoin-avatar-button",
            "data-testid": PREJOIN_AVATAR_BUTTON_TESTID,
            "aria-haspopup": "dialog",
            "aria-label": label,
            title: label,
            onclick: move |_| on_click.call(()),
            AvatarImage { view, extra_class: "avatar-circle", testid: "prejoin-avatar-img" }
            AvatarEditBadge { class: "prejoin-avatar-badge" }
        }
    }
}

/// The in-meeting self tile's camera-off avatar, as a button that calls `on_edit`;
/// `children` instead for a user who cannot set an avatar.
#[component]
pub fn SelfAvatarEditButton(on_edit: EventHandler<()>, children: Element) -> Element {
    let (_, label) = use_own_avatar_and_label();
    if !avatar_editing_allowed() {
        return children;
    }
    rsx! {
        button {
            id: SELF_AVATAR_EDIT_BUTTON_TESTID,
            r#type: "button",
            class: "self-avatar-edit-button",
            "data-testid": SELF_AVATAR_EDIT_BUTTON_TESTID,
            "aria-haspopup": "dialog",
            "aria-label": label,
            title: label,
            onclick: move |_| on_edit.call(()),
            SelfAvatar { extra_class: "avatar-circle" }
            AvatarEditBadge { class: "self-avatar-edit-badge" }
        }
    }
}

/// Moves focus into Settings when a route opens it and back out when that modal closes.
#[derive(Clone, Copy)]
pub struct SettingsFocusReturn(Signal<Option<u32>>, fn());

pub fn use_settings_focus_return(restore: fn()) -> SettingsFocusReturn {
    SettingsFocusReturn(use_signal(|| None), restore)
}

pub fn use_self_avatar_settings_focus() -> SettingsFocusReturn {
    use_settings_focus_return(|| {
        focus_first_taker(&[
            &format!("#{SELF_AVATAR_EDIT_BUTTON_TESTID}"),
            "[data-testid='camera-toggle-button']",
            "#grid-container",
        ])
    })
}

impl SettingsFocusReturn {
    /// This route opened Settings modal `generation`; `focus_in` runs once it has mounted.
    pub fn opened(mut self, generation: u32, focus_in: impl FnOnce() + 'static) {
        self.0.set(Some(generation));
        Timeout::new(0, focus_in).forget();
    }

    /// Settings modal `generation` closed.
    pub fn closed(mut self, generation: u32) {
        let from_this_route = *self.0.peek() == Some(generation);
        if self.0.peek().is_some() {
            self.0.set(None);
        }
        if from_this_route {
            Timeout::new(0, self.1).forget();
        }
    }
}

const DIALOG_SELECTOR: &str = ".avatar-dialog";

fn open_avatar_dialog() -> Option<web_sys::HtmlDialogElement> {
    web_sys::window()?
        .document()?
        .query_selector(DIALOG_SELECTOR)
        .ok()??
        .dyn_into()
        .ok()
}

type NativeCloseListener = (web_sys::HtmlDialogElement, Closure<dyn FnMut()>);

#[component]
pub fn AvatarDialog(on_close: EventHandler<()>) -> Element {
    let mut closed = use_hook(|| CopyValue::new(false));
    let mut finish = move || {
        if !*closed.peek() {
            closed.set(true);
            on_close.call(());
        }
    };
    let mut close = move || {
        if let Some(dialog) = open_avatar_dialog() {
            dialog.close();
        }
        finish();
    };
    let native_close = use_hook(|| Rc::new(RefCell::new(None::<NativeCloseListener>)));
    use_drop({
        let native_close = native_close.clone();
        move || {
            if let Some((dialog, listener)) = native_close.borrow_mut().take() {
                let _ = dialog.remove_event_listener_with_callback(
                    "close",
                    listener.as_ref().unchecked_ref(),
                );
            }
        }
    });
    rsx! {
        dialog {
            class: "avatar-dialog",
            tabindex: "-1",
            "aria-modal": "true",
            "aria-labelledby": "avatar-dialog-title",
            "data-testid": "avatar-dialog",
            onmounted: move |evt: MountedEvent| {
                let dialog = evt
                    .data()
                    .downcast::<web_sys::Element>()
                    .and_then(|el| el.dyn_ref::<web_sys::HtmlDialogElement>().cloned());
                if let Some(dialog) = &dialog {
                    if native_close.borrow().is_none() {
                        let listener = Closure::<dyn FnMut()>::new(finish);
                        let _ = dialog.add_event_listener_with_callback(
                            "close",
                            listener.as_ref().unchecked_ref(),
                        );
                        native_close.replace(Some((dialog.clone(), listener)));
                    }
                }
                if let Some(dialog) = dialog.filter(|d| !d.open()) {
                    let _ = dialog.show_modal();
                }
                focus_with_fallback(UPLOAD_INPUT_ID, DIALOG_SELECTOR);
            },
            oncancel: move |evt: CancelEvent| {
                evt.prevent_default();
                close();
            },
            onkeydown: move |evt: KeyboardEvent| match evt.key() {
                Key::Escape => {
                    evt.prevent_default();
                    evt.stop_propagation();
                    close();
                }
                Key::Tab if trap_tab_within(DIALOG_SELECTOR, evt.modifiers().shift()) => {
                    evt.prevent_default();
                }
                _ => {}
            },
            onclick: move |_| close(),
            div {
                class: "avatar-dialog-body",
                onclick: move |evt: MouseEvent| evt.stop_propagation(),
                div { class: "avatar-dialog-header",
                    h2 { id: "avatar-dialog-title", class: "avatar-dialog-title", "Your avatar" }
                    button {
                        r#type: "button",
                        class: "settings-modal-close",
                        "aria-label": "Close",
                        onclick: move |_| close(),
                        svg { view_box: "0 0 24 24", width: "16", height: "16", "aria-hidden": "true",
                            path {
                                d: "M6 6L18 18M18 6L6 18",
                                stroke: "currentColor",
                                stroke_width: "2",
                                stroke_linecap: "round",
                            }
                        }
                    }
                }
                AvatarSettingsPanel { variant: AvatarPanelVariant::Dialog, can_edit: true }
                div { class: "avatar-dialog-actions",
                    button {
                        r#type: "button",
                        class: "btn-apple btn-primary btn-sm avatar-dialog-done",
                        "data-testid": "avatar-dialog-done",
                        onclick: move |_| close(),
                        "Done"
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(status: u16, code: &str) -> ApiError {
        ApiError::ServerError {
            status,
            body: format!(r#"{{"success":false,"result":{{"code":"{code}","message":"m"}}}}"#),
        }
    }

    fn copy_for(action: AvatarAction, err: &ApiError) -> &'static str {
        avatar_error_copy(action, &classify_api_error(err))
    }

    #[test]
    fn upload_errors_map_backend_codes_to_their_copy() {
        assert_eq!(
            copy_for(AvatarAction::Upload, &server(415, "UNSUPPORTED_MEDIA_TYPE")),
            "That file type isn't supported. Use a PNG, JPEG or WebP image."
        );
        assert_eq!(
            copy_for(AvatarAction::Upload, &server(422, "INVALID_IMAGE")),
            "That image couldn't be read. Try a different file."
        );
        assert_eq!(
            copy_for(AvatarAction::Upload, &server(413, "PAYLOAD_TOO_LARGE")),
            COPY_UNREADABLE
        );
        assert_eq!(
            copy_for(AvatarAction::Upload, &server(503, "SERVICE_BUSY")),
            "Couldn't save your avatar. Try again."
        );
        assert_eq!(
            copy_for(AvatarAction::Upload, &ApiError::RateLimitExceeded),
            "You've changed your avatar several times. Try again in a minute."
        );
        assert_eq!(
            copy_for(AvatarAction::Upload, &ApiError::NotAuthenticated),
            "Your sign-in has expired. Sign in again to change your avatar."
        );
    }

    #[test]
    fn gravatar_errors_map_backend_codes_to_their_copy() {
        assert_eq!(
            copy_for(AvatarAction::Gravatar, &server(400, "INVALID_GRAVATAR")),
            "Enter your Gravatar email or a link like https://gravatar.com/avatar/…"
        );
        assert_eq!(
            copy_for(AvatarAction::Gravatar, &server(400, "INVALID_INPUT")),
            COPY_GRAVATAR_INVALID
        );
        assert_eq!(
            copy_for(AvatarAction::Gravatar, &server(422, "GRAVATAR_NOT_FOUND")),
            COPY_GRAVATAR_NOT_FOUND
        );
        assert_eq!(
            copy_for(AvatarAction::Gravatar, &server(422, "INVALID_IMAGE")),
            COPY_GRAVATAR_UNUSABLE
        );
        assert_eq!(
            copy_for(AvatarAction::Gravatar, &server(502, "GRAVATAR_UNAVAILABLE")),
            COPY_SAVE_FAILED
        );
        assert_eq!(
            copy_for(
                AvatarAction::Gravatar,
                &ApiError::NotFound(
                    r#"{"success":false,"result":{"code":"NOT_FOUND","message":"m"}}"#.into()
                )
            ),
            COPY_SAVE_FAILED
        );
    }

    #[test]
    fn a_too_small_image_names_the_minimum_size() {
        assert_eq!(
            avatar_error_copy(AvatarAction::Upload, &AvatarFailure::TooSmall),
            COPY_TOO_SMALL
        );
        let side = MIN_AVATAR_DIMENSION;
        assert!(
            COPY_TOO_SMALL.contains(&format!("at least {side}×{side} px")),
            "{COPY_TOO_SMALL}"
        );
    }

    #[test]
    fn a_save_that_timed_out_is_reported_as_unconfirmed() {
        for action in [
            AvatarAction::Upload,
            AvatarAction::Gravatar,
            AvatarAction::Remove,
        ] {
            assert_eq!(
                avatar_error_copy(action, &AvatarFailure::Unconfirmed),
                "Couldn't confirm your change. If your avatar doesn't update in a moment, try again.",
                "{action:?}"
            );
        }
    }

    fn stored(source: AvatarSource, etag: &str) -> AvatarInfo {
        AvatarInfo {
            source,
            gravatar_url: None,
            content_type: "image/jpeg".into(),
            etag: etag.into(),
            updated_at: 0,
        }
    }

    #[test]
    fn a_timed_out_change_counts_as_landed_only_when_the_server_shows_it() {
        let upload = stored(AvatarSource::Upload, "new");
        let gravatar = stored(AvatarSource::Gravatar, "new");
        assert!(change_landed(AvatarAction::Upload, None, Some(&upload)));
        assert!(change_landed(
            AvatarAction::Upload,
            Some("old"),
            Some(&upload)
        ));
        assert!(!change_landed(
            AvatarAction::Upload,
            Some("new"),
            Some(&upload)
        ));
        assert!(!change_landed(AvatarAction::Upload, None, None));
        assert!(!change_landed(AvatarAction::Upload, None, Some(&gravatar)));
        assert!(change_landed(
            AvatarAction::Gravatar,
            Some("old"),
            Some(&gravatar)
        ));
        assert!(!change_landed(
            AvatarAction::Gravatar,
            Some("old"),
            Some(&upload)
        ));
        assert!(!change_landed(
            AvatarAction::Gravatar,
            Some("new"),
            Some(&gravatar)
        ));
        assert!(change_landed(AvatarAction::Remove, Some("old"), None));
        assert!(!change_landed(
            AvatarAction::Remove,
            Some("old"),
            Some(&upload)
        ));
    }

    #[test]
    fn a_failed_remove_never_reads_as_an_upload_problem() {
        assert_eq!(
            copy_for(AvatarAction::Remove, &server(422, "INVALID_IMAGE")),
            "Couldn't remove your avatar. Try again."
        );
        assert_eq!(
            copy_for(AvatarAction::Remove, &ApiError::RateLimitExceeded),
            COPY_RATE_LIMITED
        );
        assert_eq!(
            copy_for(AvatarAction::Remove, &server(500, "INTERNAL")),
            COPY_REMOVE_FAILED
        );
    }

    #[test]
    fn an_unparseable_error_body_is_a_generic_failure() {
        assert_eq!(
            classify_api_error(&ApiError::ServerError {
                status: 500,
                body: "<html>".into()
            }),
            AvatarFailure::Server(None)
        );
        assert_eq!(
            copy_for(AvatarAction::Gravatar, &ApiError::Config("x".into())),
            COPY_SAVE_FAILED
        );
    }

    #[test]
    fn a_write_for_another_account_says_so_for_every_action() {
        for action in [
            AvatarAction::Upload,
            AvatarAction::Gravatar,
            AvatarAction::Remove,
        ] {
            assert_eq!(
                copy_for(action, &ApiError::IdentityMismatch),
                COPY_SIGNED_IN_ELSEWHERE
            );
        }
    }

    #[test]
    fn file_checks_run_type_first_then_size() {
        let limit = AVATAR_CLIENT_MAX_FILE_BYTES as f64;
        assert_eq!(validate_avatar_file("image/png", "a.png", limit), Ok(()));
        assert_eq!(validate_avatar_file("IMAGE/JPEG", "a", 10.0), Ok(()));
        assert_eq!(validate_avatar_file("image/webp", "a", 10.0), Ok(()));
        assert_eq!(
            validate_avatar_file("image/png", "a.png", limit + 1.0),
            Err(AvatarFailure::TooLarge)
        );
        assert_eq!(
            validate_avatar_file("image/gif", "a.gif", limit + 1.0),
            Err(AvatarFailure::UnsupportedType)
        );
        assert_eq!(
            validate_avatar_file("image/svg+xml", "a.png", 10.0),
            Err(AvatarFailure::UnsupportedType)
        );
    }

    #[test]
    fn an_untyped_file_falls_back_to_its_extension() {
        assert_eq!(validate_avatar_file("", "Me.JPG", 10.0), Ok(()));
        assert_eq!(validate_avatar_file("", "me.webp", 10.0), Ok(()));
        assert_eq!(
            validate_avatar_file("", "me.heic", 10.0),
            Err(AvatarFailure::UnsupportedType)
        );
        assert_eq!(
            validate_avatar_file("", "png", 10.0),
            Err(AvatarFailure::UnsupportedType)
        );
    }

    #[test]
    fn the_crop_is_a_centered_square_that_never_upscales() {
        assert_eq!(
            crop_plan(1920, 1080),
            Some(CropPlan {
                sx: 420,
                sy: 0,
                side: 1080,
                out: 256
            })
        );
        assert_eq!(
            crop_plan(300, 801),
            Some(CropPlan {
                sx: 0,
                sy: 250,
                side: 300,
                out: 256
            })
        );
        assert_eq!(
            crop_plan(64, 100),
            Some(CropPlan {
                sx: 0,
                sy: 18,
                side: 64,
                out: 64
            })
        );
        assert_eq!(crop_plan(0, 50), None);
    }

    #[test]
    fn the_source_line_names_where_the_avatar_came_from() {
        let mut own = OwnAvatar::default();
        assert_eq!(avatar_source_text(&own), "Loading your avatar…");
        own.load = OwnAvatarLoad::Failed;
        assert_eq!(avatar_source_text(&own), "Couldn't load your avatar.");
        own.load = OwnAvatarLoad::Loaded;
        assert_eq!(
            avatar_source_text(&own),
            "No avatar set. Others see the default icon."
        );
        own.info = Some(AvatarInfo {
            source: AvatarSource::Gravatar,
            gravatar_url: None,
            content_type: "image/jpeg".into(),
            etag: "e".into(),
            updated_at: 0,
        });
        assert_eq!(avatar_source_text(&own), "Using your Gravatar image.");
    }

    #[test]
    fn only_arrow_keys_are_kept_from_the_settings_tablist() {
        for key in [
            Key::ArrowLeft,
            Key::ArrowRight,
            Key::ArrowUp,
            Key::ArrowDown,
        ] {
            assert!(is_tab_switch_key(&key), "{key:?}");
        }
        for key in [Key::Enter, Key::Escape, Key::Tab, Key::Home] {
            assert!(!is_tab_switch_key(&key), "{key:?}");
        }
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod dom_tests {
    use super::*;
    use crate::components::announcement_overrides::use_override_editor;
    use crate::components::avatar::dom_support::*;
    use crate::components::avatar::{
        use_provide_peer_avatar_cache, AvatarCacheConfig, AvatarTimings, PeerAvatar, SelfAvatar,
        AVATAR_TIMINGS,
    };
    use crate::components::co_hosts::MeetingOwnership;
    use crate::components::device_settings_modal::{
        focus_profile_tab, DeviceSettingsModal, PROFILE_SECTION,
    };
    use crate::components::host::SelfCameraOffPlaceholder;
    use crate::components::pre_join_settings_card::PreJoinSettingsCard;
    use crate::components::video_control_buttons::CameraButton;
    use std::cell::{Cell, RefCell};
    use wasm_bindgen_futures::JsFuture;
    use wasm_bindgen_test::wasm_bindgen_test;

    thread_local! {
        static OWN_SEED: RefCell<OwnAvatar> = RefCell::new(OwnAvatar::default());
        static GUEST: Cell<bool> = const { Cell::new(false) };
        static CAMERA: RefCell<Option<Signal<bool>>> = const { RefCell::new(None) };
        static MEETING: RefCell<Option<(Signal<&'static str>, Signal<bool>)>> =
            const { RefCell::new(None) };
        static PANEL_OPEN: RefCell<Option<Signal<bool>>> = const { RefCell::new(None) };
        static SELF_CAMERA: RefCell<Option<Signal<bool>>> = const { RefCell::new(None) };
        static PLACEMENTS: Cell<(bool, bool)> = const { Cell::new((true, false)) };
        static CAMERA_TOGGLE: Cell<bool> = const { Cell::new(true) };
        static MEETING_CACHE: RefCell<Option<PeerAvatarCacheCtx>> = const { RefCell::new(None) };
    }

    const ROUTES: &str = "\
        if (window.__refuseWrites && call.method !== 'GET' \
            && call.path.indexOf('/api/v1/user/avatar') === 0) { \
            return json(409, { success: false, result: \
                { code: 'AVATAR_IDENTITY_MISMATCH', message: 'signed in as someone else' } }); } \
        var own = call.path === '/api/v1/user/avatar'; \
        var store = function(info) { \
            window.__ownMeta = Object.assign({}, window.__ownMeta, { avatar: info }); return info; }; \
        if (own && call.method === 'GET') { var meta = window.__ownMeta; \
            var answer = function() { return json(200, { success: true, result: meta }); }; \
            return window.__holdMeta ? hold(answer) : answer(); } \
        if (own && call.method === 'DELETE') { \
            var gone = function() { store(null); return new Response(null, { status: 204 }); }; \
            return window.__holdDelete ? hold(gone) : gone(); } \
        if (own && call.method === 'PUT') { \
            var saved = function() { return json(200, { success: true, result: store( \
                { source: 'upload', content_type: 'image/jpeg', etag: 'e2', updated_at: 2 }) }); }; \
            return window.__holdUpload ? hold(saved) : saved(); } \
        if (call.path === '/api/v1/user/avatar/gravatar' && call.method === 'PUT') { \
            var linked = function() { return json(200, { success: true, result: store( \
                { source: 'gravatar', gravatar_url: 'https://gravatar.com/avatar/' + 'ab'.repeat(32), \
                content_type: 'image/jpeg', etag: 'g1', updated_at: 3 }) }); }; \
            return window.__holdGravatar ? hold(linked) : linked(); } \
        if (call.path === '/api/v1/user/avatar/image') { return jpeg(); } \
        if (call.path.indexOf('/meetings/') >= 0) { \
            var stored = window.__meetingAvatar !== undefined ? window.__meetingAvatar \
                : window.__ownMeta && window.__ownMeta.avatar; \
            if (!stored) { return json(404, { success: false, \
                result: { code: 'AVATAR_NOT_FOUND', message: 'none' } }); } \
            return new Promise(function(resolve) { \
                var c = document.createElement('canvas'); c.width = 16; c.height = 16; \
                c.getContext('2d').fillText(stored.etag, 0, 12); \
                c.toBlob(function(b) { resolve(new Response(b, { status: 200, \
                    headers: { 'content-type': 'image/png' } })); }, 'image/png'); }); } \
        return json(500, {});";

    fn upload_info(etag: &str) -> AvatarInfo {
        AvatarInfo {
            source: AvatarSource::Upload,
            gravatar_url: None,
            content_type: "image/jpeg".to_string(),
            etag: etag.to_string(),
            updated_at: 1,
        }
    }

    fn seed(own: OwnAvatar, guest: bool, meta: &str) {
        OWN_SEED.with(|s| *s.borrow_mut() = own);
        GUEST.with(|g| g.set(guest));
        install_fetch(ROUTES);
        js(&format!(
            "window.__ownMeta = {meta}; window.__refuseWrites = false; \
             window.__meetingAvatar = undefined;"
        ));
    }

    fn loaded(gravatar_enabled: bool) -> OwnAvatar {
        OwnAvatar {
            load: OwnAvatarLoad::Loaded,
            gravatar_enabled,
            ..OwnAvatar::default()
        }
    }

    fn provide_meeting() {
        let own = use_signal(|| OWN_SEED.with(|s| s.borrow().clone()));
        use_context_provider(|| OwnAvatarCtx(own));
        let cache = use_provide_peer_avatar_cache(AvatarCacheConfig {
            meeting_id: "m1".to_string(),
            room_token: "room-token".to_string(),
            is_guest: GUEST.with(Cell::get),
            own_user_id: Some("me@example.com".to_string()),
        });
        MEETING_CACHE.with(|c| *c.borrow_mut() = Some(cache));
    }

    #[allow(non_snake_case)]
    fn ProfileModal() -> Element {
        provide_meeting();
        rsx! {
            div { id: "self-tile", SelfAvatar {} }
            div { id: "pa-21",
                PeerAvatar { session_id: "21".to_string(), user_id: Some("me@example.com".to_string()) }
            }
            DeviceSettingsModal {
                microphones: Vec::new(),
                cameras: Vec::new(),
                speakers: Vec::new(),
                selected_microphone_id: None,
                selected_camera_id: None,
                selected_speaker_id: None,
                on_microphone_select: move |_| {},
                on_camera_select: move |_| {},
                on_speaker_select: move |_| {},
                visible: true,
                on_close: move |_| {},
                initial_section: Some("profile".to_string()),
            }
        }
    }

    fn text(mount: &web_sys::Element, selector: &str) -> String {
        query(mount, selector)
            .and_then(|el| el.text_content())
            .unwrap_or_default()
            .trim_end_matches('\u{00A0}')
            .to_string()
    }

    /// The avatar state at `selector` once it reads `expected`, or what it reads at the deadline.
    async fn settled_state(
        mount: &web_sys::Element,
        selector: &str,
        expected: &str,
    ) -> Option<String> {
        let read = || attr(mount, selector, "data-avatar-state");
        wait_until(|| read().as_deref() == Some(expected)).await;
        read()
    }

    fn active_tab(mount: &web_sys::Element) -> Option<String> {
        attr(mount, ".settings-nav-button.active", "data-testid")
    }

    fn press(target_id: &str, key: &str) {
        js(&format!(
            "var el = document.getElementById('{target_id}'); \
             if (el) {{ el.dispatchEvent(new KeyboardEvent('keydown', {{ key: '{key}', bubbles: true }})); }}"
        ));
    }

    fn pick_files(js_files: &str) {
        js(&format!(
            "var input = document.getElementById('{UPLOAD_INPUT_ID}'); \
             var dt = new DataTransfer(); {js_files}.forEach(function(f) {{ dt.items.add(f); }}); \
             input.files = dt.files; input.dispatchEvent(new Event('change', {{ bubbles: true }}));"
        ));
    }

    /// Pick a real `width`×`height` PNG in the upload control.
    async fn pick_png(width: u32, height: u32) {
        let picked: js_sys::Promise = js(&format!(
            "return new Promise(function(resolve) {{ \
               var c = document.createElement('canvas'); c.width = {width}; c.height = {height}; \
               var g = c.getContext('2d'); g.fillStyle = 'red'; g.fillRect(0, 0, {width}, {height}); \
               c.toBlob(function(b) {{ \
                 var input = document.getElementById('{UPLOAD_INPUT_ID}'); \
                 var dt = new DataTransfer(); \
                 dt.items.add(new File([b], 'me.png', {{ type: 'image/png' }})); \
                 input.files = dt.files; \
                 input.dispatchEvent(new Event('change', {{ bubbles: true }})); \
                 resolve(true); }}, 'image/png'); }});"
        ))
        .unchecked_into();
        JsFuture::from(picked).await.unwrap();
    }

    fn puts_to(path: &str) -> Vec<serde_json::Value> {
        calls()
            .into_iter()
            .filter(|c| c["method"] == "PUT" && c["path"] == path)
            .collect()
    }

    fn dialog_open() -> bool {
        js("var d = document.querySelector('.avatar-dialog'); return !!d && d.open;")
            .as_bool()
            .unwrap_or(false)
    }

    fn focused_testid() -> Option<String> {
        gloo_utils::document()
            .active_element()
            .and_then(|el| el.get_attribute("data-testid"))
    }

    #[wasm_bindgen_test]
    async fn arrow_keys_in_the_gravatar_field_do_not_switch_settings_tabs() {
        seed(
            loaded(true),
            false,
            "{ avatar: null, gravatar_enabled: true }",
        );
        let mount = mount(ProfileModal);
        let ready = wait_until(|| query(&mount, "#avatar-gravatar-input").is_some()).await;
        let mut tabs = Vec::new();
        for key in ["ArrowRight", "ArrowLeft", "ArrowDown", "ArrowUp"] {
            press("avatar-gravatar-input", key);
            settle().await;
            tabs.push(active_tab(&mount));
        }
        let field_kept = query(&mount, "#avatar-gravatar-input").is_some();
        press("device-settings-dialog", "ArrowRight");
        settle().await;
        let outside_field = active_tab(&mount);
        restore_fetch();
        mount.remove();
        assert!(ready, "the Gravatar field renders on the Profile tab");
        assert!(
            tabs.iter()
                .all(|tab| tab.as_deref() == Some("settings-nav-profile")),
            "{tabs:?}"
        );
        assert!(
            field_kept,
            "the field must not unmount while the user types"
        );
        assert_eq!(
            outside_field.as_deref(),
            Some("settings-nav-audio"),
            "outside the field Arrow keys still move along the tab ring"
        );
    }

    #[wasm_bindgen_test]
    async fn a_guest_is_told_to_sign_in_and_nothing_is_loaded() {
        seed(
            OwnAvatar::default(),
            true,
            "{ avatar: null, gravatar_enabled: true }",
        );
        let mount = mount(ProfileModal);
        let note =
            wait_until(|| query(&mount, "[data-testid='avatar-guest-note']").is_some()).await;
        settle().await;
        let note_text = text(&mount, "[data-testid='avatar-guest-note']");
        let upload = query(&mount, "#avatar-upload-input").is_some();
        let own_calls =
            calls_to("/api/v1/user/avatar").len() + calls_to("/user/avatar/image").len();
        restore_fetch();
        mount.remove();
        assert!(note);
        assert!(
            note_text.contains("Sign in to set an avatar."),
            "{note_text}"
        );
        assert!(!upload, "a guest gets no upload control");
        assert_eq!(own_calls, 0, "a guest never asks for an own avatar");
    }

    #[wasm_bindgen_test]
    async fn remove_reverts_every_own_tile_to_the_default_icon() {
        let url = real_jpeg_url().await;
        seed(
            OwnAvatar {
                info: Some(upload_info("e1")),
                image_url: Some(url),
                ..loaded(true)
            },
            false,
            "{ avatar: { source: 'upload', content_type: 'image/jpeg', etag: 'e1', updated_at: 1 }, \
               gravatar_enabled: true }",
        );
        let mount = mount(ProfileModal);
        let ready = wait_until(|| {
            query(&mount, "[data-testid='avatar-remove-button']").is_some()
                && attr(&mount, "#pa-21 .peer-avatar-slot", "data-avatar-state").as_deref()
                    == Some("image")
        })
        .await;
        let before = attr(&mount, ".avatar-preview", "data-avatar-state");
        click(&mount, "[data-testid='avatar-remove-button']");
        let removed = wait_until(|| {
            attr(&mount, ".avatar-preview", "data-avatar-state").as_deref() == Some("default")
                && query(&mount, "[data-testid='avatar-remove-button']").is_none()
        })
        .await;
        settle().await;
        let focused = active_element_id();
        let status = text(&mount, "[data-testid='avatar-status']");
        let source = text(&mount, "[data-testid='avatar-source-label']");
        let sibling = attr(&mount, "#pa-21 .peer-avatar-slot", "data-avatar-state");
        let deletes = calls()
            .iter()
            .filter(|c| c["method"] == "DELETE" && c["path"] == "/api/v1/user/avatar")
            .count();
        restore_fetch();
        mount.remove();
        assert!(ready);
        assert_eq!(before.as_deref(), Some("image"));
        assert!(removed, "the preview falls back to the default icon");
        assert_eq!(deletes, 1);
        assert_eq!(
            focused, UPLOAD_INPUT_ID,
            "focus moves off the vanished Remove button"
        );
        assert_eq!(status, ANNOUNCE_REMOVED);
        assert_eq!(source, "No avatar set. Others see the default icon.");
        assert_eq!(
            sibling.as_deref(),
            Some("default"),
            "this user's tile in another tab drops the image too"
        );
    }

    #[wasm_bindgen_test]
    async fn oversize_and_unsupported_files_are_refused_before_any_upload() {
        seed(
            loaded(false),
            false,
            "{ avatar: null, gravatar_enabled: false }",
        );
        let mount = mount(ProfileModal);
        let ready = wait_until(|| query(&mount, "#avatar-upload-input").is_some()).await;
        pick_files(
            "[new File([new Uint8Array(2 * 1024 * 1024 + 1)], 'big.png', { type: 'image/png' })]",
        );
        let too_large =
            wait_until(|| text(&mount, "[data-testid='avatar-upload-error']") == COPY_TOO_LARGE)
                .await;
        pick_files("[new File([new Uint8Array(10)], 'anim.gif', { type: 'image/gif' })]");
        let unsupported =
            wait_until(|| text(&mount, "[data-testid='avatar-upload-error']") == COPY_UNSUPPORTED)
                .await;
        let cleared = js(&format!(
            "return document.getElementById('{UPLOAD_INPUT_ID}').value === '';"
        ))
        .as_bool();
        let puts = calls().iter().filter(|c| c["method"] == "PUT").count();
        let gravatar_form = query(&mount, "#avatar-gravatar-input").is_some();
        restore_fetch();
        mount.remove();
        assert!(ready);
        assert!(too_large, "a file over 2 MB is refused with its copy");
        assert!(unsupported, "a GIF is refused with its copy");
        assert_eq!(
            cleared,
            Some(true),
            "the picker resets so the same file can be chosen again"
        );
        assert_eq!(puts, 0, "neither file reaches the server");
        assert!(
            !gravatar_form,
            "the Gravatar form is hidden while Gravatar is disabled"
        );
    }

    #[wasm_bindgen_test]
    async fn an_upload_is_cropped_encoded_and_shown_as_saved() {
        seed(
            loaded(false),
            false,
            "{ avatar: null, gravatar_enabled: false }",
        );
        let mount = mount(ProfileModal);
        let ready = wait_until(|| query(&mount, "#avatar-upload-input").is_some()).await;
        pick_png(300, 200).await;
        let saved =
            wait_until(|| text(&mount, "[data-testid='avatar-status']") == ANNOUNCE_UPLOADED_SMALL)
                .await;
        settle().await;
        let puts: Vec<_> = calls()
            .into_iter()
            .filter(|c| c["method"] == "PUT")
            .collect();
        let preview = settled_state(&mount, ".avatar-preview", "image").await;
        let source = text(&mount, "[data-testid='avatar-source-label']");
        let label = text(&mount, "label.avatar-btn");
        let fetched_stored = calls_to("/user/avatar/image").len();
        restore_fetch();
        mount.remove();
        assert!(ready);
        assert!(saved, "a 300x200 upload is saved with the small-image note");
        assert_eq!(puts.len(), 1, "{puts:?}");
        assert_eq!(
            puts[0]["type"], "image/webp",
            "the canvas re-encode is WebP"
        );
        assert!(puts[0]["size"].as_u64().unwrap_or(0) > 0);
        assert_eq!(preview.as_deref(), Some("image"));
        assert_eq!(source, "Using an uploaded image.");
        assert_eq!(label, "Replace image");
        assert_eq!(
            fetched_stored, 0,
            "in a meeting the image is read with the room token, never the browser's sign-in"
        );
    }

    #[allow(non_snake_case)]
    fn PreJoin() -> Element {
        provide_meeting();
        let toggle = use_signal(|| false);
        let camera_on = use_signal(|| false);
        CAMERA.with(|c| *c.borrow_mut() = Some(camera_on));
        let error = use_signal(|| None::<String>);
        let join: EventHandler<()> = use_callback(|_: ()| {});
        rsx! {
            PreJoinSettingsCard {
                ownership: MeetingOwnership::of(Some("owner@example.com"), Some("me@example.com")),
                is_guest: GUEST.with(Cell::get),
                meeting_id: "m1".to_string(),
                meeting_active: true,
                waiting_room_toggle: toggle,
                admitted_can_admit_toggle: toggle,
                end_on_host_leave_toggle: toggle,
                allow_guests_toggle: toggle,
                recording_allowed_for_all_toggle: toggle,
                chat_allowed_for_all_toggle: toggle,
                announcement_overrides: use_override_editor(use_signal(Default::default)),
                saving: toggle,
                toggle_error: error,
                connection_error: error,
                on_join: join,
                media_access_granted: true,
                camera_on,
            }
        }
    }

    #[wasm_bindgen_test]
    async fn the_pre_join_avatar_opens_and_closes_the_avatar_dialog() {
        seed(
            loaded(false),
            false,
            "{ avatar: null, gravatar_enabled: false }",
        );
        let mount = mount(PreJoin);
        let button = format!("[data-testid='{PREJOIN_AVATAR_BUTTON_TESTID}']");
        let ready = wait_until(|| {
            attr(
                &mount,
                &format!("{button} .peer-avatar-slot"),
                "data-avatar-state",
            )
            .as_deref()
                == Some("default")
        })
        .await;
        let label = attr(&mount, &button, "aria-label");
        let camera_text = text(&mount, ".prejoin-video-placeholder-text");
        click(&mount, &button);
        let opened = wait_until(|| query(&mount, "[data-testid='avatar-dialog']").is_some()).await;
        settle().await;
        let focus_on_open = active_element_id();
        press(UPLOAD_INPUT_ID, "Escape");
        let closed = wait_until(|| query(&mount, "[data-testid='avatar-dialog']").is_none()).await;
        let focus_on_close = gloo_utils::document()
            .active_element()
            .and_then(|el| el.get_attribute("data-testid"));
        restore_fetch();
        mount.remove();
        assert!(ready);
        assert_eq!(label.as_deref(), Some("Add avatar"));
        assert_eq!(camera_text, "Camera is off");
        assert!(opened);
        assert_eq!(focus_on_open, UPLOAD_INPUT_ID);
        assert!(closed, "Escape closes the dialog");
        assert_eq!(
            focus_on_close.as_deref(),
            Some(PREJOIN_AVATAR_BUTTON_TESTID)
        );
    }

    #[wasm_bindgen_test]
    async fn a_guest_pre_join_keeps_the_camera_off_glyph() {
        seed(
            OwnAvatar::default(),
            true,
            "{ avatar: null, gravatar_enabled: false }",
        );
        let mount = mount(PreJoin);
        let glyph = wait_until(|| {
            query(&mount, ".prejoin-video-placeholder svg.prejoin-toggle-icon").is_some()
        })
        .await;
        let button = query(
            &mount,
            &format!("[data-testid='{PREJOIN_AVATAR_BUTTON_TESTID}']"),
        )
        .is_some();
        restore_fetch();
        mount.remove();
        assert!(glyph);
        assert!(!button, "guests cannot set an avatar");
    }

    #[allow(non_snake_case)]
    fn SelfTile() -> Element {
        provide_meeting();
        let mut section = use_signal(|| None::<String>);
        let mut generation = use_signal(|| 0u32);
        let camera_on = use_signal(|| false);
        SELF_CAMERA.with(|c| *c.borrow_mut() = Some(camera_on));
        let focus = use_self_avatar_settings_focus();
        let on_edit: EventHandler<()> = use_callback(move |_: ()| {
            generation.set(generation() + 1);
            section.set(Some(PROFILE_SECTION.to_string()));
            focus.opened(generation(), focus_profile_tab);
        });
        let (grid, corner) = PLACEMENTS.with(Cell::get);
        let camera_toggle = CAMERA_TOGGLE.with(Cell::get);
        rsx! {
            if camera_toggle {
                CameraButton { enabled: camera_on(), available: true, onclick: move |_| {} }
            }
            div { id: "grid-container", tabindex: "-1",
                if grid && !camera_on() {
                    div { id: "self-grid",
                        SelfCameraOffPlaceholder { self_in_grid: true, on_edit_avatar: on_edit }
                    }
                }
                if corner && !camera_on() {
                    div { id: "self-corner",
                        SelfCameraOffPlaceholder { self_in_grid: false, on_edit_avatar: on_edit }
                    }
                }
            }
            button {
                id: "other-settings-opener",
                onclick: move |_| {
                    generation.set(generation() + 1);
                    section.set(Some("audio".to_string()));
                },
            }
            div {
                DeviceSettingsModal {
                    key: "{generation}",
                    microphones: Vec::new(),
                    cameras: Vec::new(),
                    speakers: Vec::new(),
                    selected_microphone_id: None,
                    selected_camera_id: None,
                    selected_speaker_id: None,
                    on_microphone_select: move |_| {},
                    on_camera_select: move |_| {},
                    on_speaker_select: move |_| {},
                    visible: section().is_some(),
                    on_close: move |_| {
                        section.set(None);
                        focus.closed(generation());
                    },
                    initial_section: section(),
                }
            }
        }
    }

    const PROFILE_TAB: &str = "settings-tab-profile";

    fn self_edit_button(placement: &str) -> String {
        format!("#{placement} [data-testid='{SELF_AVATAR_EDIT_BUTTON_TESTID}']")
    }

    fn mount_self_tile(grid: bool, corner: bool) -> web_sys::Element {
        PLACEMENTS.with(|p| p.set((grid, corner)));
        mount(SelfTile)
    }

    async fn focus_settles_on(id: &str) -> String {
        wait_until(|| active_element_id() == id).await;
        active_element_id()
    }

    #[wasm_bindgen_test]
    async fn the_self_tile_avatar_opens_profile_settings_and_gets_focus_back() {
        seed(
            loaded(false),
            false,
            "{ avatar: { source: 'upload', content_type: 'image/jpeg', etag: 'e1', updated_at: 1 }, \
               gravatar_enabled: false }",
        );
        let mount = mount_self_tile(true, false);
        let button = self_edit_button("self-grid");
        let ready = wait_until(|| {
            attr(
                &mount,
                &format!("{button} .peer-avatar-slot"),
                "data-avatar-state",
            )
            .as_deref()
                == Some("image")
        })
        .await;
        let label = attr(&mount, &button, "aria-label");
        let badge = attr(
            &mount,
            &format!("{button} .self-avatar-edit-badge"),
            "aria-hidden",
        );
        let closed_tab = active_tab(&mount);
        click(&mount, &button);
        let opened = wait_until(|| active_tab(&mount).is_some()).await;
        let tab = active_tab(&mount);
        let focus_on_open = focus_settles_on(PROFILE_TAB).await;
        press(PROFILE_TAB, "Escape");
        let closed = wait_until(|| active_tab(&mount).is_none()).await;
        let focus_on_close = focus_settles_on(SELF_AVATAR_EDIT_BUTTON_TESTID).await;
        restore_fetch();
        mount.remove();
        assert!(ready);
        assert_eq!(label.as_deref(), Some("Change avatar"));
        assert_eq!(badge.as_deref(), Some("true"));
        assert_eq!(closed_tab, None);
        assert!(opened);
        assert_eq!(tab.as_deref(), Some("settings-nav-profile"));
        assert_eq!(focus_on_open, PROFILE_TAB);
        assert!(closed, "Escape closes Settings");
        assert_eq!(focus_on_close, SELF_AVATAR_EDIT_BUTTON_TESTID);
    }

    #[wasm_bindgen_test]
    async fn an_editable_corner_without_an_avatar_shows_the_person_icon() {
        seed(
            loaded(false),
            false,
            "{ avatar: null, gravatar_enabled: false }",
        );
        let mount = mount_self_tile(false, true);
        let corner = self_edit_button("self-corner");
        let ready = wait_until(|| {
            attr(
                &mount,
                &format!("{corner} > .peer-avatar-slot"),
                "data-avatar-state",
            )
            .as_deref()
                == Some("default")
        })
        .await;
        let label = attr(&mount, &corner, "aria-label");
        let person = query(
            &mount,
            &format!("{corner} > .peer-avatar-slot.avatar-circle[data-avatar-state='default']"),
        )
        .is_some();
        let camera_glyph = query(&mount, "#self-corner .placeholder-content > svg").is_some()
            || query(&mount, &format!("{corner} > svg")).is_some();
        let text_outside = query(&mount, &format!("{corner} .placeholder-text")).is_none()
            && query(&mount, "#self-corner .placeholder-text").is_some();
        restore_fetch();
        mount.remove();
        assert!(ready);
        assert_eq!(label.as_deref(), Some("Add avatar"));
        assert!(person, "the button holds the person icon");
        assert!(!camera_glyph, "no camera-off glyph under the edit badge");
        assert!(
            text_outside,
            "\"Camera Off\" is not part of the button's name"
        );
    }

    #[wasm_bindgen_test]
    async fn the_self_tile_label_follows_the_meeting_user_not_the_browser_account() {
        seed(
            OwnAvatar {
                info: Some(upload_info("e9")),
                ..loaded(true)
            },
            false,
            OTHER_META,
        );
        js("window.__meetingAvatar = null;");
        let mount = mount_self_tile(true, false);
        let button = self_edit_button("self-grid");
        let settled = wait_until(|| {
            attr(
                &mount,
                &format!("{button} .peer-avatar-slot"),
                "data-avatar-state",
            )
            .as_deref()
                == Some("default")
                && own_meta_reads() == 1
        })
        .await;
        settle().await;
        let label = attr(&mount, &button, "aria-label");
        click(&mount, &button);
        let opened = wait_until(|| query(&mount, ELSEWHERE).is_some()).await;
        let upload_label = text(&mount, "label.avatar-btn");
        let remove = query(&mount, "[data-testid='avatar-remove-button']").is_some();
        restore_fetch();
        mount.remove();
        assert!(settled);
        assert_eq!(
            label.as_deref(),
            Some("Add avatar"),
            "the meeting user has no avatar, whatever the browser account has"
        );
        assert!(opened, "the button still opens Profile, which explains");
        assert_eq!(upload_label, "Upload image", "Profile agrees with the tile");
        assert!(!remove, "there is no meeting user's avatar to remove");
    }

    #[wasm_bindgen_test]
    async fn the_avatar_button_does_not_say_add_while_the_avatar_is_loading() {
        seed(loaded(false), false, ME_META);
        install_fetch(&format!(
            "if (call.path.indexOf('/meetings/') >= 0) {{ return hold(jpeg); }} {ROUTES}"
        ));
        let mount = mount_self_tile(true, false);
        let button = self_edit_button("self-grid");
        let state = || {
            attr(
                &mount,
                &format!("{button} .peer-avatar-slot"),
                "data-avatar-state",
            )
        };
        let loading = wait_until(|| state().as_deref() == Some("loading")).await;
        let while_loading = attr(&mount, &button, "aria-label");
        release_held();
        let shown = wait_until(|| state().as_deref() == Some("image")).await;
        let once_shown = attr(&mount, &button, "aria-label");
        restore_fetch();
        mount.remove();
        assert!(loading && shown);
        assert_eq!(
            while_loading.as_deref(),
            Some("Change avatar"),
            "a user whose avatar has not loaded yet is not told to add one"
        );
        assert_eq!(once_shown.as_deref(), Some("Change avatar"));
    }

    #[wasm_bindgen_test]
    async fn a_guest_self_tile_has_no_avatar_button() {
        seed(
            OwnAvatar::default(),
            true,
            "{ avatar: null, gravatar_enabled: false }",
        );
        let mount = mount_self_tile(true, true);
        let rendered = wait_until(|| {
            query(
                &mount,
                "#self-grid .placeholder-content > .peer-avatar-slot:not(.avatar-circle)",
            )
            .is_some()
                && query(&mount, "#self-corner .placeholder-content > svg").is_some()
        })
        .await;
        let buttons = query(&mount, "#self-grid button, #self-corner button").is_some();
        let badge = query(&mount, ".self-avatar-edit-badge").is_some();
        restore_fetch();
        mount.remove();
        assert!(rendered, "a guest's self tile renders as before");
        assert!(!buttons, "guests cannot set an avatar");
        assert!(!badge);
    }

    /// Opens Settings from the avatar, turns the camera on, closes Settings, and
    /// returns where focus lands (the testid, else the id).
    async fn focus_after_closing_with_the_camera_on(camera_toggle: bool) -> String {
        seed(
            loaded(false),
            false,
            "{ avatar: null, gravatar_enabled: false }",
        );
        CAMERA_TOGGLE.with(|t| t.set(camera_toggle));
        let mount = mount_self_tile(true, false);
        let button = self_edit_button("self-grid");
        wait_until(|| query(&mount, &button).is_some()).await;
        click(&mount, &button);
        let opened = focus_settles_on(PROFILE_TAB).await == PROFILE_TAB;
        let mut camera_on = SELF_CAMERA.with(|c| c.borrow().unwrap());
        camera_on.set(true);
        let unmounted = wait_until(|| query(&mount, &button).is_none()).await;
        press(PROFILE_TAB, "Escape");
        let expected = if camera_toggle {
            "camera-toggle-button"
        } else {
            "grid-container"
        };
        let landed = || focused_testid().unwrap_or_else(active_element_id);
        wait_until(|| landed() == expected).await;
        let focus = landed();
        restore_fetch();
        mount.remove();
        CAMERA_TOGGLE.with(|t| t.set(true));
        assert!(opened && unmounted);
        focus
    }

    #[wasm_bindgen_test]
    async fn closing_settings_after_the_camera_turned_on_focuses_the_camera_toggle() {
        assert_eq!(
            focus_after_closing_with_the_camera_on(true).await,
            "camera-toggle-button"
        );
        assert_eq!(
            focus_after_closing_with_the_camera_on(false).await,
            "grid-container",
            "the grid is the last resort"
        );
    }

    #[wasm_bindgen_test]
    async fn settings_opened_elsewhere_never_sends_focus_to_the_avatar() {
        seed(
            loaded(false),
            false,
            "{ avatar: null, gravatar_enabled: false }",
        );
        let mount = mount_self_tile(true, false);
        let button = self_edit_button("self-grid");
        let ready = wait_until(|| query(&mount, &button).is_some()).await;
        click(&mount, &button);
        let focus_on_open = focus_settles_on(PROFILE_TAB).await;
        click(&mount, "#other-settings-opener");
        let reopened =
            wait_until(|| active_tab(&mount).as_deref() == Some("settings-nav-audio")).await;
        press("device-settings-dialog", "Escape");
        let closed = wait_until(|| active_tab(&mount).is_none()).await;
        settle().await;
        let focus_on_close = active_element_id();
        restore_fetch();
        mount.remove();
        assert!(ready);
        assert_eq!(focus_on_open, PROFILE_TAB);
        assert!(reopened);
        assert!(closed);
        assert_ne!(focus_on_close, SELF_AVATAR_EDIT_BUTTON_TESTID);
    }

    #[allow(non_snake_case)]
    fn StyledSelfTile() -> Element {
        provide_meeting();
        rsx! {
            nav {
                class: "host",
                "data-self-placement": "grid",
                style: "--tile-h: 100px",
                SelfCameraOffPlaceholder { self_in_grid: true, on_edit_avatar: move |_| {} }
            }
        }
    }

    fn self_button_and_badge() -> (String, String) {
        let button = format!("[data-testid='{SELF_AVATAR_EDIT_BUTTON_TESTID}']");
        let badge = format!("{button} .self-avatar-edit-badge");
        (button, badge)
    }

    /// Empty when `selector` is not rendered, so a failing test still removes its stylesheet.
    fn computed(mount: &web_sys::Element, selector: &str, pseudo: &str, property: &str) -> String {
        query(mount, selector)
            .and_then(|el| {
                gloo_utils::window()
                    .get_computed_style_with_pseudo_elt(&el, pseudo)
                    .ok()
                    .flatten()
            })
            .and_then(|style| style.get_property_value(property).ok())
            .unwrap_or_default()
    }

    fn width(mount: &web_sys::Element, selector: &str, pseudo: &str) -> f64 {
        computed(mount, selector, pseudo, "width")
            .trim_end_matches("px")
            .parse()
            .unwrap_or(f64::NAN)
    }

    const APP_CSS: &str = concat!(
        include_str!("../../static/style.css"),
        include_str!("../../static/global.css"),
    );

    fn install_styles(css: &str) -> web_sys::Element {
        let style = gloo_utils::document().create_element("style").unwrap();
        style.set_text_content(Some(css));
        gloo_utils::document()
            .head()
            .unwrap()
            .append_child(&style)
            .unwrap();
        style
    }

    #[wasm_bindgen_test]
    async fn a_coarse_pointer_widens_the_target_but_not_the_avatar() {
        seed(
            loaded(false),
            false,
            "{ avatar: null, gravatar_enabled: false }",
        );
        let style = install_styles(&APP_CSS.replace("(pointer: coarse)", "all"));
        let (button, _) = self_button_and_badge();
        let mount = mount(StyledSelfTile);
        let ready = wait_until(|| query(&mount, &button).is_some()).await;
        settle().await;
        let (box_w, circle_w, target_w) = (
            width(&mount, &button, ""),
            width(&mount, &format!("{button} > .avatar-circle"), ""),
            width(&mount, &button, "::before"),
        );
        style.remove();
        restore_fetch();
        mount.remove();
        assert!(ready);
        assert!(circle_w < 44.0, "the fixture avatar is {circle_w}px");
        assert_eq!(box_w, circle_w, "the badge's box stays the avatar's");
        assert_eq!(target_w, 44.0);
    }

    #[wasm_bindgen_test]
    async fn the_edit_badge_appears_on_keyboard_focus() {
        seed(
            loaded(false),
            false,
            "{ avatar: null, gravatar_enabled: false }",
        );
        let style = install_styles(&APP_CSS.replace("(hover: none)", "not all"));
        let (button, badge) = self_button_and_badge();
        let mount = mount(StyledSelfTile);
        let ready = wait_until(|| query(&mount, &button).is_some()).await;
        let resting = computed(&mount, &badge, "", "display");
        if let Some(el) = query(&mount, &button) {
            let _ = el.unchecked_into::<web_sys::HtmlElement>().focus();
        }
        settle().await;
        let focused = computed(&mount, &badge, "", "display");
        let (button_w, circle_w, badge_w, icon_w) = (
            width(&mount, &button, ""),
            width(&mount, &format!("{button} > .avatar-circle"), ""),
            width(&mount, &badge, ""),
            width(&mount, &format!("{badge} svg"), ""),
        );
        let icon_fill = computed(&mount, &format!("{badge} svg"), "", "fill");
        style.remove();
        restore_fetch();
        mount.remove();
        assert!(ready);
        assert_eq!(resting, "none", "hidden until hover or focus");
        assert_eq!(focused, "grid");
        assert_eq!(
            button_w, circle_w,
            "the badge is anchored to the drawn circle"
        );
        assert!(
            (14.0..=24.0).contains(&badge_w) && badge_w < circle_w / 2.0,
            "badge {badge_w}px on a {circle_w}px avatar"
        );
        assert!(
            icon_w > 0.0 && icon_w < badge_w,
            "pencil {icon_w}px in a {badge_w}px badge"
        );
        assert_eq!(icon_fill, "none");
    }

    #[wasm_bindgen_test]
    async fn a_touch_screen_always_shows_the_edit_badge() {
        seed(
            loaded(false),
            false,
            "{ avatar: null, gravatar_enabled: false }",
        );
        let style = install_styles(&APP_CSS.replace("(hover: none)", "all"));
        let (button, badge) = self_button_and_badge();
        let mount = mount(StyledSelfTile);
        let ready = wait_until(|| query(&mount, &button).is_some()).await;
        let resting = computed(&mount, &badge, "", "display");
        style.remove();
        restore_fetch();
        mount.remove();
        assert!(ready);
        assert_eq!(resting, "grid");
    }

    #[wasm_bindgen_test]
    async fn a_gravatar_save_is_shown_from_the_api_and_never_loads_from_gravatar() {
        seed(
            loaded(true),
            false,
            "{ avatar: null, gravatar_enabled: true }",
        );
        let mount = mount(ProfileModal);
        let ready = wait_until(|| {
            query(&mount, "#avatar-gravatar-input").is_some()
                && attr(&mount, "#pa-21 .peer-avatar-slot", "data-avatar-state").as_deref()
                    == Some("default")
        })
        .await;
        js(
            "var el = document.getElementById('avatar-gravatar-input'); \
            el.value = 'Me@Example.com'; el.dispatchEvent(new Event('input', { bubbles: true }));",
        );
        let save = "[data-testid='avatar-gravatar-save']";
        let enabled =
            wait_until(|| attr(&mount, save, "aria-disabled").as_deref() == Some("false")).await;
        click(&mount, save);
        let saved =
            wait_until(|| text(&mount, "[data-testid='avatar-status']") == ANNOUNCE_GRAVATAR).await;
        settle().await;
        let puts = puts_to("/api/v1/user/avatar/gravatar");
        let hosts: Vec<String> = calls()
            .iter()
            .map(|c| c["host"].as_str().unwrap_or_default().to_string())
            .collect();
        let images = mount.query_selector_all("img").unwrap();
        let srcs: Vec<String> = (0..images.length())
            .filter_map(|i| images.item(i))
            .filter_map(|node| node.dyn_into::<web_sys::Element>().ok())
            .map(|img| img.get_attribute("src").unwrap_or_default())
            .collect();
        let preview = settled_state(&mount, ".avatar-preview", "image").await;
        let tile_state = || attr(&mount, "#pa-21 .peer-avatar-slot", "data-avatar-state");
        wait_until(|| tile_state().as_deref() == Some("image")).await;
        let tile = tile_state();
        restore_fetch();
        mount.remove();
        assert!(ready);
        assert!(enabled);
        assert!(saved, "the save is announced");
        assert_eq!(puts.len(), 1, "{puts:?}");
        assert_eq!(puts[0]["body"], r#"{"gravatar":"Me@Example.com"}"#);
        assert!(hosts.iter().all(|h| h == "avatar.test"), "{hosts:?}");
        assert_eq!(preview.as_deref(), Some("image"));
        assert_eq!(tile.as_deref(), Some("image"));
        assert!(
            srcs.len() >= 2,
            "the preview and the tile render images: {srcs:?}"
        );
        assert!(
            srcs.iter().all(|src| src.starts_with("blob:")),
            "every image comes from a blob: URL: {srcs:?}"
        );
    }

    const ME_META: &str = "{ avatar: { source: 'upload', content_type: 'image/jpeg', etag: 'e1', \
        updated_at: 1 }, gravatar_enabled: true, user_id: 'me@example.com' }";
    const OTHER_META: &str =
        "{ avatar: { source: 'upload', content_type: 'image/jpeg', etag: 'e9', \
        updated_at: 9 }, gravatar_enabled: true, user_id: 'other@example.com' }";
    const ELSEWHERE: &str = "[data-testid='avatar-signed-in-elsewhere']";

    /// Every own-avatar write sent so far, as (method, path, expected_user_id).
    fn writes() -> Vec<(String, String, Option<String>)> {
        calls()
            .into_iter()
            .filter(|c| c["method"] != "GET")
            .map(|c| {
                (
                    c["method"].as_str().unwrap_or_default().to_string(),
                    c["path"].as_str().unwrap_or_default().to_string(),
                    c["expected"].as_str().map(str::to_string),
                )
            })
            .collect()
    }

    fn enter_gravatar(value: &str) {
        js(&format!(
            "var el = document.getElementById('avatar-gravatar-input'); \
             el.value = '{value}'; el.dispatchEvent(new Event('input', {{ bubbles: true }}));"
        ));
    }

    #[wasm_bindgen_test]
    async fn in_meeting_writes_name_the_meeting_user() {
        seed(loaded(true), false, ME_META);
        let mount = mount(ProfileModal);
        let status = |mount: &web_sys::Element| text(mount, "[data-testid='avatar-status']");
        let ready =
            wait_until(|| query(&mount, "[data-testid='avatar-remove-button']").is_some()).await;
        pick_png(300, 200).await;
        let uploaded = wait_until(|| status(&mount) == ANNOUNCE_UPLOADED_SMALL).await;
        enter_gravatar("me@example.com");
        let save = "[data-testid='avatar-gravatar-save']";
        wait_until(|| attr(&mount, save, "aria-disabled").as_deref() == Some("false")).await;
        click(&mount, save);
        let linked = wait_until(|| status(&mount) == ANNOUNCE_GRAVATAR).await;
        click(&mount, "[data-testid='avatar-remove-button']");
        let removed = wait_until(|| status(&mount) == ANNOUNCE_REMOVED).await;
        let sent = writes();
        let browser_image_reads = calls_to("/user/avatar/image").len();
        restore_fetch();
        mount.remove();
        assert!(ready && uploaded && linked && removed);
        assert_eq!(
            browser_image_reads, 0,
            "the meeting never reads the own image through the browser's sign-in"
        );
        let me = Some("me@example.com".to_string());
        assert_eq!(
            sent,
            vec![
                (
                    "PUT".to_string(),
                    "/api/v1/user/avatar".to_string(),
                    me.clone()
                ),
                (
                    "PUT".to_string(),
                    "/api/v1/user/avatar/gravatar".to_string(),
                    me.clone()
                ),
                ("DELETE".to_string(), "/api/v1/user/avatar".to_string(), me),
            ]
        );
    }

    #[allow(non_snake_case)]
    fn PanelWithoutMeeting() -> Element {
        let own = use_signal(|| OWN_SEED.with(|s| s.borrow().clone()));
        use_context_provider(|| OwnAvatarCtx(own));
        rsx! {
            AvatarSettingsPanel { variant: AvatarPanelVariant::Dialog, can_edit: true }
        }
    }

    #[wasm_bindgen_test]
    async fn writes_without_a_meeting_user_name_no_one() {
        seed(
            loaded(false),
            false,
            "{ avatar: null, gravatar_enabled: false }",
        );
        let mount = mount(PanelWithoutMeeting);
        let ready = wait_until(|| query(&mount, "#avatar-upload-input").is_some()).await;
        pick_png(300, 200).await;
        let uploaded =
            wait_until(|| text(&mount, "[data-testid='avatar-status']") == ANNOUNCE_UPLOADED_SMALL)
                .await;
        let sent = writes();
        restore_fetch();
        mount.remove();
        assert!(ready && uploaded);
        assert_eq!(
            sent,
            vec![("PUT".to_string(), "/api/v1/user/avatar".to_string(), None)]
        );
    }

    #[wasm_bindgen_test]
    async fn a_write_refused_for_another_account_paints_nothing() {
        let url = real_jpeg_url().await;
        seed(
            OwnAvatar {
                info: Some(upload_info("e1")),
                image_url: Some(url.clone()),
                ..loaded(true)
            },
            false,
            ME_META,
        );
        js("window.__refuseWrites = true;");
        let mount = mount(ProfileModal);
        let src = |selector: &str| attr(&mount, &format!("{selector} img"), "src");
        let ready = wait_until(|| {
            query(&mount, "[data-testid='avatar-remove-button']").is_some()
                && src(".avatar-preview").is_some()
                && src("#pa-21").is_some()
                && src("#self-tile").is_some()
                && own_meta_reads() == 2
        })
        .await;
        let before = [src(".avatar-preview"), src("#pa-21"), src("#self-tile")];
        let images_before = calls_to("/user/avatar/image").len();
        pick_png(300, 200).await;
        let refused = wait_until(|| query(&mount, ELSEWHERE).is_some()).await;
        settle().await;
        let after = [src(".avatar-preview"), src("#pa-21"), src("#self-tile")];
        let notice = text(&mount, ELSEWHERE);
        let upload_error = query(&mount, "[data-testid='avatar-upload-error']").is_some();
        let disabled = [
            attr(&mount, "#avatar-upload-input", "aria-disabled"),
            attr(
                &mount,
                "[data-testid='avatar-remove-button']",
                "aria-disabled",
            ),
            attr(
                &mount,
                "[data-testid='avatar-gravatar-save']",
                "aria-disabled",
            ),
        ];
        let described = [
            "#avatar-upload-input",
            "[data-testid='avatar-remove-button']",
            "#avatar-gravatar-input",
            "[data-testid='avatar-gravatar-save']",
        ]
        .map(|selector| attr(&mount, selector, "aria-describedby").unwrap_or_default());
        click(&mount, "[data-testid='avatar-remove-button']");
        settle().await;
        let sent = writes();
        let images_after = calls_to("/user/avatar/image").len();
        restore_fetch();
        mount.remove();
        assert!(ready);
        assert_eq!(
            before[0], before[1],
            "the preview shows the meeting user's image, like the tiles"
        );
        assert_ne!(before[0].as_deref(), Some(url.as_str()));
        assert!(
            described
                .iter()
                .all(|ids| ids.split(' ').any(|id| id == SIGNED_IN_ELSEWHERE_ID)),
            "{described:?}"
        );
        assert!(refused, "the refusal is explained");
        assert_eq!(notice, COPY_SIGNED_IN_ELSEWHERE);
        assert!(!upload_error, "the notice is the only message");
        assert_eq!(
            after, before,
            "the preview and every own tile keep their image"
        );
        assert_eq!(
            images_after, images_before,
            "nothing is re-read after a refusal"
        );
        assert!(
            disabled.iter().all(|d| d.as_deref() == Some("true")),
            "{disabled:?}"
        );
        assert_eq!(
            sent,
            vec![(
                "PUT".to_string(),
                "/api/v1/user/avatar".to_string(),
                Some("me@example.com".to_string())
            )],
            "remove is not sent once the account is known to differ"
        );
    }

    #[wasm_bindgen_test]
    async fn another_accounts_avatar_is_never_shown_or_written() {
        seed(loaded(true), false, OTHER_META);
        js("window.__meetingAvatar = { etag: 'm1' };");
        let mount = mount(ProfileModal);
        let flagged = wait_until(|| query(&mount, ELSEWHERE).is_some()).await;
        let self_tile = wait_until(|| {
            attr(&mount, "#self-tile img", "src").is_some_and(|src| src.starts_with("blob:"))
                && attr(&mount, ".avatar-preview img", "src").is_some()
        })
        .await;
        let shown = [
            attr(&mount, ".avatar-preview img", "src"),
            attr(&mount, "#self-tile img", "src"),
        ];
        let upload_label = text(&mount, "label.avatar-btn");
        let disabled = [
            attr(&mount, "#avatar-upload-input", "aria-disabled"),
            attr(
                &mount,
                "[data-testid='avatar-gravatar-save']",
                "aria-disabled",
            ),
        ];
        pick_png(300, 200).await;
        enter_gravatar("other@example.com");
        settle().await;
        click(&mount, "[data-testid='avatar-gravatar-save']");
        settle().await;
        let sent = writes();
        let other_image_reads = calls_to("/user/avatar/image").len();
        let meeting_reads: Vec<_> = calls()
            .into_iter()
            .filter(|c| c["path"].as_str().is_some_and(|p| p.contains("/meetings/")))
            .map(|c| c["user_id"].as_str().unwrap_or_default().to_string())
            .collect();
        restore_fetch();
        mount.remove();
        assert!(flagged, "the panel says another account is signed in");
        assert!(self_tile, "the self tile shows the meeting user's image");
        assert_eq!(meeting_reads, vec!["me@example.com".to_string()]);
        assert_eq!(
            shown[0], shown[1],
            "the preview shows the meeting user's image, like the self tile"
        );
        assert_eq!(
            upload_label, "Replace image",
            "the label follows the meeting user's avatar"
        );
        assert_eq!(other_image_reads, 0, "its image is not even fetched");
        assert!(
            disabled.iter().all(|d| d.as_deref() == Some("true")),
            "{disabled:?}"
        );
        assert!(sent.is_empty(), "{sent:?}");
    }

    #[wasm_bindgen_test]
    async fn an_older_server_without_a_user_id_is_trusted_as_before() {
        seed(
            loaded(false),
            false,
            "{ avatar: { source: 'upload', content_type: 'image/jpeg', etag: 'e9', updated_at: 9 }, \
               gravatar_enabled: false }",
        );
        let mount = mount(ProfileModal);
        let shown = wait_until(|| {
            attr(&mount, ".avatar-preview", "data-avatar-state").as_deref() == Some("image")
        })
        .await;
        settle().await;
        let notice = query(&mount, ELSEWHERE).is_some();
        let upload = attr(&mount, "#avatar-upload-input", "aria-disabled");
        restore_fetch();
        mount.remove();
        assert!(shown, "the avatar is shown as before");
        assert!(!notice);
        assert_eq!(upload.as_deref(), Some("false"));
    }

    const MEETING_REPLIES: &str = "\
        var png8 = function() { return new Promise(function(resolve) { \
            var c = document.createElement('canvas'); c.width = 8; c.height = 8; \
            c.getContext('2d').fillRect(0, 0, 8, 8); \
            c.toBlob(function(b) { resolve(new Response(b, { status: 200, \
                headers: { 'content-type': 'image/png' } })); }, 'image/png'); }); }; \
        var noAvatar = function() { return json(404, { success: false, \
            result: { code: 'AVATAR_NOT_FOUND', message: 'none' } }); };";

    /// Before any save the meeting lookup finds no avatar; after one it serves an 8×8 PNG, while
    /// the browser account's own image (ROUTES) stays a 4×4 JPEG.
    const MEETING_IMAGE_AFTER_SAVE: &str = "\
        if (call.path.indexOf('/meetings/') >= 0) { return window.__saved ? png8() : noAvatar(); } \
        if (call.method === 'PUT') { window.__saved = true; }";

    /// As above, but each meeting lookup is held and answers with what the server held when it
    /// was asked.
    const MEETING_IMAGE_HELD: &str = "\
        if (call.path.indexOf('/meetings/') >= 0) { var saved = window.__saved; \
            return hold(function() { return saved ? png8() : noAvatar(); }); } \
        if (call.method === 'PUT') { window.__saved = true; }";

    fn release_first_held() -> bool {
        js("var reply = (window.__avatarHeld || []).shift(); if (reply) { reply(); } return !!reply;")
            .as_bool()
            .unwrap_or(false)
    }

    fn natural_width(selector: &str) -> f64 {
        js(&format!(
            "var img = document.querySelector(\"{selector}\"); return img ? img.naturalWidth : 0;"
        ))
        .as_f64()
        .unwrap_or_default()
    }

    #[wasm_bindgen_test]
    async fn after_a_save_the_tiles_show_the_meeting_users_image_not_the_browsers() {
        seed(
            loaded(false),
            false,
            "{ avatar: null, gravatar_enabled: false, user_id: 'me@example.com' }",
        );
        install_fetch(&format!(
            "{MEETING_REPLIES} {MEETING_IMAGE_AFTER_SAVE} {ROUTES}"
        ));
        js("window.__saved = false;");
        let mount = mount(ProfileModal);
        let ready = wait_until(|| {
            attr(&mount, "#self-tile .peer-avatar-slot", "data-avatar-state").as_deref()
                == Some("default")
        })
        .await;
        pick_png(300, 200).await;
        let saved =
            wait_until(|| text(&mount, "[data-testid='avatar-status']") == ANNOUNCE_UPLOADED_SMALL)
                .await;
        let shown = || {
            [
                natural_width("#self-tile img"),
                natural_width("#pa-21 img"),
                natural_width(".avatar-preview img"),
            ]
        };
        let from_meeting = wait_until(|| shown() == [8.0, 8.0, 8.0]).await;
        settle().await;
        let widths = shown();
        let browser_image_reads = calls_to("/user/avatar/image").len();
        restore_fetch();
        mount.remove();
        assert!(ready && saved);
        assert!(
            from_meeting,
            "the tiles and the preview show the meeting lookup's image: {widths:?}"
        );
        assert_eq!(browser_image_reads, 0);
    }

    #[allow(non_snake_case)]
    fn PanelOpenedLater() -> Element {
        provide_meeting();
        let open = use_signal(|| false);
        PANEL_OPEN.with(|p| *p.borrow_mut() = Some(open));
        rsx! {
            div { id: "self-tile", SelfAvatar {} }
            if open() {
                AvatarSettingsPanel { variant: AvatarPanelVariant::InMeeting, can_edit: true }
            }
        }
    }

    #[wasm_bindgen_test]
    async fn opening_the_panel_rechecks_the_account_before_any_save() {
        seed(loaded(true), false, ME_META);
        let mount = mount(PanelOpenedLater);
        let joined = wait_until(|| own_meta_reads() == 1).await;
        settle().await;
        js(&format!("window.__ownMeta = {OTHER_META};"));
        let mut open = PANEL_OPEN.with(|p| p.borrow().unwrap());
        open.set(true);
        let flagged = wait_until(|| query(&mount, ELSEWHERE).is_some()).await;
        let reads = own_meta_reads();
        let sent = writes();
        restore_fetch();
        mount.remove();
        assert!(joined, "joining read the account once");
        assert!(flagged, "the panel says so as soon as it opens");
        assert_eq!(reads, 2, "opening the panel read the account again");
        assert!(sent.is_empty(), "{sent:?}");
    }

    #[wasm_bindgen_test]
    async fn a_lookup_older_than_a_save_cannot_replace_its_image() {
        seed(
            loaded(false),
            false,
            "{ avatar: null, gravatar_enabled: false, user_id: 'me@example.com' }",
        );
        install_fetch(&format!("{MEETING_REPLIES} {MEETING_IMAGE_HELD} {ROUTES}"));
        js("window.__saved = false;");
        let mount = mount(ProfileModal);
        let meeting_calls = || calls_to("/meetings/m1/avatar").len();
        let first_held = wait_until(|| meeting_calls() == 1).await;
        pick_png(300, 200).await;
        let reload_held = wait_until(|| meeting_calls() == 2).await;
        let stale_released = release_first_held();
        settle().await;
        settle().await;
        let fresh_released = release_first_held();
        let shown = wait_until(|| natural_width("#self-tile img") == 8.0).await;
        settle().await;
        let width = natural_width("#self-tile img");
        restore_fetch();
        mount.remove();
        assert!(first_held && reload_held && stale_released && fresh_released);
        assert!(
            shown,
            "the lookup sent before the save is dropped, so the saved image stays: {width}"
        );
    }

    #[wasm_bindgen_test]
    async fn check_again_clears_the_notice_once_the_accounts_match() {
        seed(OwnAvatar::default(), false, OTHER_META);
        let mount = mount(ProfileModal);
        let check_again = "[data-testid='avatar-check-again']";
        let flagged = wait_until(|| query(&mount, ELSEWHERE).is_some()).await;
        let source = text(&mount, "[data-testid='avatar-source-label']");
        let retry = query(&mount, "[data-testid='avatar-retry-button']").is_some();
        js(&format!(
            "window.__noticeBefore = document.querySelector(\"{ELSEWHERE}\"); \
             document.querySelector(\"{check_again}\").focus();"
        ));
        click(&mount, check_again);
        let reannounced = wait_until(|| {
            js(&format!(
                "var n = document.querySelector(\"{ELSEWHERE}\"); \
                 return !!n && n !== window.__noticeBefore;"
            ))
            .as_bool()
                == Some(true)
        })
        .await;
        settle().await;
        let focus_kept = focused_testid();
        js(&format!("window.__ownMeta = {ME_META};"));
        click(&mount, check_again);
        let cleared = wait_until(|| query(&mount, ELSEWHERE).is_none()).await;
        settle().await;
        let upload = attr(&mount, "#avatar-upload-input", "aria-disabled");
        let button_gone = query(&mount, check_again).is_none();
        restore_fetch();
        mount.remove();
        assert!(flagged);
        assert_eq!(source, "", "no load failure is shown beside the notice");
        assert!(!retry, "Check again replaces Try again");
        assert!(
            reannounced,
            "a check that finds the other account again says so again"
        );
        assert_eq!(
            focus_kept.as_deref(),
            Some("avatar-check-again"),
            "focus stays on Check again while the notice stays"
        );
        assert!(
            cleared,
            "a check that finds the meeting user clears the notice"
        );
        assert_eq!(upload.as_deref(), Some("false"));
        assert!(button_gone);
    }

    #[wasm_bindgen_test]
    async fn check_again_says_it_is_checking_and_moves_focus_only_once_the_notice_clears() {
        seed(OwnAvatar::default(), false, OTHER_META);
        let mount = mount(ProfileModal);
        let check_again = "[data-testid='avatar-check-again']";
        let flagged =
            wait_until(|| query(&mount, ELSEWHERE).is_some() && own_meta_reads() == 2).await;
        js(&format!(
            "window.__ownMeta = {ME_META}; window.__holdMeta = true; \
             document.querySelector(\"{check_again}\").focus();"
        ));
        click(&mount, check_again);
        let held = wait_until(|| own_meta_reads() == 3).await;
        settle().await;
        let focus_during = focused_testid();
        let label_during = text(&mount, check_again);
        let busy_during = attr(&mount, check_again, "aria-busy");
        let source_during = query(&mount, SOURCE_LABEL).is_some();
        js("window.__holdMeta = false;");
        release_held();
        let cleared = wait_until(|| query(&mount, ELSEWHERE).is_none()).await;
        let focus_after = focus_settles_on(UPLOAD_INPUT_ID).await;
        let upload = attr(&mount, "#avatar-upload-input", "aria-disabled");
        let status = text(&mount, STATUS);
        restore_fetch();
        mount.remove();
        assert!(flagged && held);
        assert_eq!(
            focus_during.as_deref(),
            Some("avatar-check-again"),
            "focus does not move to a control that may stay disabled"
        );
        assert_eq!(label_during, "Checking…");
        assert_eq!(busy_during.as_deref(), Some("true"));
        assert!(!source_during, "no loading line shows above the notice");
        assert!(cleared);
        assert_eq!(
            focus_after, UPLOAD_INPUT_ID,
            "focus leaves the vanished button"
        );
        assert_eq!(upload.as_deref(), Some("false"));
        assert_eq!(status, ANNOUNCE_UNLOCKED);
    }

    #[wasm_bindgen_test]
    async fn opening_profile_while_flagged_does_not_read_the_notice_out_twice() {
        seed(loaded(true), false, OTHER_META);
        let mount = mount(PanelOpenedLater);
        let joined = wait_until(|| own_meta_reads() == 1).await;
        settle().await;
        js("window.__holdMeta = true;");
        let mut open = PANEL_OPEN.with(|p| p.borrow().unwrap());
        open.set(true);
        let shown =
            wait_until(|| query(&mount, ELSEWHERE).is_some() && own_meta_reads() == 2).await;
        js(&format!(
            "window.__noticeAtOpen = document.querySelector(\"{ELSEWHERE}\");"
        ));
        js("window.__holdMeta = false;");
        release_held();
        let answered =
            wait_until(|| js("return window.__avatarPending;").as_f64() == Some(0.0)).await;
        settle().await;
        let same = js(&format!(
            "return document.querySelector(\"{ELSEWHERE}\") === window.__noticeAtOpen;"
        ))
        .as_bool();
        restore_fetch();
        mount.remove();
        assert!(joined && shown && answered);
        assert_eq!(
            same,
            Some(true),
            "the open's check finding the same account keeps the notice it opened with"
        );
    }

    #[wasm_bindgen_test]
    async fn opening_profile_during_a_refresh_reads_the_account_again_after_it() {
        seed(loaded(true), false, ME_META);
        js("window.__holdMeta = true;");
        let mount = mount(PanelOpenedLater);
        let join_held = wait_until(|| own_meta_reads() == 1).await;
        js(&format!(
            "window.__ownMeta = {OTHER_META}; window.__holdMeta = false;"
        ));
        let mut open = PANEL_OPEN.with(|p| p.borrow().unwrap());
        open.set(true);
        TimeoutFuture::new(200).await;
        let reads_while_running = own_meta_reads();
        release_held();
        let flagged = wait_until(|| query(&mount, ELSEWHERE).is_some()).await;
        let reads = own_meta_reads();
        restore_fetch();
        mount.remove();
        assert!(join_held);
        assert_eq!(
            reads_while_running, 1,
            "the open waits for the read already running"
        );
        assert!(
            flagged,
            "then reads again and finds the account that signed in meanwhile"
        );
        assert_eq!(reads, 2);
    }

    #[wasm_bindgen_test]
    async fn opening_profile_looks_a_failed_own_image_up_again() {
        let _timings = TimingsGuard::set(fast_retry());
        seed(loaded(false), false, ME_META);
        install_fetch(&format!(
            "if (call.path.indexOf('/meetings/') >= 0 && window.__lookupsDown) {{ \
                return json(500, {{}}); }} {ROUTES}"
        ));
        js("window.__lookupsDown = true;");
        let mount = mount(PanelOpenedLater);
        let lookups = || calls_to("/meetings/m1/avatar").len();
        let state = || attr(&mount, "#self-tile .peer-avatar-slot", "data-avatar-state");
        let gave_up = wait_until(|| lookups() == 2).await;
        TimeoutFuture::new(200).await;
        let after_failures = (lookups(), state());
        js("window.__lookupsDown = false;");
        let mut open = PANEL_OPEN.with(|p| p.borrow().unwrap());
        open.set(true);
        let shown = wait_until(|| state().as_deref() == Some("image")).await;
        let total = lookups();
        restore_fetch();
        mount.remove();
        assert!(gave_up);
        assert_eq!(after_failures, (2, Some("default".to_string())));
        assert!(shown, "opening Profile looks the image up again");
        assert_eq!(total, 3);
    }

    /// After a save, each meeting lookup is held and answers 500 once released.
    const LOOKUPS_HELD_AFTER_SAVE: &str = "\
        if (call.path.indexOf('/meetings/') >= 0 && window.__saved) { \
            return hold(function() { return json(500, {}); }); } \
        if (call.method === 'PUT') { window.__saved = true; }";

    #[wasm_bindgen_test]
    async fn an_upload_shows_on_every_own_tile_at_once_and_stays_if_the_lookup_fails() {
        let _timings = TimingsGuard::set(fast_retry());
        seed(loaded(false), false, ME_META);
        install_fetch(&format!("{LOOKUPS_HELD_AFTER_SAVE} {ROUTES}"));
        js("window.__saved = false;");
        let mount = mount(ProfileModal);
        let shown = || {
            [
                natural_width("#self-tile img"),
                natural_width("#pa-21 img"),
                natural_width(".avatar-preview img"),
            ]
        };
        let lookups = || calls_to("/meetings/m1/avatar").len();
        let before = wait_until(|| shown() == [16.0; 3] && own_meta_reads() == 2).await;
        pick_png(300, 200).await;
        let saved = wait_until(|| text(&mount, STATUS) == ANNOUNCE_UPLOADED_SMALL).await;
        let confirming = wait_until(|| lookups() == 2).await;
        let at_once = wait_until(|| shown() == [200.0; 3]).await;
        let widths_at_once = shown();
        release_held();
        let retried = wait_until(|| lookups() == 3).await;
        settle().await;
        let after_failure = shown();
        restore_fetch();
        mount.remove();
        assert!(before && saved && confirming);
        assert!(
            at_once,
            "the uploaded image shows before the meeting lookup answers: {widths_at_once:?}"
        );
        assert!(retried, "a failed lookup is retried");
        assert_eq!(
            after_failure, [200.0; 3],
            "and the uploaded image stays meanwhile"
        );
    }

    /// The first meeting lookup after a Gravatar save fails; the rest are answered by ROUTES.
    const FIRST_LOOKUP_AFTER_GRAVATAR_FAILS: &str = "\
        if (call.path.indexOf('/meetings/') >= 0 && window.__failNextLookup) { \
            window.__failNextLookup = false; return json(500, {}); } \
        if (call.path === '/api/v1/user/avatar/gravatar') { window.__failNextLookup = true; }";

    #[wasm_bindgen_test]
    async fn a_gravatar_save_whose_lookup_fails_keeps_the_old_image_then_looks_up_again() {
        let _timings = TimingsGuard::set(AvatarTimings {
            retry_ms: 500,
            retry_jitter_ms: 0,
            ..AVATAR_TIMINGS
        });
        seed(loaded(true), false, ME_META);
        install_fetch(&format!("{FIRST_LOOKUP_AFTER_GRAVATAR_FAILS} {ROUTES}"));
        js("window.__failNextLookup = false;");
        let mount = mount(ProfileModal);
        let tile = || attr(&mount, "#pa-21 img.peer-avatar", "src");
        let lookups = || calls_to("/meetings/m1/avatar").len();
        let ready =
            wait_until(|| tile().is_some() && query(&mount, "#avatar-gravatar-input").is_some())
                .await;
        let before = tile();
        enter_gravatar("me@example.com");
        let save = "[data-testid='avatar-gravatar-save']";
        wait_until(|| attr(&mount, save, "aria-disabled").as_deref() == Some("false")).await;
        click(&mount, save);
        let saved = wait_until(|| text(&mount, STATUS) == ANNOUNCE_GRAVATAR).await;
        let failed = wait_until(|| lookups() == 2).await;
        settle().await;
        let kept = tile();
        let replaced = wait_until(|| tile().is_some_and(|src| Some(src) != before)).await;
        let total = lookups();
        restore_fetch();
        mount.remove();
        assert!(ready && saved && failed);
        assert_eq!(kept, before, "a failed lookup keeps the image shown");
        assert!(replaced, "the lookup is retried and shows the saved image");
        assert_eq!(total, 3);
    }

    #[wasm_bindgen_test]
    async fn an_unconfirmed_save_that_finds_another_account_shows_only_the_notice() {
        let _timings = short_save_timeout();
        seed(
            loaded(false),
            false,
            "{ avatar: null, gravatar_enabled: false, user_id: 'me@example.com' }",
        );
        js("window.__holdUpload = true;");
        let mount = mount(ProfileModal);
        let ready =
            wait_until(|| query(&mount, "#avatar-upload-input").is_some() && own_meta_reads() == 2)
                .await;
        pick_png(300, 300).await;
        let sent = wait_until(|| puts_to("/api/v1/user/avatar").len() == 1).await;
        js(&format!("window.__ownMeta = {OTHER_META};"));
        let flagged = wait_until(|| query(&mount, ELSEWHERE).is_some()).await;
        settle().await;
        let beside_notice = query(&mount, UPLOAD_ERROR).map(|el| el.text_content());
        let lookups = || calls_to("/meetings/m1/avatar").len();
        let lookups_flagged = lookups();
        js(&format!("window.__ownMeta = {ME_META};"));
        click(&mount, "[data-testid='avatar-check-again']");
        let cleared = wait_until(|| query(&mount, ELSEWHERE).is_none()).await;
        let reloaded = wait_until(|| lookups() == lookups_flagged + 1).await;
        settle().await;
        let after_notice = query(&mount, UPLOAD_ERROR).map(|el| el.text_content());
        release_held();
        restore_fetch();
        mount.remove();
        assert!(ready && sent && flagged && cleared);
        assert_eq!(beside_notice, None, "the notice is the only message");
        assert_eq!(
            after_notice, None,
            "the older alert does not come back when the notice clears"
        );
        assert!(
            reloaded,
            "the tiles are looked up again in case the unconfirmed save landed"
        );
    }

    #[wasm_bindgen_test]
    async fn the_gravatar_field_is_not_marked_invalid_behind_the_notice() {
        let _timings = short_save_timeout();
        seed(
            loaded(true),
            false,
            "{ avatar: null, gravatar_enabled: true, user_id: 'me@example.com' }",
        );
        js("window.__holdGravatar = true;");
        let mount = mount(ProfileModal);
        let field = "#avatar-gravatar-input";
        let ready = wait_until(|| query(&mount, field).is_some() && own_meta_reads() == 2).await;
        enter_gravatar("me@example.com");
        let save = "[data-testid='avatar-gravatar-save']";
        wait_until(|| attr(&mount, save, "aria-disabled").as_deref() == Some("false")).await;
        click(&mount, save);
        let sent = wait_until(|| puts_to("/api/v1/user/avatar/gravatar").len() == 1).await;
        js(&format!("window.__ownMeta = {OTHER_META};"));
        let flagged = wait_until(|| query(&mount, ELSEWHERE).is_some()).await;
        settle().await;
        let invalid = attr(&mount, field, "aria-invalid");
        let class = attr(&mount, field, "class").unwrap_or_default();
        release_held();
        restore_fetch();
        mount.remove();
        assert!(ready && sent && flagged);
        assert_eq!(
            invalid.as_deref(),
            Some("false"),
            "no invalid state without its message"
        );
        assert!(!class.contains("input-apple--invalid"), "{class}");
    }

    #[wasm_bindgen_test]
    async fn check_again_gives_up_on_a_read_that_never_answers() {
        let _timings = TimingsGuard::set(AvatarTimings {
            lookup_timeout_ms: 300,
            ..AVATAR_TIMINGS
        });
        seed(OwnAvatar::default(), false, OTHER_META);
        let mount = mount(ProfileModal);
        let check_again = "[data-testid='avatar-check-again']";
        let flagged =
            wait_until(|| query(&mount, ELSEWHERE).is_some() && own_meta_reads() == 2).await;
        js("window.__holdMeta = true;");
        click(&mount, check_again);
        let checking = wait_until(|| text(&mount, check_again) == "Checking…").await;
        let gave_up = wait_until(|| text(&mount, check_again) == "Check again").await;
        let busy = attr(&mount, check_again, "aria-busy");
        let still_flagged = query(&mount, ELSEWHERE).is_some();
        js("window.__holdMeta = false;");
        release_held();
        restore_fetch();
        mount.remove();
        assert!(flagged && checking);
        assert!(gave_up, "Check again works again once the read times out");
        assert_eq!(busy.as_deref(), Some("false"));
        assert!(still_flagged, "nothing was verified, so the notice stays");
    }

    #[wasm_bindgen_test]
    async fn a_check_that_could_not_read_the_account_does_not_repeat_the_notice() {
        seed(OwnAvatar::default(), false, OTHER_META);
        install_fetch(&format!(
            "if (window.__metaDown && call.method === 'GET' \
                && call.path === '/api/v1/user/avatar') {{ return json(500, {{}}); }} {ROUTES}"
        ));
        js("window.__metaDown = false;");
        let mount = mount(ProfileModal);
        let check_again = "[data-testid='avatar-check-again']";
        let flagged =
            wait_until(|| query(&mount, ELSEWHERE).is_some() && own_meta_reads() == 2).await;
        js(&format!(
            "window.__metaDown = true; \
             window.__noticeBefore = document.querySelector(\"{ELSEWHERE}\");"
        ));
        click(&mount, check_again);
        let read = wait_until(|| own_meta_reads() == 3).await;
        let done = wait_until(|| text(&mount, check_again) == "Check again").await;
        settle().await;
        let same = js(&format!(
            "return document.querySelector(\"{ELSEWHERE}\") === window.__noticeBefore;"
        ))
        .as_bool();
        restore_fetch();
        mount.remove();
        assert!(flagged && read && done);
        assert_eq!(
            same,
            Some(true),
            "a read that failed verified nothing, so the notice is not read out again"
        );
    }

    #[wasm_bindgen_test]
    async fn focus_moved_away_during_check_again_stays_where_it_was_put() {
        seed(loaded(true), false, OTHER_META);
        let mount = mount(ProfileModal);
        let check_again = "[data-testid='avatar-check-again']";
        let flagged = wait_until(|| {
            query(&mount, ELSEWHERE).is_some()
                && query(&mount, "#avatar-gravatar-input").is_some()
                && own_meta_reads() == 2
        })
        .await;
        js(&format!(
            "window.__ownMeta = {ME_META}; window.__holdMeta = true; \
             document.querySelector(\"{check_again}\").focus();"
        ));
        click(&mount, check_again);
        let held = wait_until(|| own_meta_reads() == 3).await;
        js("document.getElementById('avatar-gravatar-input').focus(); \
            window.__holdMeta = false;");
        release_held();
        let cleared = wait_until(|| query(&mount, ELSEWHERE).is_none()).await;
        settle().await;
        let focused = active_element_id();
        restore_fetch();
        mount.remove();
        assert!(flagged && held && cleared);
        assert_eq!(
            focused, "avatar-gravatar-input",
            "focus the user moved is left alone"
        );
    }

    #[wasm_bindgen_test]
    async fn an_upload_waits_for_the_meeting_lookup_until_the_server_names_the_account() {
        let _timings = TimingsGuard::set(fast_retry());
        seed(
            loaded(false),
            false,
            "{ avatar: { source: 'upload', content_type: 'image/jpeg', etag: 'e1', updated_at: 1 }, \
               gravatar_enabled: false }",
        );
        install_fetch(&format!("{LOOKUPS_HELD_AFTER_SAVE} {ROUTES}"));
        js("window.__saved = false;");
        let mount = mount(ProfileModal);
        let shown = || {
            [
                natural_width("#self-tile img"),
                natural_width("#pa-21 img"),
                natural_width(".avatar-preview img"),
            ]
        };
        let lookups = || calls_to("/meetings/m1/avatar").len();
        let before = wait_until(|| shown() == [16.0; 3] && own_meta_reads() == 2).await;
        pick_png(300, 200).await;
        let saved = wait_until(|| text(&mount, STATUS) == ANNOUNCE_UPLOADED_SMALL).await;
        let looked_up = wait_until(|| lookups() == 2).await;
        settle().await;
        settle().await;
        let widths = shown();
        release_held();
        let retried = wait_until(|| lookups() == 3).await;
        restore_fetch();
        mount.remove();
        assert!(before && saved && looked_up && retried);
        assert_eq!(
            widths, [16.0; 3],
            "a server that never named the account may not have checked whose avatar it saved"
        );
    }

    #[wasm_bindgen_test]
    async fn after_an_unconfirmed_save_the_tiles_show_the_meeting_users_image() {
        let _timings = short_save_timeout();
        seed(
            loaded(false),
            false,
            "{ avatar: null, gravatar_enabled: false, user_id: 'me@example.com' }",
        );
        install_fetch(&format!(
            "{MEETING_REPLIES} {MEETING_IMAGE_AFTER_SAVE} {ROUTES}"
        ));
        js("window.__saved = false; window.__holdUpload = true;");
        let mount = mount(ProfileModal);
        let ready = wait_until(|| {
            attr(&mount, "#self-tile .peer-avatar-slot", "data-avatar-state").as_deref()
                == Some("default")
        })
        .await;
        pick_png(300, 300).await;
        let sent = wait_until(|| puts_to("/api/v1/user/avatar").len() == 1).await;
        js(
            "window.__ownMeta = { avatar: { source: 'upload', content_type: 'image/jpeg', \
            etag: 'e2', updated_at: 2 }, gravatar_enabled: false, user_id: 'me@example.com' };",
        );
        let confirmed = wait_until(|| text(&mount, STATUS) == ANNOUNCE_UPLOADED).await;
        let tiles = || [natural_width("#self-tile img"), natural_width("#pa-21 img")];
        let from_meeting = wait_until(|| tiles() == [8.0, 8.0]).await;
        let widths = tiles();
        release_held();
        restore_fetch();
        mount.remove();
        assert!(ready && sent);
        assert!(confirmed, "the re-read shows the upload landed");
        assert!(
            from_meeting,
            "the re-read paints the meeting lookup's image: {widths:?}"
        );
    }

    #[allow(non_snake_case)]
    fn SwitchingMeetings() -> Element {
        let own = use_signal(|| OWN_SEED.with(|s| s.borrow().clone()));
        use_context_provider(|| OwnAvatarCtx(own));
        let meeting = use_signal(|| "m1");
        let settings_open = use_signal(|| true);
        MEETING.with(|m| *m.borrow_mut() = Some((meeting, settings_open)));
        use_provide_peer_avatar_cache(AvatarCacheConfig {
            meeting_id: meeting().to_string(),
            room_token: "room-token".to_string(),
            is_guest: false,
            own_user_id: Some("me@example.com".to_string()),
        });
        rsx! {
            if settings_open() {
                AvatarSettingsPanel { variant: AvatarPanelVariant::InMeeting, can_edit: true }
            }
        }
    }

    #[wasm_bindgen_test]
    async fn the_other_account_notice_stays_in_its_meeting() {
        seed(loaded(true), false, OTHER_META);
        let mount = mount(SwitchingMeetings);
        let flagged = wait_until(|| query(&mount, ELSEWHERE).is_some()).await;
        js("window.__holdMeta = true;");
        let (mut meeting, mut settings_open) = MEETING.with(|m| m.borrow().unwrap());
        settings_open.set(false);
        settle().await;
        meeting.set("m2");
        settle().await;
        settings_open.set(true);
        settle().await;
        let carried_over = query(&mount, ELSEWHERE).is_some();
        let upload = attr(&mount, "#avatar-upload-input", "aria-disabled");
        release_held();
        restore_fetch();
        mount.remove();
        assert!(flagged);
        assert!(!carried_over, "the next meeting starts unflagged");
        assert_eq!(upload.as_deref(), Some("false"));
    }

    #[wasm_bindgen_test]
    async fn a_native_close_request_leaves_the_avatar_button_working() {
        seed(
            loaded(false),
            false,
            "{ avatar: null, gravatar_enabled: false }",
        );
        let mount = mount(PreJoin);
        let button = format!("[data-testid='{PREJOIN_AVATAR_BUTTON_TESTID}']");
        let dialog = "[data-testid='avatar-dialog']";
        let ready = wait_until(|| query(&mount, &button).is_some()).await;
        click(&mount, &button);
        let opened = wait_until(dialog_open).await;
        js("document.querySelector('.avatar-dialog') \
            .dispatchEvent(new Event('cancel', { cancelable: true }));");
        let cancelled = wait_until(|| query(&mount, dialog).is_none()).await;
        click(&mount, &button);
        let reopened = wait_until(dialog_open).await;
        js("document.querySelector('.avatar-dialog').close();");
        let closed = wait_until(|| query(&mount, dialog).is_none()).await;
        click(&mount, &button);
        let reopened_again = wait_until(dialog_open).await;
        restore_fetch();
        mount.remove();
        assert!(ready);
        assert!(opened);
        assert!(cancelled, "a cancel request closes the dialog");
        assert!(reopened, "the button opens the dialog again after a cancel");
        assert!(
            closed,
            "a close the browser made on its own unmounts the dialog"
        );
        assert!(
            reopened_again,
            "the button opens the dialog again after a native close"
        );
    }

    #[wasm_bindgen_test]
    async fn try_again_moves_focus_to_the_upload_control() {
        OWN_SEED.with(|s| {
            *s.borrow_mut() = OwnAvatar {
                load: OwnAvatarLoad::Failed,
                ..OwnAvatar::default()
            }
        });
        GUEST.with(|g| g.set(false));
        install_fetch("return call.path.indexOf('/meetings/') >= 0 ? jpeg() : json(500, {});");
        let mount = mount(ProfileModal);
        let retry = "[data-testid='avatar-retry-button']";
        let failed_once = wait_until(|| {
            !calls_to("/api/v1/user/avatar").is_empty() && query(&mount, retry).is_some()
        })
        .await;
        settle().await;
        let retry_shown = wait_until(|| query(&mount, retry).is_some()).await;
        js("document.querySelector(\"[data-testid='avatar-retry-button']\").focus();");
        let focused_before = focused_testid();
        click(&mount, retry);
        let failed_again = wait_until(|| {
            calls_to("/api/v1/user/avatar").len() >= 2 && query(&mount, retry).is_some()
        })
        .await;
        settle().await;
        let focused_after = active_element_id();
        restore_fetch();
        mount.remove();
        assert!(failed_once && retry_shown);
        assert_eq!(focused_before.as_deref(), Some("avatar-retry-button"));
        assert!(failed_again, "the retry asks again and fails again");
        assert_eq!(
            focused_after, UPLOAD_INPUT_ID,
            "focus moves off the vanishing retry button"
        );
    }

    #[wasm_bindgen_test]
    async fn closing_the_dialog_after_the_camera_turned_on_keeps_focus_in_the_card() {
        seed(
            loaded(false),
            false,
            "{ avatar: null, gravatar_enabled: false }",
        );
        let mount = mount(PreJoin);
        let button = format!("[data-testid='{PREJOIN_AVATAR_BUTTON_TESTID}']");
        let ready = wait_until(|| query(&mount, &button).is_some()).await;
        click(&mount, &button);
        let opened = wait_until(dialog_open).await;
        let mut camera = CAMERA.with(|c| c.borrow().unwrap());
        camera.set(true);
        let button_gone = wait_until(|| query(&mount, &button).is_none()).await;
        press(UPLOAD_INPUT_ID, "Escape");
        let closed = wait_until(|| query(&mount, "[data-testid='avatar-dialog']").is_none()).await;
        let focused = focused_testid();
        restore_fetch();
        mount.remove();
        assert!(ready);
        assert!(opened);
        assert!(button_gone, "a live camera replaces the avatar button");
        assert!(closed);
        assert_eq!(
            focused.as_deref(),
            Some(crate::components::pre_join_settings_card::PREVIEW_CAMERA_TOGGLE_TESTID)
        );
    }

    #[wasm_bindgen_test]
    async fn an_image_under_sixteen_pixels_is_refused_before_any_upload() {
        seed(
            loaded(false),
            false,
            "{ avatar: null, gravatar_enabled: false }",
        );
        let mount = mount(ProfileModal);
        let ready = wait_until(|| query(&mount, "#avatar-upload-input").is_some()).await;
        pick_png(8, 20).await;
        let refused =
            wait_until(|| text(&mount, "[data-testid='avatar-upload-error']") == COPY_TOO_SMALL)
                .await;
        settle().await;
        let puts = puts_to("/api/v1/user/avatar").len();
        restore_fetch();
        mount.remove();
        assert!(ready);
        assert!(refused, "an 8x20 image is refused with its copy");
        assert_eq!(puts, 0, "it never reaches the server");
    }

    #[wasm_bindgen_test]
    async fn an_upload_in_progress_survives_a_panel_remount() {
        seed(
            loaded(false),
            false,
            "{ avatar: null, gravatar_enabled: false }",
        );
        js("window.__holdUpload = true;");
        let mount = mount(ProfileModal);
        let ready = wait_until(|| query(&mount, "#avatar-upload-input").is_some()).await;
        pick_png(300, 300).await;
        let uploading = wait_until(|| puts_to("/api/v1/user/avatar").len() == 1).await;
        click(&mount, "[data-testid='settings-nav-audio']");
        let left = wait_until(|| query(&mount, "#avatar-upload-input").is_none()).await;
        click(&mount, "[data-testid='settings-nav-profile']");
        let back = wait_until(|| query(&mount, "#avatar-upload-input").is_some()).await;
        let label = text(&mount, "label.avatar-btn");
        let disabled = attr(&mount, "#avatar-upload-input", "aria-disabled");
        pick_png(300, 300).await;
        settle().await;
        let puts_while_busy = puts_to("/api/v1/user/avatar").len();
        release_held();
        let done = wait_until(|| text(&mount, "label.avatar-btn") == "Replace image").await;
        restore_fetch();
        mount.remove();
        assert!(ready);
        assert!(uploading);
        assert!(left && back, "switching tabs remounts the panel");
        assert_eq!(
            label, "Uploading…",
            "the remounted panel still shows the upload"
        );
        assert_eq!(disabled.as_deref(), Some("true"));
        assert_eq!(puts_while_busy, 1, "no second upload starts");
        assert!(done, "the held upload still completes");
    }

    fn short_save_timeout() -> TimingsGuard {
        TimingsGuard::set(AvatarTimings {
            save_timeout_ms: 300,
            ..AVATAR_TIMINGS
        })
    }

    const UPLOAD_ERROR: &str = "[data-testid='avatar-upload-error']";
    const SOURCE_LABEL: &str = "[data-testid='avatar-source-label']";
    const STATUS: &str = "[data-testid='avatar-status']";

    fn own_meta_reads() -> usize {
        calls_to("/api/v1/user/avatar")
            .iter()
            .filter(|c| c["method"] == "GET")
            .count()
    }

    #[wasm_bindgen_test]
    async fn a_timed_out_upload_is_confirmed_only_when_the_server_shows_it() {
        let _timings = short_save_timeout();
        seed(
            loaded(false),
            false,
            "{ avatar: null, gravatar_enabled: false }",
        );
        js("window.__holdUpload = true;");
        let mount = mount(ProfileModal);
        let ready =
            wait_until(|| query(&mount, "#avatar-upload-input").is_some() && own_meta_reads() == 2)
                .await;
        let reads_at_start = own_meta_reads();

        pick_png(300, 300).await;
        let first_sent = wait_until(|| puts_to("/api/v1/user/avatar").len() == 1).await;
        let uncertain = wait_until(|| text(&mount, UPLOAD_ERROR) == COPY_UNCONFIRMED).await;
        let reread = wait_until(|| own_meta_reads() > reads_at_start).await;
        settle().await;
        settle().await;
        let unsaved_alert = text(&mount, UPLOAD_ERROR);
        let unsaved_role = attr(&mount, UPLOAD_ERROR, "role");
        let unsaved_status = text(&mount, STATUS);
        let unsaved_preview = attr(&mount, ".avatar-preview", "data-avatar-state");

        pick_png(300, 300).await;
        let second_sent = wait_until(|| puts_to("/api/v1/user/avatar").len() == 2).await;
        js(
            "window.__ownMeta = { avatar: { source: 'upload', content_type: 'image/jpeg', \
            etag: 'e2', updated_at: 2 }, gravatar_enabled: false };",
        );
        let confirmed = wait_until(|| {
            text(&mount, STATUS) == ANNOUNCE_UPLOADED && query(&mount, UPLOAD_ERROR).is_none()
        })
        .await;
        let preview_state = || attr(&mount, ".avatar-preview", "data-avatar-state");
        wait_until(|| preview_state().as_deref() == Some("image")).await;
        let preview = preview_state();
        let source = text(&mount, SOURCE_LABEL);
        let label = text(&mount, "label.avatar-btn");
        let disabled = attr(&mount, "#avatar-upload-input", "aria-disabled");
        restore_fetch();
        mount.remove();
        assert!(ready && first_sent && second_sent);
        assert!(
            uncertain,
            "a request that never settles is reported as unconfirmed"
        );
        assert!(
            reread,
            "the server is asked what it holds after the timeout"
        );
        assert_eq!(
            unsaved_alert, COPY_UNCONFIRMED,
            "the alert stays when the server does not show the upload"
        );
        assert_eq!(unsaved_role.as_deref(), Some("alert"));
        assert_ne!(unsaved_status, ANNOUNCE_UPLOADED);
        assert_eq!(unsaved_preview.as_deref(), Some("default"));
        assert!(
            confirmed,
            "an upload the server shows clears the alert and is announced as saved"
        );
        assert_eq!(preview.as_deref(), Some("image"));
        assert_eq!(source, "Using an uploaded image.");
        assert_eq!(label, "Replace image");
        assert_eq!(
            disabled.as_deref(),
            Some("false"),
            "the controls work again without a reload"
        );
    }

    #[wasm_bindgen_test]
    async fn a_timed_out_remove_is_confirmed_only_when_the_server_shows_it() {
        let _timings = short_save_timeout();
        let url = real_jpeg_url().await;
        seed(
            OwnAvatar {
                info: Some(upload_info("e1")),
                image_url: Some(url),
                ..loaded(false)
            },
            false,
            "{ avatar: { source: 'upload', content_type: 'image/jpeg', etag: 'e1', updated_at: 1 }, \
               gravatar_enabled: false }",
        );
        js("window.__holdDelete = true;");
        let mount = mount(ProfileModal);
        let remove = "[data-testid='avatar-remove-button']";
        let ready = wait_until(|| {
            query(&mount, remove).is_some()
                && attr(&mount, "#pa-21 .peer-avatar-slot", "data-avatar-state").as_deref()
                    == Some("image")
                && own_meta_reads() == 2
        })
        .await;
        let deletes = || {
            calls()
                .iter()
                .filter(|c| c["method"] == "DELETE" && c["path"] == "/api/v1/user/avatar")
                .count()
        };
        let reads_at_start = own_meta_reads();

        click(&mount, remove);
        let first_sent = wait_until(|| deletes() == 1).await;
        let uncertain = wait_until(|| text(&mount, UPLOAD_ERROR) == COPY_UNCONFIRMED).await;
        let reread = wait_until(|| own_meta_reads() > reads_at_start).await;
        settle().await;
        settle().await;
        let kept_alert = text(&mount, UPLOAD_ERROR);
        let kept_status = text(&mount, STATUS);
        let kept_button = query(&mount, remove).is_some();
        let kept_preview = attr(&mount, ".avatar-preview", "data-avatar-state");

        click(&mount, remove);
        let second_sent = wait_until(|| deletes() == 2).await;
        js("window.__ownMeta = { avatar: null, gravatar_enabled: false };");
        let confirmed = wait_until(|| {
            text(&mount, STATUS) == ANNOUNCE_REMOVED && query(&mount, UPLOAD_ERROR).is_none()
        })
        .await;
        settle().await;
        let preview = settled_state(&mount, ".avatar-preview", "default").await;
        let button_gone = query(&mount, remove).is_none();
        let source = text(&mount, SOURCE_LABEL);
        let focused = active_element_id();
        let tile_state = || attr(&mount, "#pa-21 .peer-avatar-slot", "data-avatar-state");
        wait_until(|| tile_state().as_deref() == Some("default")).await;
        let tile = tile_state();
        restore_fetch();
        mount.remove();
        assert!(ready && first_sent && second_sent);
        assert!(
            uncertain,
            "a request that never settles is reported as unconfirmed"
        );
        assert!(
            reread,
            "the server is asked what it holds after the timeout"
        );
        assert_eq!(
            kept_alert, COPY_UNCONFIRMED,
            "the alert stays while the server still has the avatar"
        );
        assert_ne!(kept_status, ANNOUNCE_REMOVED);
        assert!(kept_button, "Remove stays offered");
        assert_eq!(kept_preview.as_deref(), Some("image"));
        assert!(
            confirmed,
            "a removal the server shows clears the alert and is announced"
        );
        assert_eq!(preview.as_deref(), Some("default"));
        assert!(button_gone);
        assert_eq!(source, "No avatar set. Others see the default icon.");
        assert_eq!(
            tile.as_deref(),
            Some("default"),
            "this user's tile in the meeting follows the server too"
        );
        assert_eq!(
            focused, UPLOAD_INPUT_ID,
            "focus moves off the vanished Remove button"
        );
    }

    #[wasm_bindgen_test]
    async fn a_timed_out_gravatar_save_the_server_shows_is_announced_as_saved() {
        let _timings = short_save_timeout();
        seed(
            loaded(true),
            false,
            "{ avatar: null, gravatar_enabled: true }",
        );
        js("window.__holdGravatar = true;");
        let mount = mount(ProfileModal);
        let ready = wait_until(|| query(&mount, "#avatar-gravatar-input").is_some()).await;
        js(
            "var el = document.getElementById('avatar-gravatar-input'); \
            el.value = 'Me@Example.com'; el.dispatchEvent(new Event('input', { bubbles: true }));",
        );
        let save = "[data-testid='avatar-gravatar-save']";
        let enabled =
            wait_until(|| attr(&mount, save, "aria-disabled").as_deref() == Some("false")).await;
        click(&mount, save);
        let sent = wait_until(|| puts_to("/api/v1/user/avatar/gravatar").len() == 1).await;
        js(
            "window.__ownMeta = { avatar: { source: 'gravatar', content_type: 'image/jpeg', \
            etag: 'g1', updated_at: 3 }, gravatar_enabled: true };",
        );
        let confirmed = wait_until(|| {
            text(&mount, STATUS) == ANNOUNCE_GRAVATAR
                && query(&mount, "[data-testid='avatar-gravatar-error']").is_none()
        })
        .await;
        let preview_state = || attr(&mount, ".avatar-preview", "data-avatar-state");
        wait_until(|| preview_state().as_deref() == Some("image")).await;
        let preview = preview_state();
        let source = text(&mount, SOURCE_LABEL);
        restore_fetch();
        mount.remove();
        assert!(ready && enabled && sent);
        assert!(
            confirmed,
            "a Gravatar save the server shows clears the alert and is announced"
        );
        assert_eq!(preview.as_deref(), Some("image"));
        assert_eq!(source, "Using your Gravatar image.");
    }

    const SAVED_UPLOAD_META: &str = "window.__ownMeta = { avatar: { source: 'upload', \
        content_type: 'image/jpeg', etag: 'e2', updated_at: 2 }, gravatar_enabled: false };";

    #[wasm_bindgen_test]
    async fn a_timed_out_save_is_re_read_after_a_refresh_that_was_already_running() {
        let _timings = short_save_timeout();
        seed(
            loaded(false),
            false,
            "{ avatar: null, gravatar_enabled: false }",
        );
        js("window.__holdUpload = true; window.__holdMeta = true;");
        let mount = mount(ProfileModal);
        let ready = wait_until(|| {
            query(&mount, "#avatar-upload-input").is_some()
                && attr(&mount, "#pa-21 .peer-avatar-slot", "data-avatar-state").as_deref()
                    == Some("default")
        })
        .await;
        let refresh_held = wait_until(|| own_meta_reads() == 1).await;
        let tile_before = attr(&mount, "#pa-21 img.peer-avatar", "src");
        pick_png(300, 300).await;
        let sent = wait_until(|| puts_to("/api/v1/user/avatar").len() == 1).await;
        js(&format!("window.__holdMeta = false; {SAVED_UPLOAD_META}"));
        let uncertain = wait_until(|| text(&mount, UPLOAD_ERROR) == COPY_UNCONFIRMED).await;
        TimeoutFuture::new(300).await;
        let reads_while_waiting = own_meta_reads();
        release_held();
        let reread = wait_until(|| own_meta_reads() >= 2).await;
        let confirmed = wait_until(|| {
            text(&mount, STATUS) == ANNOUNCE_UPLOADED && query(&mount, UPLOAD_ERROR).is_none()
        })
        .await;
        let preview = settled_state(&mount, ".avatar-preview", "image").await;
        wait_until(|| query(&mount, "#pa-21 img.peer-avatar").is_some()).await;
        let tile_after = attr(&mount, "#pa-21 img.peer-avatar", "src");
        restore_fetch();
        mount.remove();
        assert!(ready && refresh_held && sent && uncertain);
        assert_eq!(
            reads_while_waiting, 1,
            "the re-read waits for the refresh that is already running"
        );
        assert!(
            reread,
            "the server is read again once that refresh, which predates the save, is done"
        );
        assert!(confirmed, "the re-read decides that the upload landed");
        assert_eq!(preview.as_deref(), Some("image"));
        assert!(tile_after.is_some());
        assert_ne!(
            tile_after, tile_before,
            "this user's meeting tile shows the stored image"
        );
    }

    #[wasm_bindgen_test]
    async fn a_timed_out_save_is_re_read_even_when_a_running_refresh_never_finishes() {
        let _timings = TimingsGuard::set(AvatarTimings {
            save_timeout_ms: 300,
            own_refresh_wait_ms: 400,
            ..AVATAR_TIMINGS
        });
        seed(
            loaded(false),
            false,
            "{ avatar: null, gravatar_enabled: false }",
        );
        js("window.__holdUpload = true; window.__holdMeta = true;");
        let mount = mount(ProfileModal);
        let ready = wait_until(|| query(&mount, "#avatar-upload-input").is_some()).await;
        let refresh_hung = wait_until(|| own_meta_reads() == 1).await;
        pick_png(300, 300).await;
        let sent = wait_until(|| puts_to("/api/v1/user/avatar").len() == 1).await;
        let profile_read_hung = wait_until(|| own_meta_reads() == 2).await;
        js(&format!("window.__holdMeta = false; {SAVED_UPLOAD_META}"));
        let uncertain = wait_until(|| text(&mount, UPLOAD_ERROR) == COPY_UNCONFIRMED).await;
        let reread = wait_until(|| own_meta_reads() == 3).await;
        let confirmed = wait_until(|| {
            text(&mount, STATUS) == ANNOUNCE_UPLOADED && query(&mount, UPLOAD_ERROR).is_none()
        })
        .await;
        let preview = settled_state(&mount, ".avatar-preview", "image").await;
        let disabled = attr(&mount, "#avatar-upload-input", "aria-disabled");
        restore_fetch();
        mount.remove();
        assert!(ready && refresh_hung && sent && profile_read_hung && uncertain);
        assert!(
            reread,
            "after waiting out the stuck refresh, the server is read anyway"
        );
        assert!(confirmed, "that read decides that the upload landed");
        assert_eq!(preview.as_deref(), Some("image"));
        assert_eq!(disabled.as_deref(), Some("false"));
    }

    #[wasm_bindgen_test]
    async fn a_newer_error_stays_when_an_older_timed_out_save_turns_out_to_have_landed() {
        let _timings = short_save_timeout();
        seed(
            loaded(false),
            false,
            "{ avatar: null, gravatar_enabled: false }",
        );
        js("window.__holdUpload = true;");
        let mount = mount(ProfileModal);
        let ready = wait_until(|| query(&mount, "#avatar-upload-input").is_some()).await;
        let mount_refreshed = wait_until(|| own_meta_reads() == 2).await;
        settle().await;
        pick_png(300, 300).await;
        let sent = wait_until(|| puts_to("/api/v1/user/avatar").len() == 1).await;
        js(&format!("window.__holdMeta = true; {SAVED_UPLOAD_META}"));
        let uncertain = wait_until(|| text(&mount, UPLOAD_ERROR) == COPY_UNCONFIRMED).await;
        let reread_held = wait_until(|| own_meta_reads() == 3).await;
        pick_files(
            "[new File([new Uint8Array(2 * 1024 * 1024 + 1)], 'big.png', { type: 'image/png' })]",
        );
        let newer = wait_until(|| text(&mount, UPLOAD_ERROR) == COPY_TOO_LARGE).await;
        release_held();
        let landed = wait_until(|| {
            attr(&mount, ".avatar-preview", "data-avatar-state").as_deref() == Some("image")
        })
        .await;
        settle().await;
        settle().await;
        let alert = text(&mount, UPLOAD_ERROR);
        let status = text(&mount, STATUS);
        restore_fetch();
        mount.remove();
        assert!(ready && mount_refreshed && sent && uncertain && reread_held && newer);
        assert!(landed, "the re-read still shows what the server saved");
        assert_eq!(
            alert, COPY_TOO_LARGE,
            "the newer alert is not cleared by the older save"
        );
        assert_ne!(
            status, ANNOUNCE_UPLOADED,
            "the older save is not announced over it"
        );
    }

    #[wasm_bindgen_test]
    async fn a_transparent_image_saved_as_jpeg_is_flattened_like_the_server_does() {
        seed(
            loaded(false),
            false,
            "{ avatar: null, gravatar_enabled: false }",
        );
        let mount = mount(ProfileModal);
        let ready = wait_until(|| query(&mount, "#avatar-upload-input").is_some()).await;
        js(
            "window.__origToBlob = window.__origToBlob || HTMLCanvasElement.prototype.toBlob; \
            HTMLCanvasElement.prototype.toBlob = function(done, type, quality) { \
              return window.__origToBlob.call(this, done, \
                type === 'image/webp' ? 'image/png' : type, quality); };",
        );
        let picked: js_sys::Promise = js(&format!(
            "return new Promise(function(resolve) {{ \
               var c = document.createElement('canvas'); c.width = 64; c.height = 64; \
               c.toBlob(function(b) {{ \
                 var input = document.getElementById('{UPLOAD_INPUT_ID}'); \
                 var dt = new DataTransfer(); \
                 dt.items.add(new File([b], 'clear.png', {{ type: 'image/png' }})); \
                 input.files = dt.files; \
                 input.dispatchEvent(new Event('change', {{ bubbles: true }})); \
                 resolve(true); }}, 'image/png'); }});"
        ))
        .unchecked_into();
        JsFuture::from(picked).await.unwrap();
        let saved = wait_until(|| text(&mount, STATUS) == ANNOUNCE_UPLOADED_SMALL).await;
        js("HTMLCanvasElement.prototype.toBlob = window.__origToBlob;");
        let put_type = puts_to("/api/v1/user/avatar")
            .first()
            .and_then(|c| c["type"].as_str().map(str::to_string));
        let sampled: js_sys::Promise = js(
            "return createImageBitmap(new Blob([window.__lastPutBody], { type: 'image/jpeg' })) \
               .then(function(bitmap) { \
                 var c = document.createElement('canvas'); c.width = 1; c.height = 1; \
                 var g = c.getContext('2d'); g.drawImage(bitmap, 0, 0, 1, 1, 0, 0, 1, 1); \
                 return Array.from(g.getImageData(0, 0, 1, 1).data.slice(0, 3)).join(','); });",
        )
        .unchecked_into();
        let corner = JsFuture::from(sampled)
            .await
            .unwrap()
            .as_string()
            .unwrap_or_default();
        restore_fetch();
        mount.remove();
        let rgb: Vec<u8> = corner.split(',').filter_map(|c| c.parse().ok()).collect();
        assert!(ready && saved);
        assert_eq!(put_type.as_deref(), Some("image/jpeg"), "the JPEG path ran");
        assert_eq!(rgb.len(), 3, "{corner}");
        assert!(
            rgb.iter().all(|&c| (50..=66).contains(&c)),
            "a transparent corner gets the server's flatten colour, not black: {rgb:?}"
        );
    }

    fn announce_own_change() {
        let cache = MEETING_CACHE.with(|c| c.borrow().clone().unwrap());
        crate::components::avatar::avatar_change_listener(Some(&cache))
            .unwrap()
            .emit("me@example.com".to_string());
    }

    const GRAVATAR_META: &str = "{ avatar: { source: 'gravatar', \
        gravatar_url: 'https://gravatar.com/avatar/' + 'cd'.repeat(32), content_type: 'image/jpeg', \
        etag: 'g7', updated_at: 7 }, gravatar_enabled: true, user_id: 'me@example.com' }";

    fn gravatar_info(etag: &str) -> AvatarInfo {
        AvatarInfo {
            source: AvatarSource::Gravatar,
            gravatar_url: Some(format!("https://gravatar.com/avatar/{}", "cd".repeat(32))),
            ..upload_info(etag)
        }
    }

    #[wasm_bindgen_test]
    async fn a_change_saved_in_another_tab_updates_this_tabs_tiles_and_profile() {
        seed(
            OwnAvatar {
                info: Some(upload_info("e1")),
                ..loaded(true)
            },
            false,
            ME_META,
        );
        let mount = mount(ProfileModal);
        let tile = || attr(&mount, "#self-tile img.peer-avatar", "src");
        let ready = wait_until(|| {
            tile().is_some()
                && own_meta_reads() == 2
                && text(&mount, SOURCE_LABEL) == "Using an uploaded image."
        })
        .await;
        let before = tile();
        js(&format!("window.__ownMeta = {GRAVATAR_META};"));
        settle().await;
        let unannounced = (tile(), text(&mount, SOURCE_LABEL));
        announce_own_change();
        let relabelled =
            wait_until(|| text(&mount, SOURCE_LABEL) == "Using your Gravatar image.").await;
        let replaced = wait_until(|| tile().is_some_and(|src| Some(src) != before)).await;
        let tiles = (tile(), attr(&mount, "#pa-21 img.peer-avatar", "src"));
        let page = mount.text_content().unwrap_or_default();
        let reads = own_meta_reads();
        restore_fetch();
        mount.remove();
        assert!(ready);
        assert_eq!(
            unannounced,
            (before.clone(), "Using an uploaded image.".to_string())
        );
        assert!(relabelled, "the open Profile panel names the new source");
        assert!(replaced, "the self tile shows the new image");
        assert_eq!(tiles.0, tiles.1, "and so does this user's other tile");
        assert_eq!(reads, 3, "one read of the account per announcement");
        assert!(!page.contains("rejoin"), "{page}");
    }

    #[wasm_bindgen_test]
    async fn an_announcement_read_before_a_save_cannot_undo_it() {
        seed(
            OwnAvatar {
                info: Some(gravatar_info("g7")),
                ..loaded(true)
            },
            false,
            GRAVATAR_META,
        );
        let mount = mount(ProfileModal);
        let ready = wait_until(|| {
            own_meta_reads() == 2
                && query(&mount, "#avatar-upload-input").is_some()
                && text(&mount, SOURCE_LABEL) == "Using your Gravatar image."
        })
        .await;
        js("window.__holdMeta = true;");
        announce_own_change();
        let read_held = wait_until(|| own_meta_reads() == 3).await;
        pick_png(300, 200).await;
        let saved = wait_until(|| {
            text(&mount, STATUS) == ANNOUNCE_UPLOADED_SMALL
                && text(&mount, SOURCE_LABEL) == "Using an uploaded image."
        })
        .await;
        js("window.__holdMeta = false;");
        let released = release_held();
        let answered =
            wait_until(|| js("return window.__avatarPending;").as_f64() == Some(0.0)).await;
        settle().await;
        settle().await;
        let source = text(&mount, SOURCE_LABEL);
        restore_fetch();
        mount.remove();
        assert!(ready && read_held && saved && answered);
        assert_eq!(released, 1);
        assert_eq!(
            source, "Using an uploaded image.",
            "the read sent before the save must not bring the Gravatar back"
        );
    }

    #[wasm_bindgen_test]
    async fn a_removal_in_another_tab_moves_focus_off_the_vanishing_remove_button() {
        seed(
            OwnAvatar {
                info: Some(upload_info("e1")),
                ..loaded(true)
            },
            false,
            ME_META,
        );
        let mount = mount(ProfileModal);
        let remove = "[data-testid='avatar-remove-button']";
        let ready = wait_until(|| own_meta_reads() == 2 && query(&mount, remove).is_some()).await;
        js(&format!(
            "document.getElementById('{REMOVE_BUTTON_ID}').focus();"
        ));
        let focused_before = active_element_id();
        js("window.__ownMeta = Object.assign({}, window.__ownMeta, { avatar: null });");
        announce_own_change();
        let gone = wait_until(|| query(&mount, remove).is_none()).await;
        settle().await;
        let focused = active_element_id();
        restore_fetch();
        mount.remove();
        assert!(ready && gone);
        assert_eq!(focused_before, REMOVE_BUTTON_ID);
        assert_eq!(
            focused, UPLOAD_INPUT_ID,
            "focus stays in the panel when another tab removes the avatar"
        );
    }

    #[wasm_bindgen_test]
    async fn a_removal_in_another_tab_leaves_focus_moved_off_the_remove_button_alone() {
        seed(
            OwnAvatar {
                info: Some(upload_info("e1")),
                ..loaded(true)
            },
            false,
            ME_META,
        );
        let mount = mount(ProfileModal);
        let remove = "[data-testid='avatar-remove-button']";
        let field = "avatar-gravatar-input";
        let ready = wait_until(|| {
            own_meta_reads() == 2
                && query(&mount, remove).is_some()
                && query(&mount, &format!("#{field}")).is_some()
        })
        .await;
        js(&format!(
            "document.getElementById('{REMOVE_BUTTON_ID}').focus();"
        ));
        announce_own_change();
        let reread = wait_until(|| own_meta_reads() == 3).await;
        settle().await;
        let focused_through_reread = active_element_id();
        js(&format!("document.getElementById('{field}').focus();"));
        js("window.__ownMeta = Object.assign({}, window.__ownMeta, { avatar: null });");
        announce_own_change();
        let gone = wait_until(|| query(&mount, remove).is_none()).await;
        settle().await;
        let focused = active_element_id();
        restore_fetch();
        mount.remove();
        assert!(ready && reread && gone);
        assert_eq!(focused_through_reread, REMOVE_BUTTON_ID);
        assert_eq!(
            focused, field,
            "focus already moved off Remove is left where the user put it"
        );
    }
}
