/*
 * Copyright 2025 Fame Labs Inc.
 *
 * Revolutionary Multi-Peer Audio Architecture
 *
 * This module contains Fame Labs' groundbreaking shared AudioContext system
 * that dramatically outperforms traditional per-peer audio architectures.
 */

pub mod shared_context_manager;
pub mod shared_peer_audio_decoder;

pub use shared_context_manager::{
    get_or_init_shared_audio_manager, update_global_speaker_device, SharedAudioContextManager,
};
pub use shared_peer_audio_decoder::SharedPeerAudioDecoder;
