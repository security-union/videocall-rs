#![feature(future_join)]

mod constants;
mod host;
mod meeting_attendants;

use constants::LOGIN_URL;

use yew::prelude::*;
#[macro_use]
extern crate lazy_static;
use gloo_console::log;
use gloo_utils::window;
use meeting_attendants::AttendandsComponent;
use yew_router::prelude::*;

use crate::constants::ENABLE_OAUTH;

#[derive(Clone, Routable, PartialEq)]
enum Route {
    #[at("/login")]
    Login,
    #[at("/meeting/:email/:id")]
    Meeting { id: String, email: String },
    #[not_found]
    #[at("/404")]
    NotFound,
}

fn switch(routes: &Route) -> Html {
    match routes {
        Route::Login => html! { <Login/> },
        Route::Meeting { id, email } => html! {
            <>
                <AttendandsComponent id={id.clone()} email={email.clone()}/>
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

#[function_component(App)]
fn app_component() -> Html {
    log!("OAuth enabled: {}", *ENABLE_OAUTH);
    html! {
        <BrowserRouter>
        <Switch<Route> render={Switch::render(switch)} />
        </BrowserRouter>
    }
}

fn main() {
    yew::start_app::<App>();
}
