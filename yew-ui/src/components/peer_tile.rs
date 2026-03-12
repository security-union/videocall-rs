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

use crate::components::canvas_generator::generate_for_peer;
use crate::context::VideoCallClientCtx;
use futures::future::{AbortHandle, Abortable};
use gloo_timers::callback::Timeout;
use videocall_client::audio_constants::UI_AUDIO_LEVEL_DELTA;
use videocall_diagnostics::{subscribe, DiagEvent, MetricValue};
use yew::prelude::*;

#[derive(Properties, Debug, PartialEq, Clone)]
pub struct PeerTileProps {
    pub peer_id: String,
    #[prop_or(false)]
    pub full_bleed: bool,
    /// Authenticated user_id of the meeting host (for displaying crown icon).
    /// Compared against each peer's user_id to prevent display-name spoofing.
    #[prop_or_default]
    pub host_user_id: Option<String>,
}

pub enum Msg {
    Diagnostics(DiagEvent),
    /// Fired by the 1-second hold timer to clear the mic icon back to silent.
    MicHoldExpired,
}

pub struct PeerTile {
    client: videocall_client::VideoCallClient,
    audio_enabled: bool,
    video_enabled: bool,
    screen_enabled: bool,
    audio_level: f32,
    /// Separate level for the mic icon — held positive for 1 s after silence.
    mic_audio_level: f32,
    /// Pending timeout that will clear `mic_audio_level` to 0.
    mic_hold_timeout: Option<Timeout>,
    abort_handle: Option<AbortHandle>,
}

impl Component for PeerTile {
    type Message = Msg;
    type Properties = PeerTileProps;

    fn create(ctx: &Context<Self>) -> Self {
        let (client, _) = ctx
            .link()
            .context::<VideoCallClientCtx>(Callback::noop())
            .expect("VideoCallClient context missing");

        Self {
            client,
            audio_enabled: false,
            video_enabled: false,
            screen_enabled: false,
            audio_level: 0.0,
            mic_audio_level: 0.0,
            mic_hold_timeout: None,
            abort_handle: None,
        }
    }

