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

use crate::errors::TodoAppError;
use leptos::prelude::*;

// A basic page for the router's `<Routes fallback=..>` (and the axum
// file_and_error_handler fall-through). In Leptos 0.7+ the built-in
// `file_and_error_handler(shell)` renders the app for unmatched URLs, so this
// just needs to display the error and set the response status server-side.
#[component]
pub fn ErrorTemplate(#[prop(optional)] error: Option<TodoAppError>) -> impl IntoView {
    let error = error.unwrap_or(TodoAppError::NotFound);

    // The response status is only meaningful during SSR.
    #[cfg(feature = "ssr")]
    {
        if let Some(response) = use_context::<leptos_axum::ResponseOptions>() {
            response.set_status(error.status_code());
        }
    }

    view! {
        <h1>{error.status_code().to_string()}</h1>
        <p>"Error: " {error.to_string()}</p>
    }
}
