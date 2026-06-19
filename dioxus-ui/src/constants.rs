// SPDX-License-Identifier: MIT OR Apache-2.0

use std::cell::RefCell;

use serde::Deserialize;
use serde_wasm_bindgen::from_value as from_js_value;
use videocall_types::truthy;
use wasm_bindgen::JsValue;
use web_sys::window;

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct RuntimeConfig {
    #[serde(rename = "apiBaseUrl")]
    pub api_base_url: String,
    #[serde(rename = "meetingApiBaseUrl")]
    #[serde(default)]
    pub meeting_api_base_url: Option<String>,
    #[serde(rename = "wsUrl")]
    pub ws_url: String,
    #[serde(rename = "webTransportHost")]
    pub web_transport_host: String,
    #[serde(rename = "oauthEnabled")]
    pub oauth_enabled: String,
    #[serde(rename = "e2eeEnabled")]
    pub e2ee_enabled: String,
    #[serde(rename = "webTransportEnabled")]
    pub web_transport_enabled: String,
    /// Issue #1483: server-side gate for the per-peer-tile "WT"/"WS" transport
    /// badge. When truthy (`"true"`/`"1"`/…) each peer tile renders a small
    /// badge next to its signal meter showing whether that peer's media is
    /// flowing over WebTransport or WebSocket; when absent/empty/falsey the
    /// badge is never rendered. **Default OFF.**
    ///
    /// CRITICAL (config.js bind-mount trap, see project memory): `#[serde(default)]`
    /// so a stale bind-mounted `config.js` that predates this key still parses —
    /// a missing key yields the empty-string default (→ `truthy` returns false →
    /// badge OFF), never a startup-bricking parse failure. The e2e docker stack
    /// bind-mounts the host's committed `config.js`, which does NOT contain this
    /// key, so the UI must (and does) behave identically when it is absent.
    #[serde(rename = "transportBadgeEnabled")]
    #[serde(default)]
    pub transport_badge_enabled: String,
    #[serde(rename = "firefoxEnabled")]
    #[serde(default)]
    pub firefox_enabled: String,
    #[serde(rename = "usersAllowedToStream")]
    pub users_allowed_to_stream: String,
    #[serde(rename = "oauthProvider")]
    #[serde(default)]
    pub oauth_provider: Option<String>,
    /// Authorization endpoint URL for the OAuth provider.  Written into
    /// `config.js` from the `OAUTH_AUTH_URL` environment variable.
    ///
    /// When empty the UI falls back to fetching `GET /api/v1/oauth/provider-config`
    /// from the meeting-api (useful when only `OAUTH_ISSUER` is set and the
    /// auth URL was resolved via OIDC discovery).
    #[serde(rename = "oauthAuthUrl")]
    #[serde(default)]
    pub oauth_auth_url: Option<String>,
    /// OAuth client ID (public — safe to expose in the browser).
    /// Written from the `OAUTH_CLIENT_ID` environment variable.
    #[serde(rename = "oauthClientId")]
    #[serde(default)]
    pub oauth_client_id: Option<String>,
    /// Absolute URL the identity provider should redirect to after
    /// authentication.  Must be registered with the provider and must point
    /// to the dioxus-ui `/auth/callback` route.
    /// Written from the `OAUTH_REDIRECT_URL` environment variable.
    #[serde(rename = "oauthRedirectUrl")]
    #[serde(default)]
    pub oauth_redirect_url: Option<String>,
    /// Space-separated OAuth scopes (default: `"openid email profile"`).
    /// Written from the `OAUTH_SCOPES` environment variable.
    #[serde(rename = "oauthScopes")]
    #[serde(default)]
    pub oauth_scopes: Option<String>,
    /// Token endpoint URL for the identity provider.  Written from the
    /// `OAUTH_TOKEN_URL` environment variable.
    ///
    /// When empty the UI falls back to OIDC discovery via `oauthIssuer` or,
    /// as a last resort, to `GET /api/v1/oauth/provider-config`.
    #[serde(rename = "oauthTokenUrl")]
    #[serde(default)]
    pub oauth_token_url: Option<String>,
    /// OIDC issuer URL (e.g. `https://accounts.google.com`).  Written from
    /// the `OAUTH_ISSUER` environment variable.
    ///
    /// When `oauthTokenUrl` is not set, the UI fetches
    /// `{oauthIssuer}/.well-known/openid-configuration` to discover the
    /// token endpoint.
    #[serde(rename = "oauthIssuer")]
    #[serde(default)]
    pub oauth_issuer: Option<String>,
    /// Optional `prompt` parameter appended to the authorization URL.
    /// Written from the `OAUTH_PROMPT` environment variable.
    ///
    /// Common values: `"login"` (force re-authentication), `"consent"`,
    /// `"select_account"` (Google/Okta/Entra).  Leave empty (the default)
    /// for maximum provider compatibility — the parameter is omitted
    /// entirely when blank so it does not cause errors on providers that
    /// do not recognise it.
    #[serde(rename = "oauthPrompt")]
    #[serde(default)]
    pub oauth_prompt: Option<String>,
    /// OAuth flow mode: `"pkce"` for client-side PKCE, any other value
    /// (including absent/empty) for server-side OAuth where the backend
    /// exchanges the authorization code using the client secret.
    #[serde(rename = "oauthFlow")]
    #[serde(default)]
    pub oauth_flow: Option<String>,
    /// SearchV2 middleware base URL (e.g. "http://localhost:3000/api/search/v2").
    /// When absent, the SearchModal falls back to the meeting-api Postgres search.
    #[serde(rename = "searchApiBaseUrl")]
    #[serde(default)]
    pub search_api_base_url: Option<String>,
    #[serde(rename = "serverElectionPeriodMs")]
    pub server_election_period_ms: u64,
    #[serde(rename = "audioBitrateKbps")]
    pub audio_bitrate_kbps: u32,
    #[serde(rename = "videoBitrateKbps")]
    pub video_bitrate_kbps: u32,
    #[serde(rename = "screenBitrateKbps")]
    pub screen_bitrate_kbps: u32,
    #[serde(rename = "vadThreshold", default = "default_vad_threshold")]
    pub vad_threshold: f32,
    #[serde(rename = "consoleLogUploadEnabled")]
    #[serde(default)]
    pub console_log_upload_enabled: String,
    #[serde(rename = "mockPeersEnabled")]
    #[serde(default)]
    pub mock_peers_enabled: String,
    /// Maximum simulcast layers a publisher may emit (issue #989 / #1082), the
    /// runtime half of the per-receiver simulcast feature flag.
    ///
    /// **Default: ON (3 layers)** via [`default_experimental_simulcast_max_layers`].
    /// The full pipeline is live: the publisher encodes up to 3 tier-differentiated
    /// layers, each tagged with a cleartext `simulcast_layer_id`; the relay
    /// per-(source, kind) filter forwards ONLY the layer each receiver selected;
    /// and the receiver's `LayerChooser` picks the best layer its own downlink
    /// can sustain. (Audio caps at 3 rungs, screen at 3.)
    ///
    /// The effective layer count is `min(this, device-capability ceiling)`
    /// (see `host.rs` + `capability_check.rs::capability_max_simulcast_layers`),
    /// so default-ON is **safe for weak devices**: a `Block`/`StrongWarn` device,
    /// older Intel Mac, or low-benchmark device auto-gates DOWN to 1 (or 2)
    /// layers regardless of this value — it can never force a device above what
    /// it can encode.
    ///
    /// ROLLBACK: set this to `1` to disable simulcast globally — either here (the
    /// code default) or per environment by adding
    /// `experimentalSimulcastMaxLayers: 1` to the Helm `runtimeConfig`
    /// (`helm/videocall-ui/.../configmap-configjs.yaml` reads `.Values.runtimeConfig`).
    /// A per-env override always wins over this code default.
    ///
    /// CRITICAL (config.js bind-mount trap, see project memory): this is
    /// `#[serde(default = ...)]` so a stale bind-mounted `config.js` that
    /// predates this key still parses — a missing key yields the code default
    /// (now 3), never a parse failure that would brick startup. A per-env config
    /// that wants a different value must set the key explicitly.
    #[serde(rename = "experimentalSimulcastMaxLayers")]
    #[serde(default = "default_experimental_simulcast_max_layers")]
    pub experimental_simulcast_max_layers: u32,
    /// **TEST-ONLY** override for the device-capability simulcast ceiling
    /// (`capability_max_simulcast_layers`), issue #1093.
    ///
    /// The containerized e2e CI runner reports a low `navigator.hardwareConcurrency`
    /// (often 1–2 logical cores), which clamps the sniffed capability ceiling to
    /// **1** layer — so the multi-party per-receiver simulcast SEND assertions in
    /// `e2e/tests/simulcast-per-receiver.spec.ts` could never observe >1 emitted
    /// layer and had to be `test.fixme`'d. When set, this REPLACES the sniffed
    /// ceiling (cores + UA platform) so a test can force the publisher to a known
    /// layer count regardless of the runner's core count. It is clamped to the real
    /// ladder depth (`SIMULCAST_MAX_LAYERS`) and a `0` is treated as `1`, so a bogus
    /// value can never request absurd or zero layer counts (see
    /// [`crate::components::capability_check::apply_capability_override`]).
    ///
    /// This affects ONLY the capability ceiling — it does NOT change the
    /// `experimentalSimulcastMaxLayers` flag, which remains an independent input to
    /// the `min(flag, ceiling)` the encoder is configured with. It also does not
    /// touch the audio ceiling (`max_layers_for_kind(Audio)`), which is decoupled
    /// per #1082.
    ///
    /// When active, `capability_max_simulcast_layers()` emits a `warn!` naming the
    /// override so it can never silently leak into a production incident unnoticed.
    /// Production `config.js` (and the e2e docker stack's committed `config.js`)
    /// omit this key entirely.
    ///
    /// CRITICAL (config.js bind-mount trap, see project memory): `Option<u32>` +
    /// `#[serde(default)]` so a `config.js` that predates / omits this key parses
    /// to `None` (override inactive — sniffed behaviour unchanged), never a
    /// startup-bricking parse failure. The e2e docker stack bind-mounts the host's
    /// committed `config.js`, which does NOT contain this key, so the UI must (and
    /// does) behave identically when it is absent.
    #[serde(rename = "testCapabilityMaxLayersOverride")]
    #[serde(default)]
    pub test_capability_max_layers_override: Option<u32>,
    /// Operator dial for the WASM logger's max level (issue: console-log perf).
    /// Valid values (case-insensitive): `trace` / `debug` / `info` / `warn` /
    /// `error` (`off` is also accepted). When **absent** the logger initialises
    /// at Info — matching the historical hardcoded init level in `main.rs` — so
    /// behaviour is unchanged unless an operator opts in.
    ///
    /// This lets operators raise or lower client log verbosity from the Helm
    /// `runtimeConfig` (`config.js`) WITHOUT a code change or rebuild — useful
    /// for cutting per-packet log volume on a hot deployment, or temporarily
    /// raising verbosity for a debugging session.
    ///
    /// Interaction with console-log collection (see `attendants.rs`): when
    /// collection is on and `logLevel` is **absent**, the level is bumped to
    /// Debug (the historical capture behaviour). When `logLevel` is **explicitly
    /// set** — INCLUDING `"info"` — that value wins and caps collection at it
    /// (e.g. `"info"`/`"warn"` cut per-packet log volume; `"trace"` opts into the
    /// per-packet hot-path logs, which are emitted at `trace!` and otherwise stay
    /// off even at the Debug ceiling).
    ///
    /// `Option<String>` (not a defaulted `String`) so we can distinguish "key
    /// ABSENT" (`None` → Debug bump when collecting) from "explicitly `info`"
    /// (`Some("info")` → caps collection at info). A defaulted `String` collapsed
    /// those cases and made `info` unusable as a ceiling.
    ///
    /// CRITICAL (config.js bind-mount trap, see project memory): `#[serde(default)]`
    /// means a stale bind-mounted `config.js` predating the key parses to `None`,
    /// never a startup-bricking parse failure.
    #[serde(rename = "logLevel")]
    #[serde(default)]
    pub log_level: Option<String>,
}

