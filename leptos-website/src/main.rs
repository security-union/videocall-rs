use cfg_if::cfg_if;

// boilerplate to run in different modes
cfg_if! {
    if #[cfg(feature = "ssr")] {
        use leptos::*;
        use axum::{
            routing::{post, },
            Router,
        };
        use leptos_website::app::*;
        use leptos_website::fallback::file_and_error_handler;
        use leptos_axum::{generate_route_list, LeptosRoutes};
        use tower_http::{compression::CompressionLayer};

        #[tokio::main]
        async fn main() {
            simple_logger::init_with_level(log::Level::Warn).expect("couldn't initialize logging");
            let conf = get_configuration(None).await.unwrap();
            let leptos_options = conf.leptos_options;
            let addr = leptos_options.site_addr;
            let routes = generate_route_list(App);

            // build our application with a route
            let app = Router::new()
            .route("/api/*fn_name", post(leptos_axum::handle_server_fns))
            .leptos_routes(&leptos_options, routes, App)
            .fallback(file_and_error_handler)
            .with_state(leptos_options)
            .layer(CompressionLayer::new());

            // run our app with hyper
            // `axum::Server` is a re-export of `hyper::Server`
            logging::log!("listening on http://{}", &addr);
            axum::Server::bind(&addr)
                .serve(app.into_make_service())
                .await
                .unwrap();
        }
    }
}
