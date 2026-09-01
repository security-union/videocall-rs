// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0
//
// Issue #2291: `auth::refresh_access_token` must resolve the provider token
// endpoint through `token_endpoint::resolve_token_endpoint`. The
// `token_endpoint` unit tests pin the resolver body through its
// injected-network seam and never execute that call site, so reverting it to
// the base `constants::oauth_token_url()` read leaves `cargo test -p
// videocall-ui --lib` fully green. Pinning it needs a real `__APP_CONFIG`,
// `sessionStorage` and `fetch`, hence a browser target.

#![cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]

use wasm_bindgen_test::*;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const ISSUER: &str = "https://id.test.invalid";
const DISCOVERY_URL: &str = "https://id.test.invalid/.well-known/openid-configuration";
const DISCOVERED_TOKEN_URL: &str = "https://id.test.invalid/oauth2/v1/token";
const EXPLICIT_TOKEN_URL: &str = "https://explicit.test.invalid/oauth2/v1/token";
const REFRESHED_ACCESS_TOKEN: &str = "refreshed-access-token-2291";

/// Issuer-only (`token_url: None`) is the shape both PKCE clusters render.
fn inject_pkce_config(token_url: Option<&str>, issuer: Option<&str>) {
    let config = js_sys::Object::new();
    let set = |key: &str, val: &wasm_bindgen::JsValue| {
        js_sys::Reflect::set(&config, &key.into(), val).unwrap();
    };
    set("apiBaseUrl", &"http://test:8080".into());
    set("wsUrl", &"ws://test:8080".into());
    set("webTransportHost", &"https://test:4433".into());
    set("oauthEnabled", &"true".into());
    set("e2eeEnabled", &"false".into());
    set("webTransportEnabled", &"false".into());
    set("usersAllowedToStream", &"".into());
    set("serverElectionPeriodMs", &wasm_bindgen::JsValue::from(2000));
    set("oauthFlow", &"pkce".into());
    set("oauthClientId", &"videocall-test-client".into());
    if let Some(url) = token_url {
        set("oauthTokenUrl", &url.into());
    }
    if let Some(iss) = issuer {
        set("oauthIssuer", &iss.into());
    }

    let frozen = js_sys::Object::freeze(&config);
    js_sys::Reflect::set(&gloo_utils::window(), &"__APP_CONFIG".into(), &frozen).unwrap();
    // One wasm module serves every case, so clear the `app_config()` memo (#1492).
    dioxus_ui::constants::reset_config_cache_for_test();
}

/// Serve the discovery document and the token POST; 404 anything else so a
/// stray request surfaces as a failed assertion rather than a silent pass.
fn install_recording_fetch() {
    let script = format!(
        r#"
        window.__vc_fetch_log = [];
        window.__original_fetch = window.__original_fetch || window.fetch;
        window.fetch = function(input, init) {{
            var url = typeof input === 'string' ? input : input.url;
            var method = (init && init.method) ||
                         (typeof input === 'object' && input && input.method) ||
                         'GET';
            method = String(method).toUpperCase();
            window.__vc_fetch_log.push(method + ' ' + url);

            var status = 200;
            var body;
            if (method === 'GET' && url === {discovery}) {{
                body = JSON.stringify({{ issuer: {issuer}, token_endpoint: {discovered} }});
            }} else if (method === 'POST' && (url === {discovered} || url === {explicit})) {{
                body = JSON.stringify({{
                    access_token: {access},
                    token_type: 'Bearer',
                    expires_in: 3600
                }});
            }} else {{
                status = 404;
                body = JSON.stringify({{ error: 'unexpected_request' }});
            }}

            var resp = new Response(body, {{
                status: status,
                headers: {{ 'Content-Type': 'application/json' }}
            }});
            Object.defineProperty(resp, 'url', {{ value: url }});
            return Promise.resolve(resp);
        }};
        "#,
        discovery = js_literal(DISCOVERY_URL),
        issuer = js_literal(ISSUER),
        discovered = js_literal(DISCOVERED_TOKEN_URL),
        explicit = js_literal(EXPLICIT_TOKEN_URL),
        access = js_literal(REFRESHED_ACCESS_TOKEN),
    );
    js_sys::eval(&script).expect("failed to install the recording fetch stub");
}

