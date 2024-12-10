use videocall_types::truthy;

// This is read at compile time, please restart if you change this value.
pub const LOGIN_URL: &str = std::env!("LOGIN_URL");
pub const ACTIX_WEBSOCKET: &str = concat!(std::env!("ACTIX_UI_BACKEND_URL"), "/lobby");
pub const WEBTRANSPORT_HOST: &str = concat!(std::env!("WEBTRANSPORT_HOST"), "/lobby");
pub const CANVAS_LIMIT: usize = 20;

pub fn split_users(s: Option<&str>) -> Vec<String> {
    if let Some(s) = s {
        s.split(',')
            .filter_map(|s| {
                let s = s.trim().to_string();
                if s.is_empty() {
                    None
                } else {
                    Some(s)
                }
            })
            .collect::<Vec<String>>()
    } else {
        Vec::new()
    }
}
// We need a lazy static block because these vars need to call a
// few functions.
lazy_static! {
    pub static ref ENABLE_OAUTH: bool = truthy(std::option_env!("ENABLE_OAUTH"));
    pub static ref WEBTRANSPORT_ENABLED: bool = truthy(std::option_env!("WEBTRANSPORT_ENABLED"));
    pub static ref E2EE_ENABLED: bool = truthy(std::option_env!("E2EE_ENABLED"));
    pub static ref USERS_ALLOWED_TO_STREAM: Vec<String> =
        split_users(std::option_env!("USERS_ALLOWED_TO_STREAM"));
}
