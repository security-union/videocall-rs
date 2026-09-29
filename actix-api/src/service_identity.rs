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

//! Resolution of the `SERVICE_TYPE` and `REGION` strings that name a relay
//! process in the NATS subjects it publishes diagnostics on (issue #2715).

use std::path::Path;
use std::sync::OnceLock;
use tracing::error;

/// Returned when nothing identifies the value.
pub const UNKNOWN: &str = "unknown";

const SERVICE_TYPE_WEBSOCKET: &str = "websocket";
const SERVICE_TYPE_WEBTRANSPORT: &str = "webtransport";

/// Cargo `[[bin]]` stems (`actix-api/Cargo.toml`) paired with their transport.
const EXE_STEM_SERVICE_TYPES: &[(&str, &str)] = &[
    ("websocket_server", SERVICE_TYPE_WEBSOCKET),
    ("webtransport_server", SERVICE_TYPE_WEBTRANSPORT),
];

fn non_blank(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|trimmed| !trimmed.is_empty())
}

/// A non-blank `env_value` wins. Otherwise `exe_name`'s file stem is matched
/// against [`EXE_STEM_SERVICE_TYPES`]; an unmatched or absent stem yields
/// [`UNKNOWN`].
pub fn resolve_service_type(env_value: Option<&str>, exe_name: Option<&str>) -> String {
    if let Some(explicit) = non_blank(env_value) {
        return explicit.to_string();
    }

    non_blank(exe_name)
        .and_then(|path| Path::new(path).file_stem())
        .and_then(|stem| stem.to_str())
        .and_then(|stem| {
            EXE_STEM_SERVICE_TYPES
                .iter()
                .find(|(candidate, _)| *candidate == stem)
                .map(|(_, service_type)| (*service_type).to_string())
        })
        .unwrap_or_else(|| UNKNOWN.to_string())
}

/// Resolve the region from an explicit `REGION` value. A blank or absent value
/// yields [`UNKNOWN`].
pub fn resolve_region(env_value: Option<&str>) -> String {
    non_blank(env_value).unwrap_or(UNKNOWN).to_string()
}

/// [`resolve_service_type`] for this process, resolved once: the health-packet
/// path calls this per packet and the derivation reads `/proc/self/exe`.
pub fn service_type_from_env() -> &'static str {
    static RESOLVED: OnceLock<String> = OnceLock::new();

    RESOLVED.get_or_init(|| {
        let env_value = std::env::var("SERVICE_TYPE").ok();
        let exe = std::env::current_exe().ok();
        let exe_name = exe.as_deref().and_then(Path::to_str);
        let resolved = resolve_service_type(env_value.as_deref(), exe_name);

        if resolved == UNKNOWN {
            error!(
                "SERVICE_TYPE is unset or blank and executable {:?} is not a known relay binary; \
                 publishing diagnostics under '{}'. Set SERVICE_TYPE on this deployment.",
                exe_name, UNKNOWN
            );
        }
        resolved
    })
}

/// [`resolve_region`] applied to this process. Resolved once and reused, on the
/// same terms as [`service_type_from_env`].
pub fn region_from_env() -> &'static str {
    static RESOLVED: OnceLock<String> = OnceLock::new();

    RESOLVED.get_or_init(|| {
        let resolved = resolve_region(std::env::var("REGION").ok().as_deref());

        if resolved == UNKNOWN {
            error!(
                "REGION is unset or blank; publishing diagnostics under '{}'. Set REGION on this \
                 deployment.",
                UNKNOWN
            );
        }
        resolved
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_env_value_wins_over_exe_derivation() {
        assert_eq!(
            resolve_service_type(Some("websocket"), Some("/usr/bin/webtransport_server")),
            "websocket"
        );
        assert_eq!(
            resolve_service_type(Some("server-stats"), Some("/usr/bin/websocket_server")),
            "server-stats"
        );
    }

    #[test]
    fn explicit_env_value_is_trimmed() {
        assert_eq!(
            resolve_service_type(Some("  webtransport \n"), None),
            "webtransport"
        );
        assert_eq!(resolve_region(Some(" singapore ")), "singapore");
    }

    #[test]
    fn websocket_binary_derives_websocket() {
        assert_eq!(
            resolve_service_type(None, Some("/usr/bin/websocket_server")),
            "websocket"
        );
        assert_eq!(
            resolve_service_type(None, Some("websocket_server")),
            "websocket"
        );
    }

    #[test]
    fn webtransport_binary_derives_webtransport_never_websocket() {
        for exe in [
            "/usr/bin/webtransport_server",
            "webtransport_server",
            "target/release/webtransport_server",
        ] {
            let resolved = resolve_service_type(None, Some(exe));
            assert_eq!(resolved, "webtransport", "exe {exe}");
            assert_ne!(resolved, "websocket", "exe {exe}");
        }
    }

    #[test]
    fn blank_env_value_falls_through_to_exe_derivation() {
        assert_eq!(
            resolve_service_type(Some(""), Some("/usr/bin/webtransport_server")),
            "webtransport"
        );
        assert_eq!(
            resolve_service_type(Some("   "), Some("/usr/bin/webtransport_server")),
            "webtransport"
        );
    }

    #[test]
    fn unrecognized_or_absent_exe_yields_unknown() {
        assert_eq!(
            resolve_service_type(None, Some("/usr/bin/metrics_server")),
            UNKNOWN
        );
        assert_eq!(
            resolve_service_type(None, Some("sec_api-4f1c9a2b")),
            UNKNOWN
        );
        assert_eq!(resolve_service_type(None, Some("")), UNKNOWN);
        assert_eq!(resolve_service_type(None, None), UNKNOWN);
        assert_eq!(resolve_service_type(Some(""), None), UNKNOWN);
    }

    #[test]
    fn region_uses_explicit_value_or_unknown() {
        assert_eq!(resolve_region(Some("us-east")), "us-east");
        assert_eq!(resolve_region(Some("")), UNKNOWN);
        assert_eq!(resolve_region(Some("   ")), UNKNOWN);
        assert_eq!(resolve_region(None), UNKNOWN);
    }

    #[test]
    fn region_never_invents_the_removed_us_east_fallback() {
        assert_ne!(resolve_region(None), "us-east");
        assert_ne!(resolve_region(Some("")), "us-east");
    }
}
