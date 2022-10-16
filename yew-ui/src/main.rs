mod attendant;
mod constants;
mod meeting_attendants;
mod meeting_self;

use constants::LOGIN_URL;

use yew::prelude::*;
#[macro_use]
extern crate lazy_static;
use gloo_console::log;
use gloo_utils::window;
use meeting_attendants::AttendandsComponent;
use yew_router::prelude::*;

fn truthy(s: String) -> bool {
    ["true".to_string(), "1".to_string()].contains(&s.to_lowercase())
}
// We need a lazy static block because these vars need to call a
// few functions.
lazy_static! {
    static ref ENABLE_OAUTH: bool = truthy(std::env!("ENABLE_OAUTH").to_string());
}

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
