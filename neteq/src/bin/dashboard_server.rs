#![cfg(feature = "audio_out")]

use axum::{
    extract::Path,
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use clap::Parser;
use std::net::SocketAddr;
use tokio::fs;
use tower::ServiceBuilder;
use tower_http::{
    cors::{Any, CorsLayer},
    set_header::SetResponseHeaderLayer,
};

#[derive(Parser, Debug)]
#[clap(about = "NetEq Dashboard Web Server", version)]
struct Args {
    #[clap(
        short,
        long,
        default_value_t = 8000,
        help = "Port to serve the dashboard on"
    )]
    port: u16,

    #[clap(
        long,
        default_value = "dashboard.html",
        help = "Path to the dashboard HTML file"
    )]
    dashboard_file: String,

    #[clap(
        long,
        default_value = "neteq_stats.jsonl",
        help = "Path to the stats JSON lines file"
    )]
    stats_file: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let args = Args::parse();

    // Build the router with CORS and no-cache headers
    let app = Router::new()
        .route("/", get(dashboard_handler))
        .route("/dashboard.html", get(dashboard_handler))
        .route("/neteq_stats.jsonl", get(stats_handler))
        .route("/static/*file", get(static_file_handler))
        .layer(
            ServiceBuilder::new()
                .layer(
                    CorsLayer::new()
                        .allow_origin(Any)
                        .allow_methods(Any)
                        .allow_headers(Any),
                )
                .layer(SetResponseHeaderLayer::if_not_present(
                    header::CACHE_CONTROL,
                    header::HeaderValue::from_static("no-cache, no-store, must-revalidate"),
                ))
                .layer(SetResponseHeaderLayer::if_not_present(
                    header::PRAGMA,
                    header::HeaderValue::from_static("no-cache"),
                ))
                .layer(SetResponseHeaderLayer::if_not_present(
                    header::EXPIRES,
                    header::HeaderValue::from_static("0"),
                )),
        )
        .with_state(AppState {
            dashboard_file: args.dashboard_file,
            stats_file: args.stats_file,
        });

    let addr = SocketAddr::from(([127, 0, 0, 1], args.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;

    println!(
        "NetEq Dashboard server running at http://localhost:{}",
        args.port
    );
    println!(
        "Open http://localhost:{}/dashboard.html in your browser",
        args.port
    );
    println!("Press Ctrl+C to stop the server");

    axum::serve(listener, app).await?;

    Ok(())
}

#[derive(Clone)]
struct AppState {
    dashboard_file: String,
    stats_file: String,
}

async fn dashboard_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<Html<String>, (StatusCode, String)> {
    match fs::read_to_string(&state.dashboard_file).await {
        Ok(content) => Ok(Html(content)),
        Err(e) => {
            eprintln!("Error reading dashboard file: {}", e);
            Err((
                StatusCode::NOT_FOUND,
                format!("Dashboard file not found: {}", e),
            ))
        }
    }
}

async fn stats_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<Response, (StatusCode, String)> {
    match fs::read_to_string(&state.stats_file).await {
        Ok(content) => {
            let mut response = content.into_response();
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/x-ndjson"),
            );
            Ok(response)
        }
        Err(e) => {
            eprintln!("Error reading stats file: {}", e);
            Err((
                StatusCode::NOT_FOUND,
                format!("Stats file not found: {}", e),
            ))
        }
    }
}

async fn static_file_handler(
    Path(file_path): Path<String>,
) -> Result<Response, (StatusCode, String)> {
    // Security check: prevent directory traversal
    if file_path.contains("..") || file_path.starts_with('/') {
        return Err((StatusCode::BAD_REQUEST, "Invalid file path".to_string()));
    }

    match fs::read(&file_path).await {
        Ok(content) => {
            let content_type = match file_path.split('.').last().unwrap_or("") {
                "html" => "text/html",
                "css" => "text/css",
                "js" => "application/javascript",
                "json" => "application/json",
                "jsonl" => "application/x-ndjson",
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "svg" => "image/svg+xml",
                _ => "application/octet-stream",
            };

            let mut response = content.into_response();
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_str(content_type).unwrap(),
            );
            Ok(response)
        }
        Err(e) => {
            eprintln!("Error reading static file {}: {}", file_path, e);
            Err((StatusCode::NOT_FOUND, "File not found".to_string()))
        }
    }
}
