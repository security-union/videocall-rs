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

use crate::constants::{BOT_CONFIG_PATH, CLIENT_ENV, INSECURE, N_CLIENTS, ROOM, SERVER_URL};
use serde::{Deserialize, Serialize};
use std::fs;
use url::Url;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct BotConfig {
    pub ramp_up_delay_ms: Option<u64>,
    pub server_url: String,
    pub insecure: Option<bool>, // Skip certificate verification
    pub clients: Vec<ClientConfig>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
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

    pub fn from_env_or_default() -> anyhow::Result<Self> {
        // Try to load from config file first
        if let Some(config_path) = BOT_CONFIG_PATH.as_deref() {
            return Self::from_file(config_path);
        }

        // Fallback to environment variables (backwards compatibility)
        let server_url = SERVER_URL.clone();

        let n_clients = *N_CLIENTS;

        let default_meeting_id = ROOM.clone();

        // Each client gets individual settings - audio, video, and meeting_id
        let mut clients = Vec::new();
        for i in 0..n_clients {
            // Check for per-client env vars first, then default to global/defaults
            let client_enable_audio = CLIENT_ENV
                .get(&format!("CLIENT_{i}_ENABLE_AUDIO"))
                .cloned()
                .unwrap_or_else(|| "true".to_string())
                .parse::<bool>()
                .unwrap_or(true);
            let client_enable_video = CLIENT_ENV
                .get(&format!("CLIENT_{i}_ENABLE_VIDEO"))
                .cloned()
                .unwrap_or_else(|| "true".to_string())
                .parse::<bool>()
                .unwrap_or(true);
            let client_meeting_id = CLIENT_ENV
                .get(&format!("CLIENT_{i}_MEETING_ID"))
                .cloned()
                .unwrap_or_else(|| default_meeting_id.clone());

            clients.push(ClientConfig {
                user_id: format!("bot{i:03}"), // Remove hyphen for server compatibility
                meeting_id: client_meeting_id,
                enable_audio: client_enable_audio,
                enable_video: client_enable_video,
            });
        }

        // Check for insecure flag
        let insecure = *INSECURE;

        Ok(BotConfig {
            ramp_up_delay_ms: Some(1000),
            server_url,
            insecure: Some(insecure),
            clients,
        })
    }

    pub fn server_url(&self) -> anyhow::Result<Url> {
        Url::parse(&self.server_url).map_err(|e| anyhow::anyhow!("Invalid server URL: {e}"))
    }
}
