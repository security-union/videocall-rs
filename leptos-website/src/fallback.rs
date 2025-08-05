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

use cfg_if::cfg_if;

cfg_if! {
if #[cfg(feature = "ssr")] {
    use axum::{
        body::{boxed, Body, BoxBody},
        extract::State,
        response::IntoResponse,
        http::{Request, Response, StatusCode, Uri},
    };
    use axum::response::Response as AxumResponse;
    use tower::ServiceExt;
    use tower_http::services::ServeDir;
    use leptos::{LeptosOptions, Errors, view};
    use crate::error_template::ErrorTemplate;
    use crate::errors::TodoAppError;

    pub async fn file_and_error_handler(uri: Uri, State(options): State<LeptosOptions>, req: Request<Body>) -> AxumResponse {
        let root = options.site_root.clone();
        let res = get_static_file(uri.clone(), &root).await.unwrap();

        if res.status() == StatusCode::OK {
            res.into_response()
        } else{
            let mut errors = Errors::default();
            errors.insert_with_default_key(TodoAppError::NotFound);
            let handler = leptos_axum::render_app_to_stream(options.to_owned(), move || view!{<ErrorTemplate outside_errors=errors.clone()/>});
            handler(req).await.into_response()
        }
    }

    async fn get_static_file(uri: Uri, root: &str) -> Result<Response<BoxBody>, (StatusCode, String)> {
        let req = Request::builder().uri(uri.clone()).body(Body::empty()).unwrap();
        // `ServeDir` implements `tower::Service` so we can call it with `tower::ServiceExt::oneshot`
        // This path is relative to the cargo root
        match ServeDir::new(root).oneshot(req).await {
            Ok(mut res) => {
                // Add no-cache headers to disable browser caching
                res.headers_mut().insert(
                    "Cache-Control",
                    axum::http::HeaderValue::from_static("no-cache, no-store, must-revalidate, max-age=0")
                );
                res.headers_mut().insert(
                    "Pragma",
                    axum::http::HeaderValue::from_static("no-cache")
                );
                res.headers_mut().insert(
                    "Expires",
                    axum::http::HeaderValue::from_static("0")
                );
                Ok(res.map(boxed))
            },
            Err(err) => Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Something went wrong: {err}"),
            )),
        }
    }


}
}
