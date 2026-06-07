/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use url::Url;

use crate::netsim::NetworkProfile;
use videocall_netsim::{list_profiles, resolve_profile};

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    WebSocket,
    #[default]
    WebTransport,
}

/// Video rendering mode for the bot.
#[derive(Debug, Default, Deserialize, Serialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum VideoMode {
    /// Animated EKG waveform driven by audio RMS.
    #[default]
    Ekg,
    /// Pre-rendered costume sprite sheets (idle + talking).
    Costume,
}

/// Bot connection configuration (YAML file).
///
/// Participant details come from the conversation manifest, not from this config.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct BotConfig {
    /// Legacy single-server URL. Use `ws_url`/`wt_url` for new-style config.
    pub server_url: Option<String>,
    /// Legacy transport selector. Use `ws_url`/`wt_url` for new-style config.
    #[serde(default)]
    pub transport: Option<Transport>,
    /// WebSocket relay URL (new-style). Mutually exclusive with `server_url`.
    pub ws_url: Option<String>,
    /// WebTransport relay URL (new-style). Mutually exclusive with `server_url`.
    pub wt_url: Option<String>,
    /// Fraction of bots (0.0..=1.0) assigned to WebTransport when both URLs are set.
    /// Uses `f64::round()` on `ratio * total_bots` to decide how many bots get WT;
    /// e.g. `wt_ratio=0.33` with `total_bots=10` yields 3 WT bots. The first N
    /// bots by index are assigned WT, and the remainder use WS.
    pub wt_ratio: Option<f64>,
    pub jwt_secret: Option<String>,
    pub token_ttl_secs: Option<u64>,
    pub insecure: Option<bool>,
    pub ramp_up_delay_ms: Option<u64>,
    /// Meeting room ID -- all participants join the same meeting.
    pub meeting_id: String,
    /// Path to conversation asset directory (contains manifest.yaml + lines/).
    /// Defaults to "conversation".
    pub conversation_dir: Option<String>,
    /// Video rendering mode (ekg or costume). Defaults to ekg.
    #[serde(default)]
    pub video_mode: VideoMode,
    /// Warmup delay (seconds) after all bots are spawned before media starts.
    pub warmup_secs: Option<u64>,
    /// Number of participants that broadcast A/V. 0 (or omitted) means all broadcast.
    pub broadcasters: Option<usize>,
    /// Viewport fidelity for load tests (HCL issue #988): "render N of M peers".
    ///
    /// When set, each bot emits a `VIEWPORT` control packet listing only the
    /// first `N` source session_ids it has discovered (sorted ascending for
    /// reproducibility). A #988-enabled relay then stops forwarding VIDEO from
    /// the off-screen peers, so the load test measures realistic relay fan-out
    /// instead of the optimistic "every bot decodes everyone" case.
    ///
    /// `None` (the default) preserves legacy behaviour: the bot never sends a
    /// VIEWPORT and the relay forwards every stream (fail-open).
    ///
    /// Note the relay treats an *empty* viewport as "no signal → fail-open", so
    /// the bot only emits a VIEWPORT once it has selected at least one visible
    /// peer. `Some(0)` therefore behaves like `None` in practice (nothing to
    /// render means nothing to signal); use a small positive `N` to exercise
    /// filtering.
    #[serde(default)]
    pub viewport_visible_count: Option<usize>,
    /// Per-receiver simulcast layer-preference fidelity (HCL follow-up #1083-A2).
    ///
    /// When set, each bot emits a `LAYER_PREFERENCE` control packet pinning every
    /// source `session_id` it discovers to this simulcast layer — exactly like a
    /// real browser receiver that selected a fixed quality tier. `Some(0)` =
    /// "BASE LAYER ONLY" (drop every upgraded layer from each source); a
    /// per-receiver-simulcast relay then forwards only that layer to this bot, so
    /// a load test can validate the relay's layer-filter (the pinned bot's
    /// per-source `video_bytes` should fall to the base-layer rate while a
    /// no-preference bot keeps the full ladder).
    ///
    /// `None` (the default) preserves legacy behaviour: the bot never sends a
    /// LAYER_PREFERENCE and the relay forwards every layer (fail-open). The bot
    /// has NO receiver chooser, so this is the only way it expresses a layer
    /// preference — there is no dynamic per-tile selection.
    ///
    /// CLI: `--pin-layer <N>`. Env: `BOT_PIN_LAYER=<N>`.
    #[serde(default)]
    pub pin_layer: Option<u32>,
    /// Which media kind the `pin_layer` preference constrains (`video` | `audio`
    /// | `screen`). Only meaningful when `pin_layer` is set. Defaults to `video`
    /// — the only media kind the relay layer-filters today.
    ///
    /// CLI: `--pin-layer-kind <kind>`. Env: `BOT_PIN_LAYER_KIND=<kind>`.
    #[serde(default)]
    pub pin_layer_kind: Option<String>,
    /// Number of simultaneous simulcast VIDEO layers this bot PRODUCES (#989).
    ///
    /// Mirrors the real browser client: when `>= 2`, the bot runs one VP9
    /// encoder per layer at the simulcast ladder's FIXED tier resolution
    /// (lowest layer first, from `videocall_aq::constants::simulcast_layers`)
    /// and stamps `PacketWrapper.simulcast_layer_id` per layer so the relay can
    /// exercise its per-receiver layer SELECTION/forwarding path.
    ///
    /// `None` (the default) resolves to **3 layers** via
    /// [`Self::simulcast_layer_count`]. `Some(1)` reproduces today's exact
    /// single-stream behaviour byte-for-byte (one AQ-adaptive encoder, layer 0)
    /// and is the A/B rollback path. Values are clamped to
    /// `1..=SIMULCAST_MAX_LAYERS`.
    ///
    /// CLI: `--simulcast-layers <N>`. Env: `BOT_SIMULCAST_LAYERS=<N>`.
    #[serde(default)]
    pub simulcast_layers: Option<u32>,
    /// CLI-only: apply this preset to every participant that has no `network:`
    /// block of its own. Never overrides manifest settings; only fills gaps.
    #[serde(default, skip)]
    pub impair_all: Option<String>,
    /// CLI-only: strict per-participant override, as `name → preset`. Takes
    /// precedence over both manifest `network:` and `impair_all`.
    #[serde(default, skip)]
    pub impair_name: HashMap<String, String>,
    /// CLI-only: force-disable impairment for every participant. Highest
    /// precedence of the impairment knobs.
    #[serde(default, skip)]
    pub no_impair: bool,
    /// CLI-only: HTTP port for the Prometheus `/metrics` endpoint. `None`
    /// (the default) disables the endpoint entirely. Only honored when the
    /// crate is built with `--features metrics`.
    #[serde(default, skip)]
    pub metrics_port: Option<u16>,
    /// CLI-only: bind address for the Prometheus `/metrics` endpoint.
    /// Defaults to `127.0.0.1` so the endpoint — which exposes meeting and
    /// user identifiers as Prometheus label values — is not reachable from
    /// the network. Operators who need fleet-wide scraping can pass
    /// `0.0.0.0` (or a specific NIC IP) via `--metrics-bind`. Only honored
    /// when the crate is built with `--features metrics`.
    #[serde(default, skip)]
    pub metrics_bind: Option<std::net::IpAddr>,
    /// CLI-only: exit immediately if costume memory exceeds 80% of available RAM.
    #[serde(default, skip)]
    pub strict_memory: bool,
}