fn js_literal(s: &str) -> String {
    serde_json::to_string(s).expect("test constant should serialize as a JS string literal")
}

/// Every request the stub saw, as `"METHOD url"`, in order.
fn fetch_log() -> Vec<String> {
    let log = js_sys::Reflect::get(&gloo_utils::window(), &"__vc_fetch_log".into())
        .expect("__vc_fetch_log should exist once the stub is installed");
    js_sys::Array::from(&log)
        .iter()
        .map(|v| v.as_string().unwrap_or_default())
        .collect()
}

fn reset_browser_state() {
    js_sys::eval(
        r#"
        if (window.__original_fetch) {
            window.fetch = window.__original_fetch;
            delete window.__original_fetch;
        }
        delete window.__vc_fetch_log;
        "#,
    )
    .expect("failed to restore fetch");
    let _ = js_sys::Reflect::delete_property(&gloo_utils::window().into(), &"__APP_CONFIG".into());
    dioxus_ui::constants::reset_config_cache_for_test();
    if let Some(storage) = web_sys::window().and_then(|w| w.session_storage().ok().flatten()) {
        let _ = storage.clear();
    }
}

/// MUTATION (run, not inferred): restore the base body `let token_endpoint =
/// crate::constants::oauth_token_url().ok_or_else(...)?;` and this fails with
/// `left: Err("OAUTH_TOKEN_URL not configured")` and an empty `fetch_log()`.
#[wasm_bindgen_test]
async fn issuer_only_config_discovers_the_endpoint_then_posts_the_refresh() {
    reset_browser_state();
    install_recording_fetch();
    inject_pkce_config(None, Some(ISSUER));
    dioxus_ui::auth::store_refresh_token("refresh-token-fixture");

    let first = dioxus_ui::auth::refresh_access_token().await;
    assert_eq!(
        first,
        Ok(REFRESHED_ACCESS_TOKEN.to_string()),
        "an issuer-only PKCE config must still refresh (issue #2291); requests seen: {:?}",
        fetch_log()
    );
    assert_eq!(
        fetch_log(),
        vec![
            format!("GET {DISCOVERY_URL}"),
            format!("POST {DISCOVERED_TOKEN_URL}"),
        ],
        "refresh must resolve via OIDC discovery, then POST to the discovered endpoint"
    );

    let second = dioxus_ui::auth::refresh_access_token().await;
    assert_eq!(second, Ok(REFRESHED_ACCESS_TOKEN.to_string()));
    assert_eq!(
        fetch_log(),
        vec![
            format!("GET {DISCOVERY_URL}"),
            format!("POST {DISCOVERED_TOKEN_URL}"),
            format!("POST {DISCOVERED_TOKEN_URL}"),
        ],
        "the resolver's session cache must spare the second refresh a discovery hop"
    );

    reset_browser_state();
}

/// An explicit `oauthTokenUrl` must still win, so setting it costs no extra hop.
#[wasm_bindgen_test]
async fn explicit_token_url_posts_directly_without_discovery() {
    reset_browser_state();
    install_recording_fetch();
    inject_pkce_config(Some(EXPLICIT_TOKEN_URL), Some(ISSUER));
    dioxus_ui::auth::store_refresh_token("refresh-token-fixture");

    let got = dioxus_ui::auth::refresh_access_token().await;
    assert_eq!(got, Ok(REFRESHED_ACCESS_TOKEN.to_string()));
    assert_eq!(
        fetch_log(),
        vec![format!("POST {EXPLICIT_TOKEN_URL}")],
        "an explicit oauthTokenUrl must short-circuit discovery"
    );

    reset_browser_state();
}