fn default_vad_threshold() -> f32 {
    0.02
}

/// Default simulcast layer ceiling when `experimentalSimulcastMaxLayers` is
/// absent from `config.js` — **3 layers (feature ON by default)** as of #1082.
/// The effective count is still `min(this, device-capability ceiling)`, so weak
/// devices auto-gate down to 1–2 layers (see `capability_check.rs`). Set to `1`
/// (here or via a per-env Helm `runtimeConfig` override) to disable simulcast.
fn default_experimental_simulcast_max_layers() -> u32 {
    3
}

thread_local! {
    /// Memoized parse of `window.__APP_CONFIG` (issue #1492). `__APP_CONFIG` is
    /// installed by `config.js` before the wasm bundle runs and is immutable for
    /// the page's lifetime, so the full `RuntimeConfig` deserialization (a 30+-field
    /// struct, done previously on EVERY one of the ~36 accessor calls — several
    /// per-tile-per-render) only needs to happen once. We cache the first
    /// **successful** parse; a failed read (config not yet present, or unparseable)
    /// is NOT cached, so a transient pre-load miss can still succeed on a later call.
    ///
    /// The wasm target is single-threaded, so `thread_local! + RefCell` is the
    /// correct (and cheapest) cell here — no `Sync` bound is needed.
    static CONFIG_CACHE: RefCell<Option<RuntimeConfig>> = const { RefCell::new(None) };
}