/// Minimal client identity -- used only by the transport layer.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub user_id: String,
    pub meeting_id: String,
    pub enable_audio: bool,
    pub enable_video: bool,
}

impl BotConfig {
    pub fn from_file(path: &str) -> anyhow::Result<Self> {
        let content = fs::read_to_string(path)?;
        let config: BotConfig = serde_yaml::from_str(&content)?;
        Ok(config)
    }

    /// Load config from CLI args. Returns (config, num_users).
    ///
    /// Usage:
    /// ```text
    /// bot --config <file> [--users <N>]
    ///     [--impair-all <preset>]
    ///     [--impair-name <name>=<preset>]...
    ///     [--no-impair]
    /// ```
    ///
    /// `--users 0` or omitting it means "all participants from manifest".
    ///
    /// Impairment precedence (highest to lowest):
    /// `--no-impair` > `--impair-name` > manifest `network:` > `--impair-all`
    /// > passthrough.
    pub fn from_args() -> anyhow::Result<(Self, usize)> {
        let args: Vec<String> = std::env::args().collect();
        let env_config_path = std::env::var("BOT_CONFIG_PATH").ok();
        Self::from_args_inner(&args, env_config_path)
    }

    fn from_args_inner(
        args: &[String],
        env_config_path: Option<String>,
    ) -> anyhow::Result<(Self, usize)> {
        let mut config_path: Option<String> = None;
        let mut num_users: usize = 0;
        let mut impair_all: Option<String> = None;
        let mut impair_name: HashMap<String, String> = HashMap::new();
        let mut no_impair = false;
        let mut metrics_port: Option<u16> = None;
        let mut metrics_bind: Option<std::net::IpAddr> = None;
        let mut strict_memory = false;
        let mut pin_layer: Option<u32> = None;
        let mut pin_layer_kind: Option<String> = None;
        let mut simulcast_layers: Option<u32> = None;

        let mut i = 1; // skip argv[0]
        while i < args.len() {
            match args[i].as_str() {
                "--config" | "-c" => {
                    if i + 1 < args.len() {
                        config_path = Some(args[i + 1].clone());
                        i += 2;
                    } else {
                        return Err(anyhow!("--config requires a path argument"));
                    }
                }
                "--users" | "-n" => {
                    if i + 1 < args.len() {
                        num_users = args[i + 1]
                            .parse()
                            .map_err(|_| anyhow!("--users requires a number"))?;
                        i += 2;
                    } else {
                        return Err(anyhow!("--users requires a number argument"));
                    }
                }
                "--impair-all" => {
                    if i + 1 < args.len() {
                        let preset = args[i + 1].clone();
                        if resolve_profile(&preset).is_none() {
                            return Err(anyhow!(
                                "--impair-all: unknown preset '{}'. Known: {}",
                                preset,
                                list_profiles().join(", ")
                            ));
                        }
                        impair_all = Some(preset);
                        i += 2;
                    } else {
                        return Err(anyhow!("--impair-all requires a preset name"));
                    }
                }
                "--impair-name" => {
                    if i + 1 < args.len() {
                        let raw = &args[i + 1];
                        let (name, preset) = raw.split_once('=').ok_or_else(|| {
                            anyhow!("--impair-name expects <name>=<preset>, got '{}'", raw)
                        })?;
                        if resolve_profile(preset).is_none() {
                            return Err(anyhow!(
                                "--impair-name {}: unknown preset '{}'. Known: {}",
                                name,
                                preset,
                                list_profiles().join(", ")
                            ));
                        }
                        impair_name.insert(name.to_string(), preset.to_string());
                        i += 2;
                    } else {
                        return Err(anyhow!("--impair-name requires <name>=<preset> argument"));
                    }
                }
                "--no-impair" => {
                    no_impair = true;
                    i += 1;
                }
                "--metrics-port" => {
                    if i + 1 < args.len() {
                        metrics_port =
                            Some(args[i + 1].parse().map_err(|_| {
                                anyhow!("--metrics-port requires a u16 port number")
                            })?);
                        i += 2;
                    } else {
                        return Err(anyhow!("--metrics-port requires a port argument"));
                    }
                }
                "--metrics-bind" => {
                    if i + 1 < args.len() {
                        metrics_bind = Some(args[i + 1].parse().map_err(|_| {
                            anyhow!(
                                "--metrics-bind requires an IP address (e.g. 127.0.0.1 or 0.0.0.0)"
                            )
                        })?);
                        i += 2;
                    } else {
                        return Err(anyhow!("--metrics-bind requires an IP address argument"));
                    }
                }
                "--strict-memory" => {
                    strict_memory = true;
                    i += 1;
                }
                "--pin-layer" => {
                    if i + 1 < args.len() {
                        pin_layer = Some(
                            args[i + 1]
                                .parse()
                                .map_err(|_| anyhow!("--pin-layer requires a u32 layer index"))?,
                        );
                        i += 2;
                    } else {
                        return Err(anyhow!("--pin-layer requires a layer-index argument"));
                    }
                }
                "--pin-layer-kind" => {
                    if i + 1 < args.len() {
                        let kind = args[i + 1].clone();
                        if crate::layer_preference_sender::PinMediaKind::parse(&kind).is_none() {
                            return Err(anyhow!(
                                "--pin-layer-kind: unknown kind '{}'. Use video, audio, or screen.",
                                kind
                            ));
                        }
                        pin_layer_kind = Some(kind);
                        i += 2;
                    } else {
                        return Err(anyhow!(
                            "--pin-layer-kind requires <video|audio|screen> argument"
                        ));
                    }
                }
                "--simulcast-layers" => {
                    if i + 1 < args.len() {
                        simulcast_layers = Some(args[i + 1].parse().map_err(|_| {
                            anyhow!("--simulcast-layers requires a u32 layer count")
                        })?);
                        i += 2;
                    } else {
                        return Err(anyhow!("--simulcast-layers requires a count argument"));
                    }
                }
                "--help" | "-h" => {
                    println!("{}", help_text());
                    std::process::exit(0);
                }
                _ => {
                    i += 1;
                }
            }
        }

        let mut config = match config_path {
            Some(p) => Self::from_file(&p)?,
            None => {
                if let Some(env_path) = env_config_path {
                    Self::from_file(&env_path)?
                } else {
                    return Err(anyhow!("{}", help_text()));
                }
            }
        };

        config.impair_all = impair_all;
        config.impair_name = impair_name;
        config.no_impair = no_impair;
        config.metrics_port = metrics_port;
        config.metrics_bind = metrics_bind;
        config.strict_memory = strict_memory;

        // Layer-preference pin (#1083-A2). Precedence: CLI flag > env var > YAML
        // file value. Mirrors how the viewport knob is configured (config field)
        // while also accepting CLI/env like the metrics/impairment knobs, so a
        // single bot can be launched in "pin to layer N" mode without editing
        // the shared config file. Default stays OFF (bot behaviour unchanged).
        if let Some(layer) = pin_layer {
            config.pin_layer = Some(layer);
        } else if let Ok(env_layer) = std::env::var("BOT_PIN_LAYER") {
            config.pin_layer = Some(
                env_layer
                    .parse()
                    .map_err(|_| anyhow!("BOT_PIN_LAYER must be a u32 layer index"))?,
            );
        }
        if let Some(kind) = pin_layer_kind {
            config.pin_layer_kind = Some(kind);
        } else if let Ok(env_kind) = std::env::var("BOT_PIN_LAYER_KIND") {
            if crate::layer_preference_sender::PinMediaKind::parse(&env_kind).is_none() {
                return Err(anyhow!(
                    "BOT_PIN_LAYER_KIND: unknown kind '{}'. Use video, audio, or screen.",
                    env_kind
                ));
            }
            config.pin_layer_kind = Some(env_kind);
        }

        // Simulcast layer count (#989). Precedence: CLI flag > env var > YAML
        // file value > default (3, resolved later in `simulcast_layer_count`).
        // Mirrors the pin-layer knob above. The raw value is stored unclamped;
        // `simulcast_layer_count()` applies the `1..=SIMULCAST_MAX_LAYERS` clamp
        // and the default-3 fallback at use sites.
        if let Some(n) = simulcast_layers {
            config.simulcast_layers = Some(n);
        } else if let Ok(env_n) = std::env::var("BOT_SIMULCAST_LAYERS") {
            config.simulcast_layers = Some(
                env_n
                    .parse()
                    .map_err(|_| anyhow!("BOT_SIMULCAST_LAYERS must be a u32 layer count"))?,
            );
        }

        Ok((config, num_users))
    }

