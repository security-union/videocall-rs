use crate::components::icons::peer::PeerIcon;
use yew::prelude::*;
use yew::{html, Component, Html};

pub struct PeerListItem {}

#[derive(Properties, Clone, PartialEq)]
pub struct PeerListItemProps {
    pub name: String,
}

impl Component for PeerListItem {
    type Message = ();

    type Properties = PeerListItemProps;

    fn create(_ctx: &Context<Self>) -> Self {
        Self {}
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        html! {
            <div class="peer_item" >
                <div class="peer_item_icon">
                    <PeerIcon />
                </div>
                <div class="peer_item_text">
                    {ctx.props().name.clone()}
                </div>
            </div>
        }
    }
}