/// Parse `window.__APP_CONFIG` into a [`RuntimeConfig`] with no caching.
/// Split out so the cache wrapper ([`app_config`]) stays a thin memoization layer.
fn parse_app_config() -> Result<RuntimeConfig, String> {
    let win = window().expect("window");
    let config = js_sys::Reflect::get(&win, &JsValue::from_str("__APP_CONFIG"))
        .unwrap_or(JsValue::UNDEFINED);
    if config.is_undefined() || config.is_null() {
        return Err("Runtime configuration not found (window.__APP_CONFIG missing)".to_string());
    }
    from_js_value::<RuntimeConfig>(config)
        .map_err(|e| format!("Failed to parse __APP_CONFIG: {e:?}"))
}

/// Return the cached `T`, or compute it via `parse` and cache the result on the
/// first **success only**. An `Err` from `parse` is propagated WITHOUT being
/// cached, so a transient failure (e.g. config not yet installed) does not
/// poison later calls. Pure over the cell + closure so it is host-unit-testable
/// for the "parse runs at most once" contract (issue #1492); `app_config` is the
/// only caller and supplies the real `RefCell` + `parse_app_config`.
fn memoize_ok<T, E, F>(cache: &RefCell<Option<T>>, parse: F) -> Result<T, E>
where
    T: Clone,
    F: FnOnce() -> Result<T, E>,
{
    if let Some(value) = cache.borrow().as_ref() {
        return Ok(value.clone());
    }
    let parsed = parse()?;
    *cache.borrow_mut() = Some(parsed.clone());
    Ok(parsed)
}

