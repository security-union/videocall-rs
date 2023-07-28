#![feature(future_join)]

mod components;
mod constants;
mod crypto;
mod model;
mod pages;

use constants::{truthy, LOGIN_URL, WEBTRANSPORT_ENABLED};

use yew::prelude::*;
#[macro_use]
extern crate lazy_static;
use components::{attendants::AttendantsComponent, top_bar::TopBar};
use gloo_console::log;
use gloo_utils::window;
use yew_router::prelude::*;

use pages::home::Home;

use crate::constants::ENABLE_OAUTH;

#[derive(Clone, Routable, PartialEq)]
enum Route {
    #[at("/")]
    Home,
    #[at("/login")]
    Login,
    #[at("/meeting/:email/:id")]
    Meeting { email: String, id: String },
    #[at("/meeting/:email/:id/:webtransport_enabled")]
    Meeting2 {
        email: String,
        id: String,
        webtransport_enabled: String,
    },
    #[not_found]
    #[at("/404")]
    NotFound,
}

fn switch(routes: Route) -> Html {
    match routes {
        Route::Home => html! { <Home /> },
        Route::Login => html! { <Login/> },
        Route::Meeting { email, id } => html! {
            <>
                <TopBar/>
                <AttendantsComponent email={email} id={id} webtransport_enabled={*WEBTRANSPORT_ENABLED} />
            </>
        },
        Route::Meeting2 {
            email,
            id,
            webtransport_enabled,
        } => html! {
            <>
                <TopBar/>
                <AttendantsComponent email={email} id={id} webtransport_enabled={truthy(webtransport_enabled)} />
            </>
        },
        Route::NotFound => html! { <h1>{ "404" }</h1> },
    }
}

#[function_component(Login)]
fn login() -> Html {
    let login = Callback::from(|_: MouseEvent| {
        window().location().set_href(LOGIN_URL).ok();
    });
    html! {<>
        <input type="image" onclick={login.clone()} src="/assets/btn_google.png" />
    </>}
}

struct App {}

impl Component for App {
    type Message = ();
    type Properties = ();

    fn create(_: &Context<Self>) -> Self {
        App {}
    }

    fn rendered(&mut self, ctx: &Context<Self>, first_render: bool) {
        if first_render {
            ctx.link().send_message(());
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        log!("OAuth enabled: {}", *ENABLE_OAUTH);
        html! {
            <BrowserRouter>
                <Switch<Route> render={switch} />
            </BrowserRouter>
        }
    }
}

fn main() {
    yew::Renderer::<App>::new().render();
}