    /// Resolve the network profile for a single participant, honoring the
    /// configured precedence order. Returns the passthrough profile when no
    /// impairment applies.
    pub fn resolve_network(&self, participant: &Participant) -> anyhow::Result<NetworkProfile> {
        if self.no_impair {
            return Ok(NetworkProfile::passthrough());
        }

        if let Some(preset) = self.impair_name.get(&participant.name) {
            let profile = resolve_profile(preset).ok_or_else(|| {
                anyhow!(
                    "--impair-name {}: unknown preset '{}'. Known: {}",
                    participant.name,
                    preset,
                    list_profiles().join(", ")
                )
            })?;
            profile.validate().map_err(|e| {
                anyhow!(
                    "invalid preset '{}' for {}: {}",
                    preset,
                    participant.name,
                    e
                )
            })?;
            return Ok(profile);
        }

        if let Some(net) = &participant.network {
            let profile = net
                .resolve()
                .map_err(|e| anyhow!("participant '{}' network: {}", participant.name, e))?;
            return Ok(profile);
        }

        if let Some(preset) = &self.impair_all {
            let profile = resolve_profile(preset).ok_or_else(|| {
                anyhow!(
                    "--impair-all: unknown preset '{}'. Known: {}",
                    preset,
                    list_profiles().join(", ")
                )
            })?;
            profile
                .validate()
                .map_err(|e| anyhow!("invalid --impair-all preset '{}': {}", preset, e))?;
            return Ok(profile);
        }

        Ok(NetworkProfile::passthrough())
    }