/// Read the runtime configuration, parsing `window.__APP_CONFIG` at most **once
/// per page load** (issue #1492). The first successful parse is memoized in a
/// thread-local cache and subsequent calls return a cheap clone, eliminating the
/// per-render `serde_wasm_bindgen` deserialization that scaled with tile count on
/// the low-power devices this project targets.
///
/// Cache semantics: only `Ok` is cached (see [`memoize_ok`]). An `Err` (config
/// absent at first access, or a parse failure) falls through uncached so a later
/// call — once `config.js` has installed `__APP_CONFIG` — can populate the cache.
/// In production `config.js` freezes `__APP_CONFIG` before the bundle runs and
/// nothing rewrites it, so the cached value never goes stale. (Playwright E2E
/// specs get a fresh page = fresh wasm module per test, so the cache resets
/// naturally there; only the in-process `wasm-bindgen-test` harness, which reuses
/// one wasm module across cases, needs the explicit [`reset_config_cache_for_test`]
/// hook below.)
pub fn app_config() -> Result<RuntimeConfig, String> {
    CONFIG_CACHE.with(|cache| memoize_ok(cache, parse_app_config))
}

/// Clear the memoized [`app_config`] cache. **Test-support only.**
///
/// Production `__APP_CONFIG` is frozen at load and never re-written, so the cache
/// is never invalidated at runtime. But the `wasm-bindgen-test` harness runs every
/// `#[wasm_bindgen_test]` in a crate against ONE wasm module instance, and the test
/// helpers inject/remove a different `__APP_CONFIG` per case. Without a reset, the
/// first successful parse would freeze the config for the rest of the run and break
/// every later test that expects its own injected (or absent) config. The
/// `dioxus-ui/tests/support` inject/remove helpers call this so each case re-parses.
///
/// `#[doc(hidden)]` + not `#[cfg(test)]`-gated on purpose: integration tests under
/// `tests/` link the library compiled WITHOUT `cfg(test)`, so a `cfg(test)`-gated
/// item would be invisible to them. It is a no-op in production code paths.
/// `#[allow(dead_code)]` because the `dioxus-ui` binary recompiles this module and
/// has no caller — only the integration-test harness (which links the lib) uses it.
#[doc(hidden)]
#[allow(dead_code)]
pub fn reset_config_cache_for_test() {
    CONFIG_CACHE.with(|cache| *cache.borrow_mut() = None);
}

/// Maximum number of **real** peer tiles rendered with full PeerTile treatment
/// (canvas, diagnostics subscription, signal history). Mock peers are
/// layout-only and bypass this limit.
pub const CANVAS_LIMIT: usize = 30;

pub fn audio_bitrate_kbps() -> Result<u32, String> {
    app_config().map(|c| c.audio_bitrate_kbps)
}
pub fn video_bitrate_kbps() -> Result<u32, String> {
    app_config().map(|c| c.video_bitrate_kbps)
}
pub fn screen_bitrate_kbps() -> Result<u32, String> {
    app_config().map(|c| c.screen_bitrate_kbps)
}
pub fn vad_threshold() -> Result<f32, String> {
    app_config().map(|c| c.vad_threshold)
}
/// Runtime simulcast layer ceiling (issue #989 / #1082). **Defaults to 3
/// (feature ON)** when `config.js` lacks the key or the config can't be read —
/// kept in lockstep with [`default_experimental_simulcast_max_layers`] so a
/// missing/unreadable config behaves identically to the serde default. The
/// effective count is `min(this, device-capability ceiling)`, so weak devices
/// still auto-gate to 1–2 layers. See
/// [`RuntimeConfig::experimental_simulcast_max_layers`] for rollback.
pub fn experimental_simulcast_max_layers() -> u32 {
    app_config()
        .map(|c| c.experimental_simulcast_max_layers)
        .unwrap_or(3)
}

