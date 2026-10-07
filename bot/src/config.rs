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
use videocall_aq::constants::SIMULCAST_MAX_LAYERS;
use videocall_meeting_types::mint::{self, LobbyAuth, MintError};

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
#[derive(Debug, Default, Deserialize, Serialize, Clone)]
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
    /// Fraction of bots (0.0..=1.0) on WebTransport when both URLs are set. WT
    /// positions are interleaved over the run-wide roster ([`interleaved_wt`]),
    /// so any slice of N positions holds `floor(ratio * N)` or one more.
    pub wt_ratio: Option<f64>,
    /// HMAC secret used to mint each bot's room access token. Falls back to the
    /// `JWT_SECRET` environment variable.
    pub jwt_secret: Option<String>,
    pub token_ttl_secs: Option<u64>,
    /// Opt in to the deprecated unauthenticated `/lobby/{user_id}/{room}` join.
    /// Only consulted when no secret is configured; the relay rejects it
    /// whenever `FEATURE_MEETING_MANAGEMENT` is on. Env:
    /// `BOT_ALLOW_DEPRECATED_PATH=true`.
    #[serde(default)]
    pub allow_deprecated_path: Option<bool>,
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
    /// Number of manifest participants, in manifest order, that publish mic and
    /// camera. 0 (or omitted) means every manifest participant. Generated
    /// `bot-NNN` participants never publish audio; see [`build_roster`].
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
    /// Prefix prepended to every participant's wire user id as `<prefix>-<name>`,
    /// so load machines running the same manifest do not collide.
    ///
    /// CLI: `--id-prefix <p>`. Env: `BOT_ID_PREFIX=<p>`. Must match `[A-Za-z0-9_-]{1,40}`.
    #[serde(default)]
    pub id_prefix: Option<String>,
    /// Control-packet cadence preset: `browser` (default) or `legacy` (1 s for
    /// heartbeat, HEALTH and DIAGNOSTICS, active streams only).
    ///
    /// CLI: `--control-timing <browser|legacy>`.
    #[serde(default)]
    pub control_timing: Option<String>,
    /// `false` stops DIAGNOSTICS entirely: no reporter, no packets.
    ///
    /// CLI: `--diagnostics <on|off>`. Env: `BOT_DIAGNOSTICS=<on|off>`. Default on.
    #[serde(default)]
    pub diagnostics: Option<bool>,
    /// Override the heartbeat keepalive interval (ms). CLI: `--heartbeat-interval-ms`.
    #[serde(default)]
    pub heartbeat_interval_ms: Option<u64>,
    /// Override the HEALTH interval (ms). CLI: `--health-interval-ms`.
    #[serde(default)]
    pub health_interval_ms: Option<u64>,
    /// Override the DIAGNOSTICS interval (ms). CLI: `--diagnostics-interval-ms`.
    #[serde(default)]
    pub diagnostics_interval_ms: Option<u64>,
    /// Stop every client after this long (e.g. `90s`, `30m`, `2h`), measured
    /// from process start. Unset = run until Ctrl-C / SIGTERM.
    ///
    /// CLI: `--duration <d>`.
    #[serde(default)]
    pub duration: Option<String>,
    /// Run-wide number of presenters: broadcasters that send every 20 ms audio
    /// frame, silence included. Default [`DEFAULT_TALKERS`].
    ///
    /// CLI: `--talkers <N>`. Env: `BOT_TALKERS=<N>`.
    #[serde(default)]
    pub talkers: Option<usize>,
    /// Run-wide number of muted participants that keep the camera on. Default 0.
    ///
    /// CLI: `--cameras <N>`. Env: `BOT_CAMERAS=<N>`.
    #[serde(default)]
    pub cameras: Option<usize>,
    /// First run-wide roster position this process runs, so several load hosts
    /// can split one roster. Default 0.
    ///
    /// CLI: `--roster-offset <K>`. Env: `BOT_ROSTER_OFFSET=<K>`.
    #[serde(default)]
    pub roster_offset: Option<usize>,
    /// Participants in the whole run, across hosts; cameras are spread over it.
    /// Default: this process's last position.
    ///
    /// CLI: `--run-size <N>`. Env: `BOT_RUN_SIZE=<N>`.
    #[serde(default)]
    pub run_size: Option<usize>,
    /// Most video streams one bot sends DIAGNOSTICS for. Default
    /// [`DEFAULT_DIAG_VIDEO_TRACKERS`].
    ///
    /// CLI: `--diag-video-trackers <N>`. Env: `BOT_DIAG_VIDEO_TRACKERS=<N>`.
    #[serde(default)]
    pub diag_video_trackers: Option<usize>,
    /// CLI-only: write the bot's participant list (run-manifest fields, see
    /// `run_manifest.rs`) to this path at start and again at shutdown.
    #[serde(default, skip)]
    pub participants_out: Option<String>,
    /// CLI-only: node alias recorded as `placement.node` in that list.
    #[serde(default, skip)]
    pub placement_node: Option<String>,
}

/// Video tiles a default desktop browser shows, and so tracks DIAGNOSTICS for:
/// `DensityMode::Auto` (the default, `dioxus-ui/src/context.rs`) is "4 cols,
/// ~12 tiles" in `dioxus-ui/src/components/density.rs`. The browser's ceiling
/// is `CANVAS_LIMIT` (30).
pub const DEFAULT_DIAG_VIDEO_TRACKERS: usize = 12;

