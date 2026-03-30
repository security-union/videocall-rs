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

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct BotConfig {
    pub ramp_up_delay_ms: Option<u64>,
    pub server_url: String,
    pub insecure: Option<bool>,
    #[serde(default)]
    pub transport: Transport,
    /// HMAC-SHA256 secret for minting JWT tokens. When set, the bot connects
    /// via `/lobby?token=<jwt>`. When omitted, falls back to the deprecated
    /// `/lobby/{user_id}/{meeting_id}` path (requires FEATURE_MEETING_MANAGEMENT=false).
    pub jwt_secret: Option<String>,
    /// JWT token TTL in seconds (default: 3600 = 1 hour).
    pub token_ttl_secs: Option<u64>,
    pub clients: Vec<ClientConfig>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ClientConfig {
    pub user_id: String,
    pub meeting_id: String,
    pub enable_audio: bool,
    pub enable_video: bool,
    /// Path to a WAV file for audio. Falls back to "BundyBests2.wav" if omitted.
    pub audio_file: Option<String>,
    /// Directory containing video frame images (frame_00000.jpg, ...).
    /// Falls back to "." with the legacy output_120..124 pattern if omitted.
    pub image_dir: Option<String>,
}

impl BotConfig {
    pub fn from_file(path: &str) -> anyhow::Result<Self> {
        let content = fs::read_to_string(path)?;
        let config: BotConfig = serde_yaml::from_str(&content)?;
        Ok(config)
    }

    /// Load config from CLI args (`--config`/`-c`), then `BOT_CONFIG_PATH` env
    /// var, then fall back to environment variable defaults.
    pub fn from_args() -> anyhow::Result<Self> {
        let args: Vec<String> = std::env::args().collect();
        for i in 0..args.len() {
            if (args[i] == "--config" || args[i] == "-c") && i + 1 < args.len() {
                return Self::from_file(&args[i + 1]);
            }
        }
        Self::from_env_or_default()
    }

    pub fn from_env_or_default() -> anyhow::Result<Self> {
        // Try to load from config file first
        if let Ok(config_path) = std::env::var("BOT_CONFIG_PATH") {
            return Self::from_file(&config_path);
        }

        // Fallback to environment variables (backwards compatibility)
        let server_url = std::env::var("SERVER_URL")
            .unwrap_or_else(|_| "https://webtransport-us-east.webtransport.video".to_string());

        let n_clients = std::env::var("N_CLIENTS")
            .unwrap_or_else(|_| "1".to_string())
            .parse::<usize>()
            .unwrap_or(1);

        let default_meeting_id = std::env::var("ROOM").unwrap_or_else(|_| "test-room".to_string());

        let transport = match std::env::var("TRANSPORT")
            .unwrap_or_else(|_| "webtransport".to_string())
            .to_lowercase()
            .as_str()
        {
            "websocket" | "ws" => Transport::WebSocket,
            _ => Transport::WebTransport,
        };

        let jwt_secret = std::env::var("JWT_SECRET").ok();
        let token_ttl_secs = std::env::var("TOKEN_TTL_SECS")
            .ok()
            .and_then(|v| v.parse().ok());

        // Each client gets individual settings - audio, video, and meeting_id
        let mut clients = Vec::new();
        for i in 0..n_clients {
            // Check for per-client env vars first, then default to global/defaults
            let client_enable_audio = std::env::var(format!("CLIENT_{i}_ENABLE_AUDIO"))
                .unwrap_or_else(|_| "true".to_string())
                .parse::<bool>()
                .unwrap_or(true);
            let client_enable_video = std::env::var(format!("CLIENT_{i}_ENABLE_VIDEO"))
                .unwrap_or_else(|_| "true".to_string())
                .parse::<bool>()
                .unwrap_or(true);
            let client_meeting_id = std::env::var(format!("CLIENT_{i}_MEETING_ID"))
                .unwrap_or_else(|_| default_meeting_id.clone());

            let client_audio_file = std::env::var(format!("CLIENT_{i}_AUDIO_FILE")).ok();
            let client_image_dir = std::env::var(format!("CLIENT_{i}_IMAGE_DIR")).ok();

            clients.push(ClientConfig {
                user_id: format!("bot{i:03}"),
                meeting_id: client_meeting_id,
                enable_audio: client_enable_audio,
                enable_video: client_enable_video,
                audio_file: client_audio_file,
                image_dir: client_image_dir,
            });
        }

        // Check for insecure flag
        let insecure = std::env::var("INSECURE")
            .unwrap_or_else(|_| "false".to_string())
            .parse::<bool>()
            .unwrap_or(false);

        Ok(BotConfig {
            ramp_up_delay_ms: Some(1000),
            server_url,
            insecure: Some(insecure),
            transport,
            jwt_secret,
            token_ttl_secs,
            clients,
        })
    }

    pub fn server_url(&self) -> anyhow::Result<Url> {
        Url::parse(&self.server_url).map_err(|e| anyhow::anyhow!("Invalid server URL: {e}"))
    }

    pub fn token_ttl_secs(&self) -> u64 {
        self.token_ttl_secs.unwrap_or(3600)
    }
}
