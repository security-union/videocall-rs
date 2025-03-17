use videocall_client::VideoCallClient;
use yew::prelude::*;

#[derive(Properties, PartialEq)]
pub struct DiagnosticsPanelProps {
    pub client: VideoCallClient,
    pub peers: Vec<String>,
    #[prop_or_default]
    pub onclose: Callback<MouseEvent>,
}

#[function_component(DiagnosticsPanel)]
pub fn diagnostics_panel(props: &DiagnosticsPanelProps) -> Html {
    html! {
        <div class="diagnostics-panel">
            <div class="diagnostics-panel-header">
                <h3>{"Diagnostics"}</h3>
                <button 
                    style="background: none; border: none; color: white; cursor: pointer;"
                    onclick={props.onclose.clone()}>
                    <svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                        <line x1="18" y1="6" x2="6" y2="18"></line>
                        <line x1="6" y1="6" x2="18" y2="18"></line>
                    </svg>
                </button>
            </div>
            <div class="diagnostics-content">
                <div class="diagnostics-summary">
                    {props.client.get_diagnostics_summary()}
                </div>
                
                {
                    if !props.peers.is_empty() {
                        html! {
                            <>
                                <h5>{"Connected Peers: "}{props.peers.len()}</h5>
                                <div class="peer-ids">
                                    {props.peers.iter().map(|peer_id| {
                                        html! { <div class="peer-id">{peer_id.clone()}</div> }
                                    }).collect::<Html>()}
                                </div>
                            </>
                        }
                    } else {
                        html! {<p>{"No peers connected"}</p>}
                    }
                }
            </div>
        </div>
    }
} 