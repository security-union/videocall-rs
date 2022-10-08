use reqwasm::http::Request;
use types::HelloResponse;
use yew::prelude::*;
#[macro_use]
extern crate lazy_static;
use gloo_console::log;
use gloo_utils::document;
use gloo_utils::window;
use wasm_bindgen::JsCast;
use web_sys::HtmlDocument;

use yew_router::prelude::*;

// This is read at compile time, please restart if you change this value.
const ACTIX_PORT: &str = std::env!("ACTIX_PORT");
const LOGIN_URL: &str = std::env!("LOGIN_URL");

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
    #[at("/")]
    Main,
    #[not_found]
    #[at("/404")]
    NotFound,
}

fn switch(routes: &Route) -> Html {
    match routes {
        Route::Login => html! { <Login/> },
        Route::Main => html! {
            <HttpGetExample/>
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
    if *ENABLE_OAUTH {
        html! {
            <BrowserRouter>
            <Switch<Route> render={Switch::render(switch)} />
            </BrowserRouter>
        }
    } else {
        html! {
            <HttpGetExample/>
        }
    }
}

#[function_component(HttpGetExample)]
fn get_example() -> Html {
    if *ENABLE_OAUTH {
        let document = document().unchecked_into::<HtmlDocument>();

        // If there's a cookie, assume that we are logged in, else redirect to login page.
        if let Ok(e) = document.cookie() {
            // TODO: Validate cookie
            if e.is_empty() {
                window().location().set_href("/login").ok();
            }
        } else {
            window().location().set_href("/login").ok();
        }
    }
    let actix_url: String = format!("http://localhost:{}", ACTIX_PORT);
    let hello_response = Box::new(use_state(|| None));
    let error = Box::new(use_state(|| None));
    let endpoint = Box::new(format!(
        "{actix_url}/hello/{name}",
        actix_url = actix_url,
        name = "world",
    ));
    let retry = {
        let hello_response = hello_response.clone();
        let error = error.clone();
        let endpoint = endpoint.clone();
        Callback::from(move |_| {
            let hello_response = hello_response.clone();
            let error = error.clone();
            let endpoint = endpoint.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let fetched_response = Request::get(&endpoint).send().await;
                match fetched_response {
                    Ok(response) => {
                        let json: Result<HelloResponse, _> = response.json().await;
                        match json {
                            Ok(f) => {
                                hello_response.set(Some(f));
                            }
                            Err(e) => error.set(Some(e.to_string())),
                        }
                    }
                    Err(e) => error.set(Some(e.to_string())),
                }
            });
        })
    };
    let logout = Callback::from(|_: MouseEvent| {
        // Clear the cookie and go to login.
        document().unchecked_into::<HtmlDocument>().set_cookie("");
        window().location().set_href("/login").ok();
    });

    match (*hello_response).as_ref() {
        Some(response) => html! {
            <div>
                <p>{ response.name.clone() }</p>
                <button onclick={logout}>{"logout"}</button>
            </div>
        },
        None => match (*error).as_ref() {
            Some(e) => {
                html! {
                    <>
                        {"error"} {e}
                        <button onclick={retry}>{"retry"}</button>
                        <button onclick={logout}>{"logout"}</button>
                    </>
                }
            }
            None => {
                html! {
                    <>
                        <button onclick={retry}>{"Call GET "}{endpoint}</button>
                        <button onclick={logout}>{"logout"}</button>
                    </>
                }
            }
        },
    }
}

fn main() {
    yew::start_app::<App>();
}
