use crate::routing::Route;
use dioxus::prelude::*;
use serde::Deserialize;

#[derive(Clone, Copy)]
pub struct SearchVisibleCtx {
    pub is_visible: Signal<bool>,
}

impl SearchVisibleCtx {
    pub fn set_visible(&mut self, visible: bool) {
        self.is_visible.set(visible);
    }
}

/// A single search result from either SearchV2 or the Postgres fallback.
#[derive(Clone, Debug)]
struct SearchResult {
    meeting_id: String,
    state: String,
    host: String,
}

// --- SearchV2 response types ---

#[derive(Deserialize, Debug)]
struct SearchV2Response {
    hits: SearchV2Hits,
}

#[derive(Deserialize, Debug)]
struct SearchV2Hits {
    hits: Vec<SearchV2Hit>,
}

#[derive(Deserialize, Debug)]
struct SearchV2Hit {
    _source: SearchV2Source,
}

#[derive(Deserialize, Debug)]
struct SearchV2Source {
    #[serde(default)]
    title: Option<String>,
    #[serde(rename = "documentObject", default)]
    document_object: Option<SearchV2DocObject>,
}

#[derive(Deserialize, Debug)]
struct SearchV2DocObject {
    #[serde(default)]
    room_id: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    host_display_name: Option<String>,
    #[serde(default)]
    creator_id: Option<String>,
}

/// Query SearchV2 middleware directly.
async fn search_v2(base_url: &str, q: &str) -> Result<Vec<SearchResult>, String> {
    let url = format!("{}/mysearch", base_url.trim_end_matches('/'));

    let body = serde_json::json!({
        "query": {
            "must": [{
                "query_string": {
                    "query": format!("*{}*", q),
                    "fields": ["title", "documentObject.room_id", "documentObject.host_display_name"]
                }
            }]
        },
        "scope": ["cs-vc-meetings"],
        "page": 0,
        "pageSize": 20
    });

    let resp = reqwest::Client::new()
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("SearchV2 request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("SearchV2 returned HTTP {}", resp.status()));
    }

    let data: SearchV2Response = resp
        .json()
        .await
        .map_err(|e| format!("SearchV2 parse error: {e}"))?;

    Ok(data
        .hits
        .hits
        .into_iter()
        .map(|hit| {
            let doc = hit._source.document_object.as_ref();
            SearchResult {
                meeting_id: doc
                    .and_then(|d| d.room_id.clone())
                    .or(hit._source.title.clone())
                    .unwrap_or_default(),
                state: doc
                    .and_then(|d| d.state.clone())
                    .unwrap_or_else(|| "unknown".to_string()),
                host: doc
                    .and_then(|d| d.host_display_name.clone().or(d.creator_id.clone()))
                    .unwrap_or_default(),
            }
        })
        .collect())
}

/// Fallback: query meeting-api Postgres search via the typed client.
async fn search_fallback(q: &str) -> Result<Vec<SearchResult>, String> {
    let client = crate::constants::meeting_api_client()
        .map_err(|e| format!("Client config error: {e}"))?;
    let response = client
        .list_meetings(20, 0, Some(q))
        .await
        .map_err(|e| format!("{e:?}"))?;
    Ok(response
        .meetings
        .into_iter()
        .map(|m| SearchResult {
            meeting_id: m.meeting_id,
            state: m.state,
            host: m.host.unwrap_or_default(),
        })
        .collect())
}

