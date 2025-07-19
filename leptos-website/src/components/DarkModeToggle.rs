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



use leptos::*;
use leptos_meta::Body;
use leptos_router::ActionForm;

#[server(ToggleDarkMode, "/api")]
pub async fn toggle_dark_mode(prefers_dark: bool) -> Result<bool, ServerFnError> {
    use axum::http::{header::SET_COOKIE, HeaderMap, HeaderValue};
    use leptos_axum::{ResponseOptions, ResponseParts};

    let response =
        use_context::<ResponseOptions>().expect("to have leptos_axum::ResponseOptions provided");
    let mut response_parts = ResponseParts::default();
    let mut headers = HeaderMap::new();
    headers.insert(
        SET_COOKIE,
        HeaderValue::from_str(&format!("darkmode={prefers_dark}; Path=/"))
            .expect("to create header value"),
    );
    response_parts.headers = headers;

    response.overwrite(response_parts);
    Ok(prefers_dark)
}

#[cfg(not(feature = "ssr"))]
fn initial_prefers_dark() -> Option<bool> {
    use wasm_bindgen::JsCast;

    let doc = document().unchecked_into::<web_sys::HtmlDocument>();
    let query = window()
        .match_media("(prefers-color-scheme: dark)")
        .ok()
        .and_then(|ql| ql.map(|ql| ql.matches()));
    let cookie = doc.cookie().unwrap_or_default();
    if cookie.contains("darkmode") {
        Some(cookie.contains("darkmode=true"))
    } else {
        query
    }
}

#[cfg(feature = "ssr")]
fn initial_prefers_dark() -> Option<bool> {
    use axum_extra::extract::cookie::CookieJar;
    use_context::<leptos_axum::RequestParts>().and_then(|req| {
        let cookies = CookieJar::from_headers(&req.headers);
        cookies.get("darkmode").and_then(|v| match v.value() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        })
    })
}

#[component]
pub fn DarkModeToggle() -> impl IntoView {
    let initial = initial_prefers_dark();
    let toggle_dark_mode_action = create_server_action::<ToggleDarkMode>();
    // input is `Some(value)` when pending, and `None` if not pending
    let input = toggle_dark_mode_action.input();
    // value contains most recently-returned value
    let value = toggle_dark_mode_action.value();
    let prefers_dark = create_memo(move |_| {
        match (input(), value()) {
            // if there's some current input, use that optimistically
            (Some(submission), _) => Some(submission.prefers_dark),
            // otherwise, if there was a previous value confirmed by server, use that
            (_, Some(Ok(value))) => Some(value),
            // otherwise, use the initial value
            _ => initial,
        }
    });
    let prefers_dark = move || {
        if cfg!(feature = "ssr") {
            initial
        } else {
            prefers_dark()
        }
    };

    view! {
        <Body class=move || match prefers_dark() {
            Some(true) => "dark".to_string(),
            Some(false) => "light".to_string(),
            _ => "".to_string(),
        }/>
        <script>{include_str!("DarkModeToggle.js")}</script>
        <ActionForm action=toggle_dark_mode_action
            class="flex items-center"
        >
            <input
                type="hidden"
                name="prefers_dark"
                value=move || (!(prefers_dark().unwrap_or(false))).to_string()
            />
            <button
                type="submit"
            >
                <img
                    class="h-6 w-6 hidden dark:block"
                    src="/images/sun.svg"
                    alt="Go to Light Mode"
                />
                <img
                    class="h-6 w-6 block dark:hidden"
                    src="/images/moon.svg"
                    alt="Go to Dark Mode"
                />
            </button>
        </ActionForm>
    }
}
