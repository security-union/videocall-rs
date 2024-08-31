#![feature(future_join)]
#[allow(non_camel_case_types)]
mod components;
mod constants;
mod pages;

use constants::{E2EE_ENABLED, LOGIN_URL, WEBTRANSPORT_ENABLED};
use types::truthy;

use log::info;
use yew::prelude::*;
#[macro_use]
extern crate lazy_static;
use components::{attendants::AttendantsComponent, matomo::MatomoTracker, top_bar::TopBar};
use gloo_utils::window;
use yew_router::prelude::*;
use enum_display::EnumDisplay;
use pages::home::Home;

use crate::constants::ENABLE_OAUTH;

#[derive(Clone, Routable, PartialEq, Debug, EnumDisplay)]
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
    let matomo = MatomoTracker::new();
    matomo.track_page_view(&routes.to_string(), &routes.to_string());
    match routes {
        Route::Home => html! { <Home /> },
        Route::Login => html! { <Login/> },
        Route::Meeting { email, id } => html! {
            <>
                <TopBar/>
                <AttendantsComponent email={email} id={id} webtransport_enabled={*WEBTRANSPORT_ENABLED} e2ee_enabled={*E2EE_ENABLED} />
            </>
        },
        Route::Meeting2 {
            email,
            id,
            webtransport_enabled,
        } => html! {
            <>
                <TopBar/>
                <AttendantsComponent email={email} id={id} webtransport_enabled={truthy(Some(&webtransport_enabled))} e2ee_enabled={*E2EE_ENABLED} />
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

    fn view(&self, _ctx: &Context<Self>) -> Html {
        info!("OAuth enabled: {}", *ENABLE_OAUTH);
        html! {
            <BrowserRouter>
                <Switch<Route> render={switch} />
            </BrowserRouter>
        }
    }
}

fn main() {
    #[cfg(feature = "debugAssertions")]
    {
        _ = console_log::init_with_level(log::Level::Debug);
    }
    #[cfg(not(feature = "debugAssertions"))]
    {
        _ = console_log::init_with_level(log::Level::Info);
    }

    console_error_panic_hook::set_once();
    yew::Renderer::<App>::new().render();
}