#[component]
pub fn SearchModal() -> Element {
    let nav = use_navigator();
    let mut search_ctx = use_context::<SearchVisibleCtx>();
    let mut query = use_signal(String::new);
    let mut results = use_signal(Vec::<SearchResult>::new);
    let mut is_loading = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);

    let search_base = crate::constants::search_api_base_url()
        .ok()
        .flatten();

    use_resource(move || {
        let q = query.read().clone();
        let base = search_base.clone();
        async move {
            if q.is_empty() {
                results.set(Vec::new());
                return;
            }
            is_loading.set(true);
            error.set(None);

            let res = if let Some(ref url) = base {
                match search_v2(url, &q).await {
                    Ok(items) => Ok(items),
                    Err(e) => {
                        log::warn!("SearchV2 unavailable ({e}), falling back to Postgres");
                        search_fallback(&q).await
                    }
                }
            } else {
                search_fallback(&q).await
            };

            match res {
                Ok(items) => results.set(items),
                Err(e) => error.set(Some(e)),
            }
            is_loading.set(false);
        }
    });

    if !*search_ctx.is_visible.read() {
        return rsx! {};
    }

    rsx! {
        div {
            style: "position:fixed; inset:0; z-index:50; display:flex; align-items:center; justify-content:center; background:rgba(0,0,0,0.5); backdrop-filter:blur(4px);",
            onclick: move |_| search_ctx.set_visible(false),
            div {
                style: "width:100%; max-width:540px; overflow:hidden; border-radius:12px; border:1px solid #374151; background:#1c1c1e; box-shadow:0 25px 50px -12px rgba(0,0,0,0.5);",
                onclick: |evt| evt.stop_propagation(),
                div { style: "display:flex; align-items:center; border-bottom:1px solid #374151; padding:12px 16px;",
                    input {
                        style: "width:100%; background:transparent; font-size:18px; color:#fff; outline:none; border:none;",
                        placeholder: "Search meetings...",
                        value: "{query}",
                        oninput: move |evt| query.set(evt.value()),
                        onkeydown: move |evt| {
                            if evt.key() == Key::Escape {
                                search_ctx.set_visible(false);
                            }
                        },
                        autofocus: true,
                    }
                }
                div { style: "max-height:60vh; overflow-y:auto; padding:8px;",
                    if *is_loading.read() {
                        div { style: "padding:16px 0; text-align:center; color:#6b7280;", "Searching..." }
                    } else if let Some(err) = error.read().as_ref() {
                        div { style: "padding:16px 0; text-align:center; color:#ef4444;", "{err}" }
                    } else if results.read().is_empty() && !query.read().is_empty() {
                        div { style: "padding:16px 0; text-align:center; color:#6b7280;", "No meetings found" }
                    } else {
                        for result in results.read().iter() {
                            {
                                let id = result.meeting_id.clone();
                                let is_active = result.state == "active" || result.state == "idle" || result.state == "created";
                                let is_ended = result.state == "ended";
                                let row_style = if is_active {
                                    "display:flex; align-items:center; justify-content:space-between; padding:10px 14px; margin-bottom:4px; border-radius:8px; cursor:pointer; transition:background 0.15s;"
                                } else {
                                    "display:flex; align-items:center; justify-content:space-between; padding:10px 14px; margin-bottom:4px; border-radius:8px; opacity:0.5;"
                                };
                                let badge_style = if is_active {
                                    "margin-left:12px; flex-shrink:0; border-radius:9999px; background:rgba(22,163,74,0.15); padding:3px 10px; font-size:11px; font-weight:600; text-transform:uppercase; letter-spacing:0.05em; color:#4ade80;"
                                } else if is_ended {
                                    "margin-left:12px; flex-shrink:0; border-radius:9999px; background:rgba(75,85,99,0.2); padding:3px 10px; font-size:11px; font-weight:600; text-transform:uppercase; letter-spacing:0.05em; color:#9ca3af;"
                                } else {
                                    "margin-left:12px; flex-shrink:0; border-radius:9999px; background:rgba(202,138,4,0.15); padding:3px 10px; font-size:11px; font-weight:600; text-transform:uppercase; letter-spacing:0.05em; color:#facc15;"
                                };
                                let badge_text = if is_active {
                                    "Join".to_string()
                                } else if is_ended {
                                    "Ended".to_string()
                                } else {
                                    result.state.clone()
                                };
                                let href = format!("/meeting/{}", result.meeting_id);
                                rsx! {
                                    a {
                                        key: "{result.meeting_id}",
                                        href: "{href}",
                                        style: "{row_style} text-decoration:none; color:inherit;",
                                        onclick: move |evt| {
                                            if !is_active {
                                                evt.prevent_default();
                                                return;
                                            }
                                            let has_meta = evt.modifiers().contains(Modifiers::META)
                                                || evt.modifiers().contains(Modifiers::CONTROL);
                                            if !has_meta {
                                                evt.prevent_default();
                                                search_ctx.set_visible(false);
                                                nav.push(Route::Meeting { id: id.clone() });
                                            } else {
                                                search_ctx.set_visible(false);
                                            }
                                        },
                                        div { style: "display:flex; flex-direction:column; min-width:0; flex:1;",
                                            span { style: "font-size:14px; font-weight:500; color:#fff; white-space:nowrap; overflow:hidden; text-overflow:ellipsis;",
                                                "{result.meeting_id}"
                                            }
                                            if !result.host.is_empty() {
                                                span { style: "font-size:12px; color:#9ca3af; margin-top:2px;",
                                                    "Host: {result.host}"
                                                }
                                            }
                                        }
                                        span { style: "{badge_style}", "{badge_text}" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