    /// Resolve the transport and server URL for a given bot index.
    ///
    /// New-style config: `ws_url` and/or `wt_url` with optional `wt_ratio`.
    /// Legacy config: single `server_url` + `transport` field.
    pub fn resolve_transport(
        &self,
        bot_index: usize,
        total_bots: usize,
    ) -> anyhow::Result<(Transport, Url)> {
        // New-style: ws_url / wt_url with ratio-based split
        if self.ws_url.is_some() || self.wt_url.is_some() {
            let ratio = self.wt_ratio.unwrap_or(0.0).clamp(0.0, 1.0);
            let use_wt = if self.wt_url.is_some() && self.ws_url.is_some() {
                // Assign first (ratio * total_bots) bots to WT, rest to WS
                let wt_count = (ratio * total_bots as f64).round() as usize;
                bot_index < wt_count
            } else {
                self.wt_url.is_some()
            };

            if use_wt {
                let url_str = self.wt_url.as_ref().ok_or_else(|| {
                    anyhow!(
                        "wt_url not set but bot_index {} selected for WebTransport",
                        bot_index
                    )
                })?;
                let url = Url::parse(url_str)
                    .map_err(|e| anyhow!("Invalid wt_url '{}': {}", url_str, e))?;
                Ok((Transport::WebTransport, url))
            } else {
                let url_str = self.ws_url.as_ref().ok_or_else(|| {
                    anyhow!(
                        "ws_url not set but bot_index {} selected for WebSocket",
                        bot_index
                    )
                })?;
                let url = Url::parse(url_str)
                    .map_err(|e| anyhow!("Invalid ws_url '{}': {}", url_str, e))?;
                Ok((Transport::WebSocket, url))
            }
        } else if let Some(ref server_url) = self.server_url {
            // Legacy fallback
            let transport = self.transport.clone().unwrap_or_default();
            let url = Url::parse(server_url)
                .map_err(|e| anyhow!("Invalid server_url '{}': {}", server_url, e))?;
            Ok((transport, url))
        } else {
            Err(anyhow!(
                "No server URL configured. Set ws_url/wt_url or legacy server_url."
            ))
        }
    }