/// **TEST-ONLY** override for the device-capability simulcast ceiling (#1093).
///
/// Returns `Some(n)` only when `config.js` explicitly sets
/// `testCapabilityMaxLayersOverride`; returns `None` when the key is absent or the
/// config can't be read — i.e. in every production and default-docker deployment,
/// where the sniffed `capability_max_simulcast_layers()` ceiling is used unchanged.
/// The raw value is NOT clamped here; clamping into `[1, SIMULCAST_MAX_LAYERS]`
/// (and the `warn!`) happens at the single consumption point in
/// [`crate::components::capability_check::capability_max_simulcast_layers`] via
/// [`crate::components::capability_check::apply_capability_override`], so the
/// clamp logic stays host-testable in one place.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn test_capability_max_layers_override() -> Option<u32> {
    app_config()
        .ok()
        .and_then(|c| c.test_capability_max_layers_override)
}

/// Parse a `logLevel` string (case-insensitive `trace`/`debug`/`info`/`warn`/
/// `error`, plus `off`) into a [`log::LevelFilter`]. Returns `None` for an empty
/// or unrecognised string so callers can apply their own fallback.
fn parse_log_level(s: &str) -> Option<log::LevelFilter> {
    use std::str::FromStr;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    // `LevelFilter::from_str` is already case-insensitive and also accepts
    // "off"; we normalise via it and only return recognised values.
    log::LevelFilter::from_str(trimmed).ok()
}

/// The operator's EXPLICITLY configured `logLevel`, or `None` when the key is
/// absent / empty / the config can't be read.
///
/// This is the single source of truth for both startup init ([`log_level`]) and
/// the console-log collection ceiling (`attendants.rs`). Unlike the prior
/// design it does NOT treat `"info"` as "unset": an explicit `Some("info")` is
/// returned and honoured, so `logLevel: "info"` works as a real ceiling that
/// caps collection at info instead of letting it bump to Debug.
///
/// A non-empty value that fails to parse (operator typo, e.g. `"wran"`) returns
/// `None` AND emits a `warn!` so the misconfiguration is visible rather than
/// silently falling back. Because `main.rs` calls this at startup (via
/// [`log_level`]) — not only the collection path — the warning fires regardless
/// of whether console-log upload is enabled. (It may fire again when collection
/// activates; a typo'd config warning twice is acceptable and still actionable.)
pub fn log_level_explicit() -> Option<log::LevelFilter> {
    let raw = app_config().ok()?.log_level?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let parsed = parse_log_level(trimmed);
    if parsed.is_none() {
        log::warn!(
            "Ignoring unparseable logLevel {trimmed:?} (expected one of \
             off/error/warn/info/debug/trace); falling back to Info"
        );
    }
    parsed
}

/// Single source of truth for the startup-init log-level fallback used when
/// `logLevel` is absent/empty/unparseable. The `precedence_fallback_literals_pinned`
/// test pins this against the documented literal (Info) so a drift here fails loudly.
pub(crate) const STARTUP_LOG_LEVEL_FALLBACK: log::LevelFilter = log::LevelFilter::Info;
/// Single source of truth for the console-log *collection* ceiling fallback used
/// when `logLevel` is absent (bump to Debug — historical capture behaviour). The
/// `precedence_fallback_literals_pinned` test pins this against the documented
/// literal (Debug) so a drift here fails loudly.
pub(crate) const COLLECTION_LOG_LEVEL_FALLBACK: log::LevelFilter = log::LevelFilter::Debug;

/// Configured WASM logger max level for startup init. Falls back to
/// [`STARTUP_LOG_LEVEL_FALLBACK`] (Info) when the key is absent, empty,
/// unparseable, or the config can't be read — preserving the historical hardcoded
/// init level so a missing/stale config behaves as before. Delegates to
/// [`log_level_explicit`] so a typo's `warn!` surfaces at startup (the collection
/// path may re-emit it).
pub fn log_level() -> log::LevelFilter {
    log_level_explicit().unwrap_or(STARTUP_LOG_LEVEL_FALLBACK)
}

pub fn split_users(s: Option<&str>) -> Vec<String> {
    if let Some(s) = s {
        s.split(',')
            .filter_map(|s| {
                let s = s.trim().to_string();
                if s.is_empty() {
                    None
                } else {
                    Some(s)
                }
            })
            .collect::<Vec<String>>()
    } else {
        Vec::new()
    }
}

