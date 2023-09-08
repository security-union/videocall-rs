// This is read at compile time, please restart if you change this value.
pub const LOGIN_URL: &str = std::env!("LOGIN_URL");

// Commented out because it is not as fast as vp9.

pub const ACTIX_WEBSOCKET: &str = concat!(std::env!("ACTIX_UI_BACKEND_URL"), "/lobby");
pub const WEBTRANSPORT_HOST: &str = concat!(std::env!("WEBTRANSPORT_HOST"), "/lobby");

pub fn truthy(s: String) -> bool {
    ["true".to_string(), "1".to_string()].contains(&s.to_lowercase())
}
// We need a lazy static block because these vars need to call a
// few functions.
lazy_static! {
    pub static ref ENABLE_OAUTH: bool = truthy(std::env!("ENABLE_OAUTH").to_string());
    pub static ref WEBTRANSPORT_ENABLED: bool =
        truthy(std::env!("WEBTRANSPORT_ENABLED").to_string());
    pub static ref E2EE_ENABLED: bool = truthy(std::env!("E2EE_ENABLED").to_string());
}