    pub fn token_ttl_secs(&self) -> u64 {
        self.token_ttl_secs.unwrap_or(86400)
    }

    pub fn conversation_dir(&self) -> &str {
        self.conversation_dir.as_deref().unwrap_or("conversation")
    }

    pub fn warmup_secs(&self) -> u64 {
        self.warmup_secs.unwrap_or(15)
    }

    pub fn broadcasters(&self) -> usize {
        self.broadcasters.unwrap_or(0)
    }

    /// Resolve the media kind the `pin_layer` preference applies to. Defaults to
    /// VIDEO when unset or unparseable (the only kind the relay layer-filters).
    pub fn pin_media_kind(&self) -> crate::layer_preference_sender::PinMediaKind {
        use crate::layer_preference_sender::PinMediaKind;
        self.pin_layer_kind
            .as_deref()
            .and_then(PinMediaKind::parse)
            .unwrap_or(PinMediaKind::Video)
    }

    /// Resolve how many simultaneous simulcast VIDEO layers this bot produces.
    ///
    /// Default (field unset) is **3** — the full ladder, matching the browser
    /// client. The value is clamped into `1..=SIMULCAST_MAX_LAYERS` so an
    /// out-of-range config degrades to the nearest valid ladder rather than
    /// panicking. `1` is the single-stream rollback path.
    pub fn simulcast_layer_count(&self) -> u32 {
        use videocall_aq::constants::SIMULCAST_MAX_LAYERS;
        self.simulcast_layers
            .unwrap_or(3)
            .clamp(1, SIMULCAST_MAX_LAYERS as u32)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostumeMemoryDecision {
    Ok,
    Warn,
    AbortExceedsAvailable,
    AbortStrictThreshold,
}

pub fn evaluate_costume_memory(
    total_costume_bytes: u64,
    available_bytes: u64,
    strict_memory: bool,
) -> CostumeMemoryDecision {
    if total_costume_bytes > available_bytes {
        CostumeMemoryDecision::AbortExceedsAvailable
    } else if total_costume_bytes > available_bytes.saturating_mul(80) / 100 {
        if strict_memory {
            CostumeMemoryDecision::AbortStrictThreshold
        } else {
            CostumeMemoryDecision::Warn
        }
    } else {
        CostumeMemoryDecision::Ok
    }
}

// ---------------------------------------------------------------------------
// Conversation manifest (generated by generate-conversation-edge.py)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Clone)]
pub struct Manifest {
    pub participants: Vec<Participant>,
    pub pause_ms: u64,
    pub lines: Vec<Line>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Participant {
    pub name: String,
    #[allow(dead_code)]
    pub voice: String,
    #[serde(default = "default_ekg_color")]
    pub ekg_color: [u8; 3],
    /// Path to costume sprite sheet directory (for VideoMode::Costume).
    pub costume_dir: Option<String>,
    /// Optional per-participant network-impairment block.
    #[serde(default)]
    pub network: Option<ParticipantNetwork>,
}

/// Manifest-level network impairment for a single participant.
///
/// Either set `profile` to a preset name, **or** supply inline fields —
/// mixing the two is rejected to avoid "which one wins" ambiguity.
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct ParticipantNetwork {
    /// Name of a preset from [`videocall_netsim::profiles`].
    pub profile: Option<String>,
    pub latency_ms: Option<u32>,
    pub jitter_ms: Option<u32>,
    pub loss_pct: Option<f32>,
    pub duplicate_pct: Option<f32>,
    pub reorder_pct: Option<f32>,
    pub uplink_kbps: Option<u32>,
    pub downlink_kbps: Option<u32>,
    pub seed: Option<u64>,
}

impl ParticipantNetwork {
    /// Produce a validated [`NetworkProfile`] from this block. Returns a
    /// human-readable error on validation failure.
    pub fn resolve(&self) -> Result<NetworkProfile, String> {
        let has_inline = self.latency_ms.is_some()
            || self.jitter_ms.is_some()
            || self.loss_pct.is_some()
            || self.duplicate_pct.is_some()
            || self.reorder_pct.is_some()
            || self.uplink_kbps.is_some()
            || self.downlink_kbps.is_some();

        if self.profile.is_some() && has_inline {
            return Err(
                "cannot combine `profile:` with inline fields — use one or the other".to_string(),
            );
        }

        let mut profile = if let Some(name) = &self.profile {
            resolve_profile(name).ok_or_else(|| {
                format!(
                    "unknown network profile '{}'. Known: {}",
                    name,
                    list_profiles().join(", ")
                )
            })?
        } else {
            NetworkProfile::passthrough()
        };

        if let Some(v) = self.latency_ms {
            profile.latency_ms = v;
        }
        if let Some(mut v) = self.jitter_ms {
            // Clamp to latency_ms — noisy jitter larger than the base latency
            // makes timing non-monotonic and isn't what users want.
            if v > profile.latency_ms {
                tracing::warn!(
                    "jitter_ms={} exceeds latency_ms={}; clamping to latency",
                    v,
                    profile.latency_ms
                );
                v = profile.latency_ms;
            }
            profile.jitter_ms = v;
        }
        if let Some(v) = self.loss_pct {
            profile.loss_pct = v;
        }
        if let Some(v) = self.duplicate_pct {
            profile.duplicate_pct = v;
        }
        if let Some(v) = self.reorder_pct {
            profile.reorder_pct = v;
        }
        if let Some(v) = self.uplink_kbps {
            profile.uplink_kbps = Some(v);
        }
        if let Some(v) = self.downlink_kbps {
            profile.downlink_kbps = Some(v);
        }
        if let Some(v) = self.seed {
            profile.seed = Some(v);
        }

        profile.validate()?;
        Ok(profile)
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct Line {
    pub speaker: String,
    pub audio_file: String,
    #[allow(dead_code)]
    pub duration_ms: u64,
}

fn default_ekg_color() -> [u8; 3] {
    [100, 100, 100]
}

impl Manifest {
    pub fn from_file(path: &str) -> anyhow::Result<Self> {
        let content = fs::read_to_string(path)?;
        let manifest: Manifest = serde_yaml::from_str(&content)?;
        Ok(manifest)
    }
}

/// Rendered `--help` text for the bot CLI.
fn help_text() -> String {
    format!(
        "Usage: bot --config <file> [--users <N>] [impairment flags]\n\
         Or set BOT_CONFIG_PATH environment variable.\n\
         \n\
         Options:\n\
         \x20 --config, -c <file>           Path to bot config YAML.\n\
         \x20 --users, -n <N>               Number of participants (0 = all in manifest).\n\
         \n\
         Network impairment (all optional):\n\
         \x20 --impair-all <preset>         Apply preset to every participant that has no\n\
         \x20                               `network:` block in the manifest. Lowest precedence.\n\
         \x20 --impair-name <name>=<preset> Strict override of one participant's network\n\
         \x20                               settings. Repeatable.\n\
         \x20 --no-impair                   Force-disable all impairment. Highest precedence.\n\
         \n\
         Safety:\n\
         \x20 --strict-memory               Exit with code 1 if costume frames exceed 80%% of\n\
         \x20                               available RAM (default: warn only).\n\
         \n\
         Simulcast layer preference (#1083; off by default):\n\
         \x20 --pin-layer <N>               Emit a LAYER_PREFERENCE pinning every discovered\n\
         \x20                               source to simulcast layer N (0 = base layer only).\n\
         \x20                               Validates the relay's per-receiver layer filter.\n\
         \x20                               Also via BOT_PIN_LAYER env var.\n\
         \x20 --pin-layer-kind <kind>       Media kind to constrain: video (default), audio,\n\
         \x20                               or screen. Also via BOT_PIN_LAYER_KIND env var.\n\
         \n\
         Simulcast production (#989; produces multiple layers):\n\
         \x20 --simulcast-layers <N>        Number of simultaneous VIDEO layers this bot\n\
         \x20                               PUBLISHES (1..=3, default 3). N>=2 runs one VP9\n\
         \x20                               encoder per ladder tier and stamps\n\
         \x20                               simulcast_layer_id per layer. N=1 = legacy single\n\
         \x20                               AQ-adaptive stream. Also via BOT_SIMULCAST_LAYERS.\n\
         \n\
         Observability (requires `--features metrics` at build time):\n\
         \x20 --metrics-port <port>         Start a Prometheus `/metrics` HTTP endpoint on the\n\
         \x20                               given port (off by default).\n\
         \x20 --metrics-bind <addr>         Bind address for the metrics endpoint. Defaults to\n\
         \x20                               127.0.0.1 so meeting/user labels are not exposed to\n\
         \x20                               the network. Pass 0.0.0.0 for fleet-wide scraping.\n\
         \n\
         Impairment precedence (highest to lowest):\n\
         \x20 --no-impair > --impair-name > manifest `network:` > --impair-all > passthrough\n\
         \n\
         Known presets: {}\n",
        list_profiles().join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::{evaluate_costume_memory, help_text, BotConfig, CostumeMemoryDecision};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn write_temp_config() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("bot-config-{unique}.yaml"));
        fs::write(
            &path,
            "meeting_id: test-room\nws_url: wss://example.invalid/lobby\n",
        )
        .unwrap();
        path
    }

    #[test]
    fn strict_memory_flag_defaults_to_false() {
        let path = write_temp_config();
        let args = vec![
            "bot".to_string(),
            "--config".to_string(),
            path.display().to_string(),
        ];
        let (config, users) = BotConfig::from_args_inner(&args, None).unwrap();
        assert!(!config.strict_memory);
        assert_eq!(users, 0);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn strict_memory_flag_parses_from_args() {
        let path = write_temp_config();
        let args = vec![
            "bot".to_string(),
            "--config".to_string(),
            path.display().to_string(),
            "--strict-memory".to_string(),
        ];
        let (config, _) = BotConfig::from_args_inner(&args, None).unwrap();
        assert!(config.strict_memory);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn help_text_mentions_strict_memory() {
        let help = help_text();
        assert!(help.contains("Safety:"));
        assert!(help.contains("--strict-memory"));
    }

    #[test]
    fn pin_layer_defaults_to_off() {
        let path = write_temp_config();
        let args = vec![
            "bot".to_string(),
            "--config".to_string(),
            path.display().to_string(),
        ];
        let (config, _) = BotConfig::from_args_inner(&args, None).unwrap();
        assert!(
            config.pin_layer.is_none(),
            "pin_layer must default to None (legacy fail-open)"
        );
        // Accessor still resolves a sane default kind for the disabled case.
        assert_eq!(
            config.pin_media_kind(),
            crate::layer_preference_sender::PinMediaKind::Video
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn pin_layer_flag_parses_layer_and_kind() {
        let path = write_temp_config();
        let args = vec![
            "bot".to_string(),
            "--config".to_string(),
            path.display().to_string(),
            "--pin-layer".to_string(),
            "0".to_string(),
            "--pin-layer-kind".to_string(),
            "screen".to_string(),
        ];
        let (config, _) = BotConfig::from_args_inner(&args, None).unwrap();
        assert_eq!(config.pin_layer, Some(0));
        assert_eq!(
            config.pin_media_kind(),
            crate::layer_preference_sender::PinMediaKind::Screen
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn pin_layer_kind_rejects_unknown_value() {
        let path = write_temp_config();
        let args = vec![
            "bot".to_string(),
            "--config".to_string(),
            path.display().to_string(),
            "--pin-layer-kind".to_string(),
            "garbage".to_string(),
        ];
        let err = BotConfig::from_args_inner(&args, None).unwrap_err();
        assert!(
            err.to_string().contains("unknown kind"),
            "unexpected error: {}",
            err
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn help_text_mentions_pin_layer() {
        let help = help_text();
        assert!(help.contains("--pin-layer"));
        assert!(help.contains("--pin-layer-kind"));
    }

    #[test]
    fn simulcast_layers_defaults_to_3() {
        let path = write_temp_config();
        let args = vec![
            "bot".to_string(),
            "--config".to_string(),
            path.display().to_string(),
        ];
        let (config, _) = BotConfig::from_args_inner(&args, None).unwrap();
        assert!(
            config.simulcast_layers.is_none(),
            "raw field must stay None when the flag/env are absent"
        );
        // The resolved count — what the producers actually consume — must be 3.
        assert_eq!(
            config.simulcast_layer_count(),
            3,
            "absent simulcast-layers must resolve to the full 3-layer ladder"
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn simulcast_layers_flag_parses() {
        let path = write_temp_config();
        let args = vec![
            "bot".to_string(),
            "--config".to_string(),
            path.display().to_string(),
            "--simulcast-layers".to_string(),
            "1".to_string(),
        ];
        let (config, _) = BotConfig::from_args_inner(&args, None).unwrap();
        assert_eq!(config.simulcast_layers, Some(1));
        assert_eq!(
            config.simulcast_layer_count(),
            1,
            "explicit --simulcast-layers 1 must resolve to the single-stream path"
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn simulcast_layers_clamps() {
        let path = write_temp_config();
        // Above SIMULCAST_MAX_LAYERS (3) clamps down to 3.
        let args_high = vec![
            "bot".to_string(),
            "--config".to_string(),
            path.display().to_string(),
            "--simulcast-layers".to_string(),
            "99".to_string(),
        ];
        let (config_high, _) = BotConfig::from_args_inner(&args_high, None).unwrap();
        assert_eq!(config_high.simulcast_layers, Some(99));
        assert_eq!(
            config_high.simulcast_layer_count(),
            3,
            "99 layers must clamp to SIMULCAST_MAX_LAYERS (3)"
        );

        // Zero clamps up to the base single layer (never 0 encoders).
        let args_zero = vec![
            "bot".to_string(),
            "--config".to_string(),
            path.display().to_string(),
            "--simulcast-layers".to_string(),
            "0".to_string(),
        ];
        let (config_zero, _) = BotConfig::from_args_inner(&args_zero, None).unwrap();
        assert_eq!(config_zero.simulcast_layers, Some(0));
        assert_eq!(
            config_zero.simulcast_layer_count(),
            1,
            "0 layers must clamp up to 1 (single stream), never 0 encoders"
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn help_text_mentions_simulcast_layers() {
        let help = help_text();
        assert!(help.contains("--simulcast-layers"));
        assert!(help.contains("Simulcast production"));
    }

    #[test]
    fn costume_memory_abort_when_frames_exceed_available_memory() {
        assert_eq!(
            evaluate_costume_memory(101, 100, false),
            CostumeMemoryDecision::AbortExceedsAvailable
        );
    }

    #[test]
    fn costume_memory_abort_strict_when_frames_exceed_threshold() {
        assert_eq!(
            evaluate_costume_memory(85, 100, true),
            CostumeMemoryDecision::AbortStrictThreshold
        );
    }

    #[test]
    fn costume_memory_warn_without_strict_flag() {
        assert_eq!(
            evaluate_costume_memory(85, 100, false),
            CostumeMemoryDecision::Warn
        );
    }

    #[test]
    fn costume_memory_ok_below_threshold() {
        assert_eq!(
            evaluate_costume_memory(50, 100, false),
            CostumeMemoryDecision::Ok
        );
    }
}