pub const DEFAULT_TALKERS: usize = 2;

/// Browser HEALTH cadence: `health_reporting_interval_ms: Some(5000)` in
/// `dioxus-ui/src/components/attendants.rs`.
pub const BROWSER_HEALTH_INTERVAL_MS: u64 = 5_000;
/// Browser DIAGNOSTICS cadence: `HEARTBEAT_PERIOD_MS` in
/// `videocall-client/src/diagnostics/diagnostics_manager.rs`.
pub const BROWSER_DIAGNOSTICS_INTERVAL_MS: u64 = 500;
/// Lower bound for any control interval, so a typo cannot flood the relay.
pub const MIN_CONTROL_INTERVAL_MS: u64 = 100;

/// Cadence of the periodic control packets every bot sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlTimings {
    pub heartbeat: std::time::Duration,
    pub health: std::time::Duration,
    pub diagnostics: std::time::Duration,
    /// Report DIAGNOSTICS for every (peer, media) seen until the peer is gone,
    /// as the browser does, rather than only for streams active this window.
    pub persistent_diagnostics: bool,
}

impl ControlTimings {
    pub fn browser() -> Self {
        use std::time::Duration;
        Self {
            heartbeat: Duration::from_millis(u64::from(
                videocall_aq::constants::HEARTBEAT_KEEPALIVE_INTERVAL_MS,
            )),
            health: Duration::from_millis(BROWSER_HEALTH_INTERVAL_MS),
            diagnostics: Duration::from_millis(BROWSER_DIAGNOSTICS_INTERVAL_MS),
            persistent_diagnostics: true,
        }
    }

    pub fn legacy() -> Self {
        use std::time::Duration;
        Self {
            heartbeat: Duration::from_secs(1),
            health: Duration::from_secs(1),
            diagnostics: Duration::from_secs(1),
            persistent_diagnostics: false,
        }
    }
}

/// Minimal client identity -- used only by the transport layer.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub user_id: String,
    pub meeting_id: String,
    pub enable_audio: bool,
    pub enable_video: bool,
    /// Heartbeat keepalive interval; a heartbeat is also sent when speaking changes.
    pub heartbeat_interval: std::time::Duration,
}

impl ClientConfig {
    /// The identity and media flags a participant with `role` announces.
    pub fn for_role(
        user_id: String,
        meeting_id: String,
        role: Role,
        heartbeat_interval: std::time::Duration,
    ) -> Self {
        Self {
            user_id,
            meeting_id,
            enable_audio: role.sends_audio(),
            enable_video: role.sends_video(),
            heartbeat_interval,
        }
    }
}

