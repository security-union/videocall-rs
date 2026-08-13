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

// The SSR bin monomorphizes the full-page view type; raise the recursion limit
// so computing its layout doesn't overflow the default (128). See lib.rs.
#![recursion_limit = "512"]

use cfg_if::cfg_if;

// boilerplate to run in different modes
cfg_if! {
    if #[cfg(feature = "ssr")] {
        use leptos::prelude::*;
        use axum::{
            routing::post,
            Router,
        };
        use leptos_website::app::{shell, App};
        use leptos_axum::{generate_route_list, LeptosRoutes};
        use tower_http::compression::CompressionLayer;
        use axum::extract::Request;
        use axum::middleware::Next;
        use axum::response::Response;
        use axum::http::HeaderValue;

        // Middleware to add no-cache headers. Axum 0.7+ dropped the `<B>` body
        // generic on `Next`/`Request`, so this now uses the concrete types.
        async fn add_no_cache_headers(req: Request, next: Next) -> Response {
            let mut response = next.run(req).await;

            response.headers_mut().insert(
                "Cache-Control",
                HeaderValue::from_static("no-cache, no-store, must-revalidate, max-age=0")
            );
            response.headers_mut().insert(
                "Pragma",
                HeaderValue::from_static("no-cache")
            );
            response.headers_mut().insert(
                "Expires",
                HeaderValue::from_static("0")
            );

            response
        }

        #[tokio::main]
        async fn main() {
            simple_logger::init_with_level(log::Level::Warn).expect("couldn't initialize logging");
            // `get_configuration` is synchronous in Leptos 0.7+.
            let conf = get_configuration(None).unwrap();
            let leptos_options = conf.leptos_options;
            let addr = leptos_options.site_addr;
            let routes = generate_route_list(App);

            // build our application with a route. The Leptos 0.7+ axum
            // integration renders through the `shell` document function and
            // serves 404s / static assets via the built-in file_and_error_handler.
            let app = Router::new()
            .route("/api/{*fn_name}", post(leptos_axum::handle_server_fns))
            .leptos_routes(&leptos_options, routes, {
                let leptos_options = leptos_options.clone();
                move || shell(leptos_options.clone())
            })
            .fallback(leptos_axum::file_and_error_handler(shell))
            .with_state(leptos_options)
            .layer(CompressionLayer::new())
            .layer(axum::middleware::from_fn(add_no_cache_headers));

            // run our app with hyper
            leptos::logging::log!("listening on http://{}", &addr);
            let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
            axum::serve(listener, app.into_make_service())
                .await
                .unwrap();
        }
    } else {
        // Non-ssr builds (csr / hydrate) have no server entry point; the client
        // is driven by the wasm `hydrate()` in lib.rs. A stub `main` keeps the
        // bin target compiling when `cargo check` visits it without the ssr feature.
        pub fn main() {}
    }
}
