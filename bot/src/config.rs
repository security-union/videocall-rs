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
use std::fs;
use url::Url;

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
    /// Usage: `bot --config <file> --users <N>`
    ///
    /// `--users 0` or omitting it means "all participants from manifest".
    pub fn from_args() -> anyhow::Result<(Self, usize)> {
        let args: Vec<String> = std::env::args().collect();
        let mut config_path: Option<String> = None;
        let mut num_users: usize = 0;

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
                _ => {
                    i += 1;
                }
            }
        }

        let config = match config_path {
            Some(p) => Self::from_file(&p)?,
            None => {
                // Try BOT_CONFIG_PATH env var
                if let Ok(env_path) = std::env::var("BOT_CONFIG_PATH") {
                    Self::from_file(&env_path)?
                } else {
                    return Err(anyhow!(
                        "Usage: bot --config <file> [--users <N>]\n\
                         Or set BOT_CONFIG_PATH environment variable."
                    ));
                }
            }
        };

        Ok((config, num_users))
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
            let ratio = self.wt_ratio.unwrap_or(1.0).clamp(0.0, 1.0);
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
        self.token_ttl_secs.unwrap_or(3600)
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
pub struct Participant {
    pub name: String,
    #[allow(dead_code)]
    pub voice: String,
    #[serde(default = "default_ekg_color")]
    pub ekg_color: [u8; 3],
    /// Path to costume sprite sheet directory (for VideoMode::Costume).
    pub costume_dir: Option<String>,
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