/// Reject a pin layer that is not on the simulcast ladder.
///
/// The relay accepts ids up to `LAYER_PREFERENCE_MAX_LAYER_ID` (7) while the ladder is
/// only [`SIMULCAST_MAX_LAYERS`] deep, so an off-ladder pin is RECORDED, every non-base
/// layer is dropped, and nothing on-ladder ever equals the pin — the bot then reports
/// `fps_received = 0` forever, so a typo reads as total starvation (#2206).
fn validate_pin_layer(layer: Option<u32>) -> anyhow::Result<()> {
    match layer {
        Some(l) if l as usize >= SIMULCAST_MAX_LAYERS => Err(anyhow!(
            "pin layer must be 0..{} (the simulcast ladder depth); got {}. \
             Set via --pin-layer, BOT_PIN_LAYER, or the config file.",
            SIMULCAST_MAX_LAYERS,
            l
        )),
        _ => Ok(()),
    }
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
        let mut id_prefix: Option<String> = None;
        let mut control_timing: Option<String> = None;
        let mut diagnostics: Option<bool> = None;
        let mut heartbeat_interval_ms: Option<u64> = None;
        let mut health_interval_ms: Option<u64> = None;
        let mut diagnostics_interval_ms: Option<u64> = None;
        let mut duration: Option<String> = None;
        let mut talkers: Option<usize> = None;
        let mut cameras: Option<usize> = None;
        let mut roster_offset: Option<usize> = None;
        let mut run_size: Option<usize> = None;
        let mut diag_video_trackers: Option<usize> = None;
        let mut broadcasters: Option<usize> = None;
        let mut participants_out: Option<String> = None;
        let mut placement_node: Option<String> = None;

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
                "--diagnostics" => {
                    diagnostics = Some(parse_on_off(args.get(i + 1).map(String::as_str))?);
                    i += 2;
                }
                "--control-timing" => {
                    if i + 1 < args.len() {
                        control_timing = Some(args[i + 1].clone());
                        i += 2;
                    } else {
                        return Err(anyhow!("--control-timing requires <browser|legacy>"));
                    }
                }
                "--heartbeat-interval-ms"
                | "--health-interval-ms"
                | "--diagnostics-interval-ms" => {
                    let flag = args[i].clone();
                    let value: u64 = args
                        .get(i + 1)
                        .ok_or_else(|| anyhow!("{flag} requires a millisecond value"))?
                        .parse()
                        .map_err(|_| anyhow!("{flag} requires a millisecond value"))?;
                    match flag.as_str() {
                        "--heartbeat-interval-ms" => heartbeat_interval_ms = Some(value),
                        "--health-interval-ms" => health_interval_ms = Some(value),
                        _ => diagnostics_interval_ms = Some(value),
                    }
                    i += 2;
                }
                "--participants-out" | "--placement-node" => {
                    let flag = args[i].clone();
                    let value = args
                        .get(i + 1)
                        .ok_or_else(|| anyhow!("{flag} requires a value"))?
                        .clone();
                    if flag == "--participants-out" {
                        participants_out = Some(value);
                    } else {
                        placement_node = Some(value);
                    }
                    i += 2;
                }
                flag @ ("--talkers"
                | "--broadcasters"
                | "--cameras"
                | "--roster-offset"
                | "--run-size"
                | "--diag-video-trackers") => {
                    let value: usize = args
                        .get(i + 1)
                        .and_then(|v| v.parse().ok())
                        .ok_or_else(|| anyhow!("{flag} requires a count"))?;
                    match flag {
                        "--talkers" => talkers = Some(value),
                        "--cameras" => cameras = Some(value),
                        "--broadcasters" => broadcasters = Some(value),
                        "--run-size" => run_size = Some(value),
                        "--diag-video-trackers" => diag_video_trackers = Some(value),
                        _ => roster_offset = Some(value),
                    }
                    i += 2;
                }
                "--duration" => {
                    if i + 1 < args.len() {
                        duration = Some(args[i + 1].clone());
                        i += 2;
                    } else {
                        return Err(anyhow!("--duration requires a value such as 30m"));
                    }
                }
                "--id-prefix" => {
                    if i + 1 < args.len() {
                        id_prefix = Some(args[i + 1].clone());
                        i += 2;
                    } else {
                        return Err(anyhow!("--id-prefix requires a prefix argument"));
                    }
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
                other => {
                    return Err(anyhow!("unknown argument '{}'\n\n{}", other, help_text()));
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
        config.participants_out = participants_out;
        config.placement_node = placement_node;

        // Participant id prefix. Precedence: CLI flag > env var > YAML file value.
        if let Some(p) = id_prefix {
            config.id_prefix = Some(p);
        } else if let Ok(env_p) = std::env::var("BOT_ID_PREFIX") {
            if !env_p.is_empty() {
                config.id_prefix = Some(env_p);
            }
        }
        validate_id_prefix(config.id_prefix.as_deref())?;

        if control_timing.is_some() {
            config.control_timing = control_timing;
        }
        if heartbeat_interval_ms.is_some() {
            config.heartbeat_interval_ms = heartbeat_interval_ms;
        }
        if health_interval_ms.is_some() {
            config.health_interval_ms = health_interval_ms;
        }
        if diagnostics_interval_ms.is_some() {
            config.diagnostics_interval_ms = diagnostics_interval_ms;
        }
        config.control_timings()?;

        if duration.is_some() {
            config.duration = duration;
        }
        for (cli, env, field) in [
            (talkers, "BOT_TALKERS", &mut config.talkers),
            (cameras, "BOT_CAMERAS", &mut config.cameras),
            (broadcasters, "BOT_BROADCASTERS", &mut config.broadcasters),
            (run_size, "BOT_RUN_SIZE", &mut config.run_size),
            (
                diag_video_trackers,
                "BOT_DIAG_VIDEO_TRACKERS",
                &mut config.diag_video_trackers,
            ),
            (
                roster_offset,
                "BOT_ROSTER_OFFSET",
                &mut config.roster_offset,
            ),
        ] {
            if cli.is_some() {
                *field = cli;
            } else if let Ok(v) = std::env::var(env) {
                *field = Some(v.parse().map_err(|_| anyhow!("{env} must be a count"))?);
            }
        }
        if diagnostics.is_some() {
            config.diagnostics = diagnostics;
        } else if let Ok(v) = std::env::var("BOT_DIAGNOSTICS") {
            config.diagnostics = Some(
                parse_on_off(Some(&v)).map_err(|_| anyhow!("BOT_DIAGNOSTICS must be on or off"))?,
            );
        }
        if config.roster_offset.unwrap_or(0) > 0 && config.run_size.is_none() {
            return Err(anyhow!(
                "--roster-offset needs --run-size (participants in the whole run) on every host"
            ));
        }
        config.run_duration()?;

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
        // Checked on the RESOLVED value, so ONE check covers all three sources (CLI,
        // BOT_PIN_LAYER, config file) rather than only the flag.
        validate_pin_layer(config.pin_layer)?;
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

        // Room-access-token credentials. Precedence: YAML file value > env var.
        if config.jwt_secret.is_none() {
            config.jwt_secret = std::env::var("JWT_SECRET").ok().filter(|s| !s.is_empty());
        }
        if config.allow_deprecated_path.is_none() {
            if let Ok(raw) = std::env::var("BOT_ALLOW_DEPRECATED_PATH") {
                config.allow_deprecated_path =
                    Some(matches!(raw.to_lowercase().as_str(), "true" | "1" | "yes"));
            }
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

    /// Name of the network profile a participant runs with, following the same
    /// precedence as [`Self::resolve_network`]: a preset name, `custom` for an
    /// inline manifest block, or `none`.
    pub fn network_label(&self, participant: &Participant) -> String {
        if self.no_impair {
            return "none".to_string();
        }
        if let Some(preset) = self.impair_name.get(&participant.name) {
            return preset.clone();
        }
        if let Some(net) = &participant.network {
            return match &net.profile {
                Some(name) => name.clone(),
                None => "custom".to_string(),
            };
        }
        self.impair_all
            .clone()
            .unwrap_or_else(|| "none".to_string())
    }

    /// Resolve the transport and server URL for a given bot index.
    ///
    /// New-style config: `ws_url` and/or `wt_url` with optional `wt_ratio`.
    /// Legacy config: single `server_url` + `transport` field.
    pub fn resolve_transport(&self, bot_index: usize) -> anyhow::Result<(Transport, Url)> {
        // New-style: ws_url / wt_url with ratio-based split
        if self.ws_url.is_some() || self.wt_url.is_some() {
            let ratio = self.wt_ratio.unwrap_or(0.0).clamp(0.0, 1.0);
            let use_wt = if self.wt_url.is_some() && self.ws_url.is_some() {
                interleaved_wt(bot_index, ratio)
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

    pub fn allow_deprecated_path(&self) -> bool {
        self.allow_deprecated_path.unwrap_or(false)
    }

    /// Resolve how this bot authenticates to the relay (#2298).
    ///
    /// A configured secret always wins over `allow_deprecated_path`, and a
    /// config with neither is an error rather than an unauthenticated join.
    pub fn resolve_lobby_auth(&self) -> Result<LobbyAuth, MintError> {
        mint::resolve_lobby_auth(
            None,
            self.jwt_secret.clone(),
            self.token_ttl_secs(),
            self.allow_deprecated_path(),
        )
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

    pub fn talker_count(&self) -> usize {
        self.talkers.unwrap_or(DEFAULT_TALKERS)
    }

    pub fn diagnostics_enabled(&self) -> bool {
        self.diagnostics.unwrap_or(true)
    }

    pub fn diag_video_tracker_cap(&self) -> usize {
        self.diag_video_trackers
            .unwrap_or(DEFAULT_DIAG_VIDEO_TRACKERS)
    }

    pub fn population(&self) -> Population {
        Population {
            broadcasters: self.broadcasters(),
            talkers: self.talker_count(),
            cameras: self.cameras.unwrap_or(0),
            run_size: self.run_size,
        }
    }

    pub fn run_duration(&self) -> anyhow::Result<Option<std::time::Duration>> {
        self.duration
            .as_deref()
            .map(crate::shutdown::parse_duration)
            .transpose()
    }

    /// Resolve the control-packet cadence: the preset, then per-packet overrides.
    pub fn control_timings(&self) -> anyhow::Result<ControlTimings> {
        use std::time::Duration;
        let mut t = match self.control_timing.as_deref().unwrap_or("browser") {
            "browser" => ControlTimings::browser(),
            "legacy" => ControlTimings::legacy(),
            other => {
                return Err(anyhow!(
                    "control timing must be 'browser' or 'legacy'; got '{}'",
                    other
                ))
            }
        };
        for (name, value, slot) in [
            ("heartbeat", self.heartbeat_interval_ms, &mut t.heartbeat),
            ("health", self.health_interval_ms, &mut t.health),
            (
                "diagnostics",
                self.diagnostics_interval_ms,
                &mut t.diagnostics,
            ),
        ] {
            if let Some(ms) = value {
                if ms < MIN_CONTROL_INTERVAL_MS {
                    return Err(anyhow!(
                        "{} interval must be >= {} ms; got {}",
                        name,
                        MIN_CONTROL_INTERVAL_MS,
                        ms
                    ));
                }
                *slot = Duration::from_millis(ms);
            }
        }
        Ok(t)
    }

    /// The user id a participant presents to the relay (the JWT `sub`).
    pub fn wire_user_id(&self, name: &str) -> String {
        wire_user_id(self.id_prefix.as_deref(), name)
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

/// Reject an id prefix the relay labels and run manifests cannot carry cleanly.
fn validate_id_prefix(prefix: Option<&str>) -> anyhow::Result<()> {
    match prefix {
        Some(p)
            if p.is_empty()
                || p.len() > 40
                || !p
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') =>
        {
            Err(anyhow!(
                "id prefix must match [A-Za-z0-9_-]{{1,40}}; got '{}'. \
                 Set via --id-prefix, BOT_ID_PREFIX, or the config file.",
                p
            ))
        }
        _ => Ok(()),
    }
}

/// `<prefix>-<name>`, or `name` when no prefix is configured.
pub fn wire_user_id(prefix: Option<&str>, name: &str) -> String {
    match prefix {
        Some(p) => format!("{p}-{name}"),
        None => name.to_string(),
    }
}

fn parse_on_off(value: Option<&str>) -> anyhow::Result<bool> {
    match value {
        Some("on") => Ok(true),
        Some("off") => Ok(false),
        _ => Err(anyhow!("--diagnostics requires on or off")),
    }
}

/// Whether run-wide position `i` uses WT: the WT positions are spread evenly,
/// so any slice of the roster gets about `ratio` of them.
pub fn interleaved_wt(i: usize, ratio: f64) -> bool {
    ((i + 1) as f64 * ratio).floor() > (i as f64 * ratio).floor()
}

/// Media loop used when there are no conversation lines.
pub const SILENT_LOOP: std::time::Duration = std::time::Duration::from_secs(10);

/// Length of the shared media loop. A slice with no conversation lines (only
/// cameras and viewers) still needs a non-zero loop for its video producers.
pub fn media_loop_duration(total_samples: usize) -> std::time::Duration {
    match total_samples as u64 * 1000 / 48000 {
        0 => SILENT_LOOP,
        ms => std::time::Duration::from_millis(ms),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Mic and camera on; sends every 20 ms audio frame, silence included.
    Presenter,
    /// Mic and camera on; sends audio frames only while its line is audible.
    Speaker,
    Camera,
    Viewer,
}

impl Role {
    pub fn sends_audio(self) -> bool {
        matches!(self, Role::Presenter | Role::Speaker)
    }

    pub fn sends_video(self) -> bool {
        self != Role::Viewer
    }

    pub fn is_talker(self) -> bool {
        self == Role::Presenter
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Population {
    /// Manifest participants (in order) with mic and camera on; 0 = all of them.
    pub broadcasters: usize,
    /// Broadcasters that are presenters (speakers with lines first).
    pub talkers: usize,
    /// Non-broadcasters with the camera on and the mic muted, spread evenly
    /// over the run so every host's slice gets its share.
    pub cameras: usize,
    /// Participants in the whole run; `None` = the last position built.
    pub run_size: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct RosterEntry {
    pub participant: Participant,
    pub role: Role,
}

pub fn generated_participant_name(position: usize) -> String {
    format!("bot-{:03}", position + 1)
}

/// Positions `offset .. offset + count` of the run-wide roster: the manifest
/// entries, then generated `bot-NNN` participants with no lines, costume or
/// network block. `count == 0` means "to the end of the manifest".
///
/// Roles depend only on the run-wide position, so hosts that split one roster
/// with different offsets assign exactly the roles one process would.
pub fn build_roster(
    manifest: &[Participant],
    speakers: &std::collections::HashSet<&str>,
    offset: usize,
    count: usize,
    population: &Population,
) -> Vec<RosterEntry> {
    let end = if count == 0 {
        manifest.len().max(offset)
    } else {
        offset + count
    };
    let broadcasters = match population.broadcasters {
        0 => manifest.len(),
        b => b.min(manifest.len()),
    };
    let muted = population
        .run_size
        .unwrap_or(end)
        .max(end)
        .saturating_sub(broadcasters);
    let is_camera = |m: usize| {
        population.cameras >= muted
            || (m + 1) * population.cameras / muted > m * population.cameras / muted
    };
    let talkers: std::collections::HashSet<usize> = (0..broadcasters)
        .filter(|&i| speakers.contains(manifest[i].name.as_str()))
        .chain((0..broadcasters).filter(|&i| !speakers.contains(manifest[i].name.as_str())))
        .take(population.talkers)
        .collect();
    (offset..end)
        .map(|position| {
            let participant = manifest.get(position).cloned().unwrap_or_else(|| {
                let mut name = generated_participant_name(position);
                if manifest.iter().any(|p| p.name == name) {
                    name.push_str("-g");
                }
                Participant {
                    name,
                    voice: "none".to_string(),
                    ekg_color: default_ekg_color(),
                    costume_dir: None,
                    network: None,
                }
            });
            let role = if talkers.contains(&position) {
                Role::Presenter
            } else if position < broadcasters {
                Role::Speaker
            } else if is_camera(position - broadcasters) {
                Role::Camera
            } else {
                Role::Viewer
            };
            RosterEntry { participant, role }
        })
        .collect()
}

/// A random RFC 4122 version-4 UUID string, the shape the browser client
/// sends as `instance_id`.
pub fn generate_instance_id<R: rand::Rng>(rng: &mut R) -> String {
    let mut b: [u8; 16] = rng.gen();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13],
        b[14], b[15]
    )
}

/// Append `instance_id=<id>` to a lobby URL, as the browser does
/// (`videocall-client/src/connection/connection_manager.rs` `build_connect_url`).
pub fn append_instance_id(mut url: Url, instance_id: &str) -> Url {
    url.query_pairs_mut()
        .append_pair("instance_id", instance_id);
    url
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
         \x20 --participants-out <path>     Write the participant list (run-manifest fields) as JSON\n\
         \x20                               at start and at shutdown.\n\
         \x20 --placement-node <alias>      Node alias recorded as placement.node in that list.\n\
         \x20 --talkers <N>                 Presenters: broadcasters that send audio continuously,\n\
         \x20                               silence included (default 2; speakers first). Run-wide.\n\
         \x20 --broadcasters <N>            Manifest participants with mic and camera on (default all).\n\
         \x20 --run-size <N>                Participants in the whole run across hosts (cameras are\n\
         \x20                               spread over it; default: this process's last position).\n\
         \x20 --diagnostics <on|off>        off sends no DIAGNOSTICS at all (default on). BOT_DIAGNOSTICS.\n\
         \x20 --diag-video-trackers <N>     Most video streams one bot sends DIAGNOSTICS for (default\n\
         \x20                               12, a default desktop browser; 30 = its maximum).\n\
         \x20 --cameras <N>                 Muted participants with the camera on (default 0). Run-wide.\n\
         \x20 --roster-offset <K>           First run-wide roster position this process runs (default\n\
         \x20                               0), so hosts can split one roster. Generated participants\n\
         \x20                               (bot-NNN) never send audio.\n\
         \x20 --duration <d>                Stop after d (90s, 30m, 2h) from process start; also stops\n\
         \x20                               on Ctrl-C or SIGTERM. Unknown arguments are rejected.\n\
         \x20 --users, -n <N>               Number of participants (0 = all in manifest). Values\n\
         \x20                               above the manifest length add bot-NNN participants.\n\
         \x20 --id-prefix <p>               Prefix every user id as <p>-<name> (unique across load\n\
         \x20                               machines). Also via BOT_ID_PREFIX.\n\
         \n\
         Control-packet cadence (default: browser = heartbeat 5s + on speaking change,\n\
         HEALTH 5s, DIAGNOSTICS 500ms for every tracked stream):\n\
         \x20 --control-timing <browser|legacy>  legacy = 1s for all three, active streams only.\n\
         \x20 --heartbeat-interval-ms <ms>  Override the heartbeat keepalive.\n\
         \x20 --health-interval-ms <ms>     Override the HEALTH interval.\n\
         \x20 --diagnostics-interval-ms <ms> Override the DIAGNOSTICS interval.\n\
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
    use super::{
        append_instance_id, build_roster, evaluate_costume_memory, generate_instance_id, help_text,
        validate_id_prefix, validate_pin_layer, wire_user_id, BotConfig, CostumeMemoryDecision,
        Participant, Population, Role, RosterEntry,
    };
    use std::collections::HashSet;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};
    use videocall_aq::constants::SIMULCAST_MAX_LAYERS;

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

    /// `write_temp_config` plus extra YAML lines, for exercising file-sourced knobs.
    fn write_temp_config_with(extra: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("bot-config-extra-{unique}.yaml"));
        fs::write(
            &path,
            format!("meeting_id: test-room\nws_url: wss://example.invalid/lobby\n{extra}"),
        )
        .unwrap();
        path
    }

    fn named(name: &str) -> Participant {
        Participant {
            name: name.to_string(),
            voice: "v".to_string(),
            ekg_color: [1, 2, 3],
            costume_dir: None,
            network: None,
        }
    }

    fn args_with(extra: &[&str]) -> (Vec<String>, PathBuf) {
        let path = write_temp_config();
        let mut args = vec![
            "bot".to_string(),
            "--config".to_string(),
            path.display().to_string(),
        ];
        args.extend(extra.iter().map(|s| s.to_string()));
        (args, path)
    }

    fn roles(roster: &[RosterEntry]) -> Vec<Role> {
        roster.iter().map(|e| e.role).collect()
    }

    fn pop(broadcasters: usize, talkers: usize, cameras: usize) -> Population {
        Population {
            broadcasters,
            talkers,
            cameras,
            run_size: None,
        }
    }

    #[test]
    fn talkers_prefer_speakers_then_fill_from_other_broadcasters() {
        let manifest = vec![named("alice"), named("bob"), named("carol"), named("dave")];
        let speakers: HashSet<&str> = ["bob", "dave"].iter().copied().collect();
        let roster = build_roster(&manifest, &speakers, 0, 0, &pop(0, 3, 0));
        use Role::{Presenter, Speaker};
        assert_eq!(
            roles(&roster),
            vec![Presenter, Presenter, Speaker, Presenter]
        );
        let one = build_roster(&manifest, &speakers, 0, 0, &pop(1, 4, 0));
        assert_eq!(
            roles(&one)[0],
            Presenter,
            "talkers never exceed broadcasters"
        );
        assert!(!roles(&one)[1..].contains(&Presenter));
    }

    #[test]
    fn generated_participants_never_publish_audio_by_default() {
        let manifest = vec![named("alice"), named("bob")];
        let speakers: HashSet<&str> = ["alice", "bob"].iter().copied().collect();
        let roster = build_roster(
            &manifest,
            &speakers,
            0,
            6,
            &BotConfig::default().population(),
        );
        use Role::{Camera, Presenter, Viewer};
        assert_eq!(
            roles(&roster),
            vec![Presenter, Presenter, Viewer, Viewer, Viewer, Viewer]
        );
        let with_cameras = build_roster(&manifest, &speakers, 0, 6, &pop(1, 1, 2));
        assert_eq!(
            roles(&with_cameras),
            vec![Presenter, Viewer, Viewer, Camera, Viewer, Camera],
            "cameras are spread over the non-broadcasters, mic muted"
        );
        assert!(roster[2..].iter().all(|e| !e.role.sends_audio()));
    }

    #[test]
    fn roles_and_names_are_run_wide_across_split_hosts() {
        let manifest = vec![named("alice"), named("bob"), named("carol")];
        let speakers: HashSet<&str> = ["carol"].iter().copied().collect();
        let p = pop(0, 2, 1);
        let whole = build_roster(&manifest, &speakers, 0, 8, &p);
        let mut split = build_roster(&manifest, &speakers, 0, 3, &p);
        split.extend(build_roster(&manifest, &speakers, 3, 5, &p));
        assert_eq!(roles(&split), roles(&whole));
        let names: Vec<&str> = split.iter().map(|e| e.participant.name.as_str()).collect();
        assert_eq!(names[3..5], ["bot-004", "bot-005"]);
        let second_host = build_roster(&manifest, &speakers, 3, 5, &p);
        assert!(
            second_host.iter().all(|e| !e.role.is_talker()),
            "a host past the manifest runs no talkers"
        );
    }

    #[test]
    fn a_split_run_requires_the_run_size() {
        let (args, path) = args_with(&["--roster-offset", "58"]);
        let err = BotConfig::from_args_inner(&args, None).unwrap_err();
        assert!(err.to_string().contains("--run-size"), "{}", err);
        let (args, path2) = args_with(&["--roster-offset", "58", "--run-size", "170"]);
        assert!(BotConfig::from_args_inner(&args, None).is_ok());
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(path2);
    }

    #[test]
    fn cameras_spread_across_host_slices() {
        let manifest = vec![named("alice"), named("bob")];
        let speakers = HashSet::new();
        let mut p = pop(0, 1, 6);
        p.run_size = Some(62);
        let cameras = |offset| {
            build_roster(&manifest, &speakers, offset, 20, &p)
                .iter()
                .filter(|e| e.role == Role::Camera)
                .count()
        };
        assert_eq!([cameras(2), cameras(22), cameras(42)], [2, 2, 2]);
    }

    #[test]
    fn a_slice_without_lines_still_has_a_media_loop() {
        assert!(super::media_loop_duration(0) > std::time::Duration::ZERO);
        assert_eq!(
            super::media_loop_duration(96_000),
            std::time::Duration::from_secs(2)
        );
    }

    #[test]
    fn webtransport_positions_interleave_rather_than_lead() {
        let wt: Vec<bool> = (0..6).map(|i| super::interleaved_wt(i, 0.5)).collect();
        assert_eq!(wt, [false, true, false, true, false, true]);
        assert_eq!(
            (0..100).filter(|&i| super::interleaved_wt(i, 0.3)).count(),
            30
        );
        assert!((0..10).all(|i| !super::interleaved_wt(i, 0.0)));
        assert!((0..10).all(|i| super::interleaved_wt(i, 1.0)));
    }

    #[test]
    fn talker_count_defaults_to_two_and_parses_from_cli() {
        assert_eq!(BotConfig::default().talker_count(), 2);
        let (args, path) = args_with(&[
            "--talkers",
            "3",
            "--cameras",
            "5",
            "--roster-offset",
            "40",
            "--run-size",
            "170",
            "--broadcasters",
            "6",
        ]);
        let (config, _) = BotConfig::from_args_inner(&args, None).unwrap();
        assert_eq!(config.talker_count(), 3);
        assert_eq!(config.population().cameras, 5);
        assert_eq!(config.roster_offset, Some(40));
        assert_eq!(config.population().run_size, Some(170));
        assert_eq!(config.population().broadcasters, 6);
        assert_eq!(
            config.diag_video_tracker_cap(),
            12,
            "default desktop browser"
        );
        assert!(config.diagnostics_enabled(), "DIAGNOSTICS default on");
        let (args, path3) = args_with(&["--diagnostics", "off"]);
        assert!(!BotConfig::from_args_inner(&args, None)
            .unwrap()
            .0
            .diagnostics_enabled());
        let (args, path4) = args_with(&["--diagnostics", "maybe"]);
        assert!(BotConfig::from_args_inner(&args, None).is_err());
        let _ = fs::remove_file(path3);
        let _ = fs::remove_file(path4);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn network_label_follows_the_impairment_precedence() {
        let mut inline = named("inline");
        inline.network = Some(super::ParticipantNetwork {
            latency_ms: Some(40),
            ..Default::default()
        });
        let mut preset = named("preset");
        preset.network = Some(super::ParticipantNetwork {
            profile: Some("good_4g".into()),
            ..Default::default()
        });
        let mut config = BotConfig {
            impair_all: Some("lossy_mobile".into()),
            ..Default::default()
        };
        config
            .impair_name
            .insert("override".into(), "satellite".into());
        assert_eq!(config.network_label(&named("plain")), "lossy_mobile");
        assert_eq!(config.network_label(&inline), "custom");
        assert_eq!(config.network_label(&preset), "good_4g");
        assert_eq!(config.network_label(&named("override")), "satellite");
        config.no_impair = true;
        assert_eq!(config.network_label(&preset), "none");
    }

    #[test]
    fn unknown_arguments_are_rejected() {
        let (args, path) = args_with(&["--pin-layers", "0"]);
        let err = BotConfig::from_args_inner(&args, None).unwrap_err();
        assert!(err.to_string().contains("unknown argument '--pin-layers'"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn duration_parses_and_invalid_duration_fails_at_startup() {
        let (args, path) = args_with(&["--duration", "30m"]);
        let (config, _) = BotConfig::from_args_inner(&args, None).unwrap();
        assert_eq!(
            config.run_duration().unwrap(),
            Some(std::time::Duration::from_secs(1800))
        );
        let _ = fs::remove_file(path);
        let (args, path) = args_with(&["--duration", "soon"]);
        assert!(BotConfig::from_args_inner(&args, None).is_err());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn control_timings_default_to_the_browser_cadence() {
        let t = BotConfig::default().control_timings().unwrap();
        assert_eq!(
            t.heartbeat.as_millis() as u64,
            u64::from(videocall_aq::constants::HEARTBEAT_KEEPALIVE_INTERVAL_MS)
        );
        assert_eq!(t.health.as_millis(), 5_000);
        assert_eq!(t.diagnostics.as_millis(), 500);
        assert!(t.persistent_diagnostics);
    }

    #[test]
    fn legacy_timing_and_overrides_resolve() {
        let path = write_temp_config();
        let args: Vec<String> = [
            "bot",
            "--config",
            path.to_str().unwrap(),
            "--control-timing",
            "legacy",
            "--health-interval-ms",
            "2500",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let (config, _) = BotConfig::from_args_inner(&args, None).unwrap();
        let t = config.control_timings().unwrap();
        assert_eq!(t.heartbeat.as_millis(), 1_000);
        assert_eq!(t.health.as_millis(), 2_500);
        assert_eq!(t.diagnostics.as_millis(), 1_000);
        assert!(!t.persistent_diagnostics);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn invalid_control_timing_is_rejected() {
        let bad_mode = BotConfig {
            control_timing: Some("fast".into()),
            ..Default::default()
        };
        assert!(bad_mode.control_timings().is_err());
        let too_fast = BotConfig {
            diagnostics_interval_ms: Some(10),
            ..Default::default()
        };
        assert!(too_fast.control_timings().is_err());
    }

    #[test]
    fn roster_grows_past_the_manifest_length() {
        let manifest = vec![named("alice"), named("bob")];
        let roster = build_roster(&manifest, &HashSet::new(), 0, 5, &pop(0, 0, 0));
        let names: Vec<&str> = roster.iter().map(|e| e.participant.name.as_str()).collect();
        assert_eq!(names, vec!["alice", "bob", "bot-003", "bot-004", "bot-005"]);
        assert!(roster[2].participant.network.is_none());
    }

    #[test]
    fn roster_truncates_and_zero_means_whole_manifest() {
        let manifest = vec![named("alice"), named("bob"), named("carol")];
        let none = HashSet::new();
        assert_eq!(build_roster(&manifest, &none, 0, 2, &pop(0, 0, 0)).len(), 2);
        assert_eq!(build_roster(&manifest, &none, 0, 0, &pop(0, 0, 0)).len(), 3);
    }

    #[test]
    fn roster_never_duplicates_a_manifest_name() {
        let manifest = vec![named("alice"), named("bot-003")];
        let roster = build_roster(&manifest, &HashSet::new(), 0, 4, &pop(0, 0, 0));
        let mut names: Vec<&str> = roster.iter().map(|e| e.participant.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 4);
    }

    #[test]
    fn wire_user_id_applies_the_prefix() {
        assert_eq!(wire_user_id(Some("hostA"), "alice"), "hostA-alice");
        assert_eq!(wire_user_id(None, "alice"), "alice");
    }

    #[test]
    fn id_prefix_validation_rejects_label_hostile_values() {
        assert!(validate_id_prefix(Some("r42-shard_1")).is_ok());
        assert!(validate_id_prefix(None).is_ok());
        assert!(validate_id_prefix(Some("")).is_err());
        assert!(validate_id_prefix(Some("a.b")).is_err());
        assert!(validate_id_prefix(Some(&"x".repeat(41))).is_err());
    }

    #[test]
    fn id_prefix_parses_from_cli() {
        let path = write_temp_config();
        let args = vec![
            "bot".to_string(),
            "--config".to_string(),
            path.display().to_string(),
            "--id-prefix".to_string(),
            "shard2".to_string(),
        ];
        let (config, _) = BotConfig::from_args_inner(&args, None).unwrap();
        assert_eq!(config.wire_user_id("alice"), "shard2-alice");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn instance_id_is_a_v4_uuid_and_unique() {
        use rand::SeedableRng;
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        let a = generate_instance_id(&mut rng);
        let b = generate_instance_id(&mut rng);
        assert_ne!(a, b);
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('4'));
        assert!(matches!(
            parts[3].chars().next(),
            Some('8' | '9' | 'a' | 'b')
        ));
    }

    #[test]
    fn instance_id_is_appended_to_the_lobby_query() {
        let url = url::Url::parse("wss://relay.example.com/lobby?token=abc").unwrap();
        let url = append_instance_id(url, "1234");
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert!(pairs.contains(&("token".to_string(), "abc".to_string())));
        assert!(pairs.contains(&("instance_id".to_string(), "1234".to_string())));
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
    fn pin_layer_rejects_a_rung_off_the_ladder() {
        // The bound itself. `validate_pin_layer` is called on the RESOLVED value, so this
        // one check governs all three sources (CLI, BOT_PIN_LAYER, config file) — which is
        // why the integration cases below only need to prove the wiring, and why no test
        // has to mutate process-global env.
        for bad in [SIMULCAST_MAX_LAYERS as u32, 7, 99] {
            let err = validate_pin_layer(Some(bad))
                .expect_err("an off-ladder pin must be rejected")
                .to_string();
            assert!(
                err.contains("simulcast ladder depth"),
                "the error must name the real bound; got {}",
                err
            );
        }
        for good in 0..SIMULCAST_MAX_LAYERS as u32 {
            assert!(validate_pin_layer(Some(good)).is_ok());
        }
        assert!(validate_pin_layer(None).is_ok(), "unset must stay valid");

        // Wiring, CLI source.
        let path = write_temp_config();
        let mut args = vec![
            "bot".to_string(),
            "--config".to_string(),
            path.display().to_string(),
            "--pin-layer".to_string(),
            "7".to_string(),
        ];
        assert!(
            BotConfig::from_args_inner(&args, None).is_err(),
            "the CLI flag must reach the check"
        );
        args.truncate(3);
        args.push("--pin-layer".to_string());
        args.push("1".to_string());
        let (config, _) = BotConfig::from_args_inner(&args, None).unwrap();
        assert_eq!(config.pin_layer, Some(1));
        let _ = fs::remove_file(path);

        // Wiring, config-file source — the path a CLI-only check silently missed.
        let file_path = write_temp_config_with("pin_layer: 7\n");
        let file_args = vec![
            "bot".to_string(),
            "--config".to_string(),
            file_path.display().to_string(),
        ];
        assert!(
            BotConfig::from_args_inner(&file_args, None).is_err(),
            "a config-file pin must reach the check too"
        );
        let _ = fs::remove_file(file_path);
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