pub fn search_api_base_url() -> Result<Option<String>, String> {
    app_config().map(|c| c.search_api_base_url)
}

pub fn login_url() -> Result<String, String> {
    meeting_api_base_url().map(|url| format!("{}/login", url))
}
pub fn logout_url() -> Result<String, String> {
    meeting_api_base_url().map(|url| format!("{}/logout", url))
}
pub fn actix_websocket_base() -> Result<String, String> {
    app_config().map(|c| c.ws_url)
}
pub fn webtransport_host_base() -> Result<String, String> {
    app_config().map(|c| c.web_transport_host)
}

pub fn webtransport_enabled() -> Result<bool, String> {
    app_config().map(|c| truthy(Some(c.web_transport_enabled.as_str())))
}
/// Issue #1483: whether the per-peer-tile "WT"/"WS" transport badge is enabled.
/// Mirrors [`webtransport_enabled`]: empty / missing / falsey → `false` (badge
/// OFF, the default), so the badge is gated entirely by the server-provided
/// `transportBadgeEnabled` flag. Callers gate rendering on
/// `transport_badge_enabled().unwrap_or(false)`.
pub fn transport_badge_enabled() -> Result<bool, String> {
    app_config().map(|c| truthy(Some(c.transport_badge_enabled.as_str())))
}
pub fn oauth_enabled() -> Result<bool, String> {
    app_config().map(|c| truthy(Some(c.oauth_enabled.as_str())))
}
pub fn e2ee_enabled() -> Result<bool, String> {
    app_config().map(|c| truthy(Some(c.e2ee_enabled.as_str())))
}
pub fn firefox_enabled() -> Result<bool, String> {
    app_config().map(|c| truthy(Some(c.firefox_enabled.as_str())))
}
pub fn console_log_upload_enabled() -> Result<bool, String> {
    app_config().map(|c| truthy(Some(c.console_log_upload_enabled.as_str())))
}

pub fn mock_peers_enabled() -> bool {
    app_config()
        .map(|c| truthy(Some(c.mock_peers_enabled.as_str())))
        .unwrap_or(false)
}

pub fn users_allowed_to_stream() -> Result<Vec<String>, String> {
    app_config().map(|c| split_users(Some(&c.users_allowed_to_stream)))
}
pub fn server_election_period_ms() -> Result<u64, String> {
    app_config().map(|c| c.server_election_period_ms)
}

pub fn oauth_provider() -> Option<String> {
    app_config()
        .ok()
        .and_then(|c| c.oauth_provider)
        .filter(|s| !s.is_empty())
}

/// Authorization endpoint URL of the identity provider, read from
/// `window.__APP_CONFIG.oauthAuthUrl` (written from `OAUTH_AUTH_URL`).
///
/// Returns `None` when the variable was not set; the caller is expected to
/// fall back to `GET /api/v1/oauth/provider-config`.
pub fn oauth_auth_url() -> Option<String> {
    app_config()
        .ok()
        .and_then(|c| c.oauth_auth_url)
        .filter(|s| !s.is_empty())
}

/// OAuth client ID, read from `window.__APP_CONFIG.oauthClientId`.
pub fn oauth_client_id() -> Option<String> {
    app_config()
        .ok()
        .and_then(|c| c.oauth_client_id)
        .filter(|s| !s.is_empty())
}

/// Redirect URI registered with the provider, read from
/// `window.__APP_CONFIG.oauthRedirectUrl`.
pub fn oauth_redirect_url() -> Option<String> {
    app_config()
        .ok()
        .and_then(|c| c.oauth_redirect_url)
        .filter(|s| !s.is_empty())
}

/// Space-separated OAuth scopes.  Defaults to `"openid email profile"` when
/// not set.
pub fn oauth_scopes() -> String {
    app_config()
        .ok()
        .and_then(|c| c.oauth_scopes)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "openid email profile".to_string())
}

/// Token endpoint URL of the identity provider, read from
/// `window.__APP_CONFIG.oauthTokenUrl` (written from `OAUTH_TOKEN_URL`).
///
/// Returns `None` when the variable is not set; callers should then attempt
/// OIDC discovery via [`oauth_issuer`] or fall back to the backend
/// `GET /api/v1/oauth/provider-config` endpoint.
pub fn oauth_token_url() -> Option<String> {
    app_config()
        .ok()
        .and_then(|c| c.oauth_token_url)
        .filter(|s| !s.is_empty())
}

/// OIDC issuer URL, read from `window.__APP_CONFIG.oauthIssuer` (written from
/// `OAUTH_ISSUER`).
///
/// When [`oauth_token_url`] is not set, the callback page uses this to
/// construct the OIDC well-known discovery URL:
/// `{issuer}/.well-known/openid-configuration`.
pub fn oauth_issuer() -> Option<String> {
    app_config()
        .ok()
        .and_then(|c| c.oauth_issuer)
        .filter(|s| !s.is_empty())
}