    fn rendered(&mut self, ctx: &Context<Self>, first_render: bool) {
        if first_render {
            self.audio_enabled = self.client.is_audio_enabled_for_peer(&ctx.props().peer_id);
            self.video_enabled = self.client.is_video_enabled_for_peer(&ctx.props().peer_id);
            self.screen_enabled = self
                .client
                .is_screen_share_enabled_for_peer(&ctx.props().peer_id);
            self.audio_level = self.client.audio_level_for_peer(&ctx.props().peer_id);
            self.mic_audio_level = self.audio_level;

            let link = ctx.link().clone();
            let (abort_handle, abort_reg) = AbortHandle::new_pair();
            let fut = async move {
                let mut rx = subscribe();
                while let Ok(evt) = rx.recv().await {
                    link.send_message(Msg::Diagnostics(evt));
                }
            };
            let abortable = Abortable::new(fut, abort_reg);
            self.abort_handle = Some(abort_handle);
            wasm_bindgen_futures::spawn_local(async move {
                let _ = abortable.await;
            });
        }
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Self::Message) -> bool {
        match msg {
            Msg::MicHoldExpired => {
                self.mic_hold_timeout = None;
                if self.mic_audio_level != 0.0 {
                    self.mic_audio_level = 0.0;
                    return true;
                }
                false
            }
            Msg::Diagnostics(evt) => {
                match evt.subsystem {
                    "peer_status" => {
                        // Parse peer_status metrics
                        let mut to_peer: Option<String> = None;
                        let mut audio_enabled: Option<bool> = None;
                        let mut video_enabled: Option<bool> = None;
                        let mut screen_enabled: Option<bool> = None;
                        let mut audio_lvl: Option<f32> = None;
                        let mut speaking: Option<bool> = None;
                        for m in &evt.metrics {
                            match (m.name, &m.value) {
                                ("to_peer", MetricValue::Text(p)) => to_peer = Some(p.clone()),
                                ("audio_enabled", MetricValue::U64(v)) => {
                                    audio_enabled = Some(*v != 0)
                                }
                                ("video_enabled", MetricValue::U64(v)) => {
                                    video_enabled = Some(*v != 0)
                                }
                                ("screen_enabled", MetricValue::U64(v)) => {
                                    screen_enabled = Some(*v != 0)
                                }
                                ("audio_level", MetricValue::F64(v)) => audio_lvl = Some(*v as f32),
                                ("is_speaking", MetricValue::U64(v)) => speaking = Some(*v != 0),
                                _ => {}
                            }
                        }

                        if to_peer.as_deref() != Some(ctx.props().peer_id.as_str()) {
                            return false;
                        }

                        let mut changed = false;
                        if let Some(a) = audio_enabled {
                            if a != self.audio_enabled {
                                self.audio_enabled = a;
                                changed = true;
                            }
                        }
                        if let Some(v) = video_enabled {
                            if v != self.video_enabled {
                                self.video_enabled = v;
                                changed = true;
                            }
                        }
                        if let Some(s) = screen_enabled {
                            if s != self.screen_enabled {
                                self.screen_enabled = s;
                                changed = true;
                            }
                        }
                        // Prefer the float audio_level; fall back to boolean
                        let resolved_level = if let Some(lvl) = audio_lvl {
                            Some(lvl)
                        } else {
                            speaking.map(|s| if s { 1.0 } else { 0.0 })
                        };
                        if let Some(lvl) = resolved_level {
                            if (lvl == 0.0 && self.audio_level != 0.0)
                                || (lvl - self.audio_level).abs() > UI_AUDIO_LEVEL_DELTA
                            {
                                self.audio_level = lvl;
                                changed = true;
                            }
                            changed |= self.update_mic_audio_level(ctx, lvl);
                        }
                        changed
                    }
                    "peer_speaking" => {
                        // Fast-path speaking updates from decoded audio frames
                        let mut to_peer: Option<String> = None;
                        let mut audio_lvl: Option<f32> = None;
                        let mut speaking: Option<bool> = None;
                        for m in &evt.metrics {
                            match (m.name, &m.value) {
                                ("to_peer", MetricValue::Text(p)) => to_peer = Some(p.clone()),
                                ("audio_level", MetricValue::F64(v)) => audio_lvl = Some(*v as f32),
                                ("speaking", MetricValue::U64(v)) => speaking = Some(*v != 0),
                                _ => {}
                            }
                        }

                        if to_peer.as_deref() != Some(ctx.props().peer_id.as_str()) {
                            return false;
                        }

                        let resolved_level = if let Some(lvl) = audio_lvl {
                            Some(lvl)
                        } else {
                            speaking.map(|s| if s { 1.0 } else { 0.0 })
                        };
                        if let Some(lvl) = resolved_level {
                            let mut changed = false;
                            if (lvl == 0.0 && self.audio_level != 0.0)
                                || (lvl - self.audio_level).abs() > UI_AUDIO_LEVEL_DELTA
                            {
                                self.audio_level = lvl;
                                changed = true;
                            }
                            changed |= self.update_mic_audio_level(ctx, lvl);
                            return changed;
                        }
                        false
                    }
                    _ => false,
                }
            }
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        // Get host user_id from props for crown icon comparison
        let host_user_id = ctx.props().host_user_id.as_deref();

        // Delegate rendering to the existing canvas generator so DOM structure and CSS remain consistent
        generate_for_peer(
            &self.client,
            &ctx.props().peer_id,
            ctx.props().full_bleed,
            self.audio_level,
            self.mic_audio_level,
            host_user_id,
        )
    }

    fn destroy(&mut self, _ctx: &Context<Self>) {
        if let Some(handle) = self.abort_handle.take() {
            handle.abort();
        }
    }
}

impl PeerTile {
    /// Update `mic_audio_level` with a 1-second hold: when audio drops to zero
    /// the mic signal keeps its last positive value for 1 s so the icon stays
    /// green. Returns true if `mic_audio_level` was changed (needs re-render).
    fn update_mic_audio_level(&mut self, ctx: &Context<Self>, level: f32) -> bool {
        if level > 0.0 {
            // Cancel any pending silence timeout — speaker is still active.
            self.mic_hold_timeout = None;
            if (level - self.mic_audio_level).abs() > UI_AUDIO_LEVEL_DELTA {
                self.mic_audio_level = level;
                return true;
            }
            false
        } else {
            // Audio dropped to zero.
            if self.mic_audio_level == 0.0 {
                return false;
            }
            if self.mic_hold_timeout.is_some() {
                // A timeout is already queued — let it fire.
                return false;
            }
            let link = ctx.link().clone();
            let timeout = Timeout::new(1_000, move || {
                link.send_message(Msg::MicHoldExpired);
            });
            self.mic_hold_timeout = Some(timeout);
            false
        }
    }
}
