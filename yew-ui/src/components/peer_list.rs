use web_sys::HtmlInputElement;
use yew::prelude::*;
use yew::{html, Component, Context};

pub struct PeerList {
    search_query: String,
}

#[derive(Properties, Clone, PartialEq)]
pub struct PeerListProperties {
    pub peers: Vec<String>,
    pub onclose: yew::Callback<yew::MouseEvent>,
}

pub enum PeerListMsg {
    UpdateSearchQuery(String),
}

impl Component for PeerList {
    type Message = PeerListMsg;

    type Properties = PeerListProperties;

    fn create(_ctx: &Context<Self>) -> Self {
        PeerList {
            search_query: String::new(),
        }
    }

    fn update(&mut self, _ctx: &Context<Self>, msg: Self::Message) -> bool {
        match msg {
            PeerListMsg::UpdateSearchQuery(query) => {
                self.search_query = query;
                true
            }
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let filtered_peers: Vec<_> = ctx
            .props()
            .peers
            .iter()
            .filter(|peer| {
                peer.to_lowercase()
                    .contains(&self.search_query.to_lowercase())
                })
            .cloned()
            .collect();

        let search_peers = ctx.link().callback(|e: InputEvent| {
            let input: HtmlInputElement = e.target_unchecked_into();
            PeerListMsg::UpdateSearchQuery(input.value())
        });

        html! {
        <>
            <div id="peer-list-container-header">
                <h1>{ "This is the Peer List." }</h1>
                <button onclick={ctx.props().onclose.clone()}>{ "X" }</button>
            </div>
            <div>
                <input
                    type="text"
                    placeholder="Search..."
                    value={self.search_query.clone()}
                    oninput={search_peers}
                />
                <ul>
                    { for filtered_peers.iter().map(|peer|
                        html!{
                            <li>{ peer } </li>        
                        })
                    }
                </ul>
            </div>
        </>
        }
    }
}