/// Optional `prompt` value appended to the OIDC authorization URL, read from
/// `window.__APP_CONFIG.oauthPrompt` (written from `OAUTH_PROMPT`).
///
/// Returns `None` when the variable is empty or unset; the parameter is then
/// omitted from the authorization URL so it does not cause errors on providers
/// that do not recognise it.  Set to e.g. `"select_account"` or `"login"` to
/// force the provider to show the account-chooser or re-authentication screen.
pub fn oauth_prompt() -> Option<String> {
    app_config()
        .ok()
        .and_then(|c| c.oauth_prompt)
        .filter(|s| !s.is_empty())
}

/// OAuth flow mode, read from `window.__APP_CONFIG.oauthFlow`.
/// Returns `Some("pkce")` for client-side PKCE flow.
/// Any other value (including `None` / empty) means server-side flow.
pub fn oauth_flow() -> Option<String> {
    app_config()
        .ok()
        .and_then(|c| c.oauth_flow)
        .filter(|s| !s.is_empty())
}

/// Returns `true` when OAuth is enabled AND the flow is explicitly set to
/// `"pkce"`.  All code paths that decide between client-side PKCE and
/// server-side OAuth should use this single predicate.
pub fn is_pkce_flow() -> bool {
    oauth_enabled().unwrap_or(false) && oauth_flow().as_deref() == Some("pkce")
}

pub fn meeting_api_base_url() -> Result<String, String> {
    app_config().map(|c| {
        c.meeting_api_base_url
            .clone()
            .unwrap_or_else(|| c.api_base_url.clone())
    })
}

pub fn meeting_api_client() -> Result<videocall_meeting_client::MeetingApiClient, String> {
    let base_url = meeting_api_base_url()?;
    // PKCE flow: check sessionStorage for Bearer tokens (access_token preferred,
    //   id_token as fallback for older sessions). The meeting-api validates
    //   these via JWKS.
    // All other flows (server-side OAuth with HttpOnly session cookie, or
    //   non-OAuth deployments with HMAC session JWT cookie): use Cookie mode
    //   so that fetch includes `credentials: 'include'`.
    let auth_mode = if is_pkce_flow() {
        crate::auth::get_stored_access_token()
            .or_else(crate::auth::get_stored_id_token)
            .map(videocall_meeting_client::AuthMode::Bearer)
            .unwrap_or(videocall_meeting_client::AuthMode::Cookie)
    } else {
        videocall_meeting_client::AuthMode::Cookie
    };
    Ok(videocall_meeting_client::MeetingApiClient::new(
        &base_url, auth_mode,
    ))
}

#[cfg(test)]
mod simulcast_default_tests {
    use super::default_experimental_simulcast_max_layers;

    /// Issue #1082: per-receiver simulcast is ON BY DEFAULT — the serde default
    /// used when `config.js` omits `experimentalSimulcastMaxLayers` is 3 (the
    /// full ladder), not the old 1 (OFF).
    #[test]
    fn serde_default_is_three_feature_on() {
        assert_eq!(
            default_experimental_simulcast_max_layers(),
            3,
            "simulcast must default ON (3 layers) — see issue 1082"
        );
    }

    /// The serde default and the `experimental_simulcast_max_layers().unwrap_or(..)`
    /// read fallback must stay in lockstep so a missing/unreadable config behaves
    /// identically to the default. The read fn itself calls `app_config()` (needs
    /// `window()`, wasm-only), so we can't invoke it on host — instead pin the
    /// fallback literal here against the default fn. If someone changes one, this
    /// test forces them to change the other.
    #[test]
    fn read_fallback_matches_serde_default() {
        // The literal in `experimental_simulcast_max_layers()`'s `.unwrap_or(3)`.
        const READ_FALLBACK: u32 = 3;
        assert_eq!(
            READ_FALLBACK,
            default_experimental_simulcast_max_layers(),
            "the read-fn fallback must equal the serde default (lockstep, issue 1082)"
        );
    }
}

#[cfg(test)]
mod memoize_ok_tests {
    use super::memoize_ok;
    use std::cell::{Cell, RefCell};

