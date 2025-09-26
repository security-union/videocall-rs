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
use futures::future::{AbortHandle, Abortable};
use videocall_client::VideoCallClient;
use videocall_diagnostics::{subscribe, DiagEvent, MetricValue};
use yew::prelude::*;

#[derive(Properties, Debug, PartialEq, Clone)]
pub struct PeerTileProps {
    pub peer_id: String,
    pub client: VideoCallClient,
    /// True when layout has only this peer and no screen share; affects styling
    #[prop_or(false)]
    pub full_bleed: bool,
}

pub enum Msg {
    Diagnostics(DiagEvent),
}

pub struct PeerTile {
    audio_enabled: bool,
    video_enabled: bool,
    screen_enabled: bool,
    abort_handle: Option<AbortHandle>,
}

impl Component for PeerTile {
    type Message = Msg;
    type Properties = PeerTileProps;

    fn create(_ctx: &Context<Self>) -> Self {
        Self {
            audio_enabled: false,
            video_enabled: false,
            screen_enabled: false,
            abort_handle: None,
        }
    }

    fn rendered(&mut self, ctx: &Context<Self>, first_render: bool) {
        if first_render {
            // Initialize from client snapshot to avoid waiting for first diagnostic
            self.audio_enabled = ctx
                .props()
                .client
                .is_audio_enabled_for_peer(&ctx.props().peer_id);
            self.video_enabled = ctx
                .props()
                .client
                .is_video_enabled_for_peer(&ctx.props().peer_id);
            self.screen_enabled = ctx
                .props()
                .client
                .is_screen_share_enabled_for_peer(&ctx.props().peer_id);

            // Subscribe to global diagnostics for peer_status updates
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
            Msg::Diagnostics(evt) => {
                if evt.subsystem != "peer_status" {
                    return false;
                }
                // Parse peer_status metrics
                let mut to_peer: Option<String> = None;
                let mut audio_enabled: Option<bool> = None;
                let mut video_enabled: Option<bool> = None;
                let mut screen_enabled: Option<bool> = None;
                for m in &evt.metrics {
                    match (m.name, &m.value) {
                        ("to_peer", MetricValue::Text(p)) => to_peer = Some(p.clone()),
                        ("audio_enabled", MetricValue::U64(v)) => audio_enabled = Some(*v != 0),
                        ("video_enabled", MetricValue::U64(v)) => video_enabled = Some(*v != 0),
                        ("screen_enabled", MetricValue::U64(v)) => screen_enabled = Some(*v != 0),
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
                changed
            }
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        // Delegate rendering to the existing canvas generator so DOM structure and CSS remain consistent
        generate_for_peer(
            &ctx.props().client,
            &ctx.props().peer_id,
            ctx.props().full_bleed,
        )
    }

    fn destroy(&mut self, _ctx: &Context<Self>) {
        if let Some(handle) = self.abort_handle.take() {
            handle.abort();
        }
    }
}
