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

use crate::components::neteq_chart::{ChartType, NetEqChart};
use yew::prelude::*;

#[derive(Properties, PartialEq)]
pub struct DiagnosticsProps {
    /// Whether the diagnostics sidebar is open
    pub is_open: bool,
    /// Callback to close the diagnostics sidebar
    pub on_close: Callback<()>,
    /// Reception diagnostics data
    pub diagnostics_data: Option<String>,
    /// Sending statistics data
    pub sender_stats: Option<String>,
    /// Encoder settings data
    pub encoder_settings: Option<String>,
    /// NetEQ statistics data
    pub neteq_stats: Option<String>,
    /// NetEQ buffer history for charting
    pub neteq_buffer_history: Vec<u64>,
    /// NetEQ jitter history for charting
    pub neteq_jitter_history: Vec<u64>,
    /// Current video enabled state
    pub video_enabled: bool,
    /// Current microphone enabled state
    pub mic_enabled: bool,
    /// Current screen share state
    pub share_screen: bool,
}

#[function_component(Diagnostics)]
pub fn diagnostics(props: &DiagnosticsProps) -> Html {
    let close_handler = {
        let on_close = props.on_close.clone();
        Callback::from(move |_| {
            on_close.emit(());
        })
    };

    html! {
        <div id="diagnostics-sidebar" class={if props.is_open {"visible"} else {""}}>
            <div class="sidebar-header">
                <h2>{"Diagnostics"}</h2>
                <button class="close-button" onclick={close_handler}>{"×"}</button>
            </div>
            <div class="sidebar-content">
                <div class="diagnostics-data">
                    <div class="diagnostics-section">
                        <h3>{"Reception Stats"}</h3>
                        {
                            if let Some(data) = &props.diagnostics_data {
                                html! { <pre>{ data }</pre> }
                            } else {
                                html! { <p>{"No reception data available."}</p> }
                            }
                        }
                    </div>
                    <div class="diagnostics-section">
                        <h3>{"Sending Stats"}</h3>
                        {
                            if let Some(data) = &props.sender_stats {
                                html! { <pre>{ data }</pre> }
                            } else {
                                html! { <p>{"No sending data available."}</p> }
                            }
                        }
                    </div>
                    <div class="diagnostics-section">
                        <h3>{"Encoder Settings"}</h3>
                        {
                            if let Some(data) = &props.encoder_settings {
                                html! { <pre>{ data }</pre> }
                            } else {
                                html! { <p>{"No encoder settings available."}</p> }
                            }
                        }
                    </div>
                    <div class="diagnostics-section">
                        <h3>{"NetEQ Stats"}</h3>
                        {
                            if let Some(data) = &props.neteq_stats {
                                html! { <pre>{ data }</pre> }
                            } else {
                                html! { <p>{"No NetEQ stats available."}</p> }
                            }
                        }
                    </div>
                    <div class="diagnostics-section">
                        <h3>{"Media Status"}</h3>
                        <pre>{format!("Video: {}\nAudio: {}\nScreen Share: {}",
                            if props.video_enabled { "Enabled" } else { "Disabled" },
                            if props.mic_enabled { "Enabled" } else { "Disabled" },
                            if props.share_screen { "Enabled" } else { "Disabled" }
                        )}</pre>
                    </div>
                    <div class="diagnostics-section">
                        <h3>{"NetEQ Buffer / Jitter History"}</h3>
                        <div style="display:flex; gap:12px; align-items:center;">
                            <NetEqChart
                                data={props.neteq_buffer_history.clone()}
                                chart_type={ChartType::Buffer}
                                width={140}
                                height={80}
                            />
                            <NetEqChart
                                data={props.neteq_jitter_history.clone()}
                                chart_type={ChartType::Jitter}
                                width={140}
                                height={80}
                            />
                        </div>
                    </div>
                </div>
            </div>
        </div>
    }
}