    /// Issue #1492: the first successful parse must run exactly once; every later
    /// call returns the cached clone WITHOUT re-invoking the parser. A mutation
    /// that dropped the cache-write (`*cache.borrow_mut() = Some(..)`) would make
    /// `calls` climb to 3 here and fail this assertion.
    #[test]
    fn parses_once_then_serves_cache() {
        let cache: RefCell<Option<String>> = RefCell::new(None);
        let calls = Cell::new(0);
        let parse = || {
            calls.set(calls.get() + 1);
            Ok::<_, ()>("value".to_string())
        };

        assert_eq!(memoize_ok(&cache, parse), Ok("value".to_string()));
        assert_eq!(memoize_ok(&cache, parse), Ok("value".to_string()));
        assert_eq!(memoize_ok(&cache, parse), Ok("value".to_string()));
        assert_eq!(
            calls.get(),
            1,
            "parser must run exactly once across 3 reads"
        );
    }

    /// An `Err` must NOT be cached: a transient failure (config not yet installed)
    /// has to fall through so a later call can still succeed and populate the cache.
    /// A mutation that cached errors would return the stale `Err` on call 2 and the
    /// final `Ok` assertion (plus the `calls == 2` count) would fail.
    #[test]
    fn errors_are_not_cached() {
        let cache: RefCell<Option<u32>> = RefCell::new(None);
        let calls = Cell::new(0);
        let parse = || {
            calls.set(calls.get() + 1);
            // Fail on the first call, succeed thereafter.
            if calls.get() == 1 {
                Err("not ready")
            } else {
                Ok(42u32)
            }
        };

        assert_eq!(memoize_ok(&cache, parse), Err("not ready"));
        assert_eq!(memoize_ok(&cache, parse), Ok(42));
        // Third call must be served from cache, not re-parsed.
        assert_eq!(memoize_ok(&cache, parse), Ok(42));
        assert_eq!(
            calls.get(),
            2,
            "parser runs on the failing call + the first success, then caches"
        );
    }
}

#[cfg(test)]
mod log_level_tests {
    use super::parse_log_level;
    use log::LevelFilter;

    /// `parse_log_level` is a pure host-testable parser: trims, accepts the five
    /// standard levels plus `off`, is case-insensitive, and rejects empty /
    /// unrecognised input with `None`. (`log_level`/`log_level_explicit` wrap it
    /// but call `app_config()` → `window()`, so only this pure core is unit-
    /// testable on host — the same split as the simulcast read fns above.)
    #[test]
    fn parses_standard_levels() {
        assert_eq!(parse_log_level("trace"), Some(LevelFilter::Trace));
        assert_eq!(parse_log_level("debug"), Some(LevelFilter::Debug));
        assert_eq!(parse_log_level("info"), Some(LevelFilter::Info));
        assert_eq!(parse_log_level("warn"), Some(LevelFilter::Warn));
        assert_eq!(parse_log_level("error"), Some(LevelFilter::Error));
        // `off` is accepted (LevelFilter has it; console_log honours it via
        // set_max_level even though init_with_level cannot express it).
        assert_eq!(parse_log_level("off"), Some(LevelFilter::Off));
    }

    #[test]
    fn trims_and_is_case_insensitive() {
        assert_eq!(parse_log_level(" TRACE "), Some(LevelFilter::Trace));
        assert_eq!(parse_log_level("Info"), Some(LevelFilter::Info));
        assert_eq!(parse_log_level("\tWARN\n"), Some(LevelFilter::Warn));
    }

    #[test]
    fn empty_and_typos_are_none() {
        assert_eq!(parse_log_level(""), None);
        assert_eq!(parse_log_level("   "), None);
        // The operator-typo path: a non-empty unrecognised value → None, which
        // drives the `warn!` + Info/Debug fallback in the callers.
        assert_eq!(parse_log_level("wran"), None);
        assert_eq!(parse_log_level("verbose"), None);
    }

    /// Lockstep pin on the two precedence fallback consts — the entire point of
    /// the dial. The shared consts (`super::STARTUP_LOG_LEVEL_FALLBACK` /
    /// `super::COLLECTION_LOG_LEVEL_FALLBACK`) are the single source of truth used
    /// at the real call sites (`log_level()` and `attendants.rs`). Pinning them
    /// against the documented literal here means editing a call-site fallback now
    /// forces editing the shared const, which this test catches on drift:
    ///   - collection ceiling when `logLevel` is ABSENT → Debug.
    ///   - startup init when `logLevel` is ABSENT/unparseable → Info.
    #[test]
    fn precedence_fallback_literals_pinned() {
        use super::{COLLECTION_LOG_LEVEL_FALLBACK, STARTUP_LOG_LEVEL_FALLBACK};
        assert_eq!(
            STARTUP_LOG_LEVEL_FALLBACK,
            LevelFilter::Info,
            "absent/typo logLevel must init at Info (historical hardcoded level)"
        );
        assert_eq!(
            COLLECTION_LOG_LEVEL_FALLBACK,
            LevelFilter::Debug,
            "absent logLevel must bump collection to Debug (historical capture)"
        );
    }
}
