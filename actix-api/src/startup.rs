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

//! Startup visibility for feature flags (issue #2410).

use tracing::{error, warn};
use videocall_types::feature_flags::MEETING_MANAGEMENT_FLAG;
use videocall_types::{FeatureFlags, ResolvedFlag};

/// Spellings a deployment uses to turn a flag off on purpose. Anything else that resolves
/// false was probably meant to be truthy.
const DELIBERATELY_FALSY: [&str; 5] = ["false", "0", "no", "off", ""];

/// `"value"` when the env var was set, `<unset>` when it was not. Quoted so that a stray
/// space or newline in the value is visible in the log.
fn render_raw(raw: Option<&str>) -> String {
    match raw {
        Some(value) => format!("{value:?}"),
        None => "<unset>".to_string(),
    }
}

/// One line naming every feature flag, the raw string its env var held, and its resolved
/// value.
pub fn feature_flag_summary_line(flags: &[ResolvedFlag]) -> String {
    let body = flags
        .iter()
        .map(|flag| {
            format!(
                "{}={} -> {}",
                flag.env_var,
                render_raw(flag.raw.as_deref()),
                flag.enabled
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    format!("feature flags resolved at startup: {body}")
}

/// Warning for the deprecated tokenless lobby path. `lobby::ws_connect` returns 410 Gone and
/// the WebTransport connect handler rejects tokenless joins only while `meeting_management`
/// is true, so a false flag leaves both open. `None` once the flag resolves true.
pub fn deprecated_lobby_path_warning(flag: &ResolvedFlag) -> Option<String> {
    if flag.enabled {
        return None;
    }
    Some(format!(
        "SECURITY: {}={} -> false, so the DEPRECATED TOKENLESS lobby path is ENABLED: \
         GET /lobby/{{user_id}}/{{room}} and the matching WebTransport path admit joins \
         carrying no room token (issue #2298). Set {}=true to close it; the recognised \
         truthy values are true, 1 and yes, case-insensitive and with no surrounding \
         whitespace.",
        flag.env_var,
        render_raw(flag.raw.as_deref()),
        flag.env_var
    ))
}

/// Warning for an env var that was set yet resolved false because nothing recognised its
/// value — the `FEATURE_MEETING_MANAGEMENT=ture` class of misconfiguration.
pub fn unrecognised_value_warning(flag: &ResolvedFlag) -> Option<String> {
    let raw = flag.raw.as_deref()?;
    let normalised = raw.trim().to_lowercase();
    if flag.enabled || DELIBERATELY_FALSY.contains(&normalised.as_str()) {
        return None;
    }
    Some(format!(
        "{}={} was set but is not a recognised on/off value, so it resolved to false.",
        flag.env_var,
        render_raw(Some(raw))
    ))
}

/// Emit the startup feature-flag lines. Callers must install the tracing subscriber first,
/// an ordering the `startup_lines_follow_subscriber_init` test pins in both relay binaries.
///
/// The two lines that report a LIVE VULNERABILITY are `error!`, not `warn!`. Both relays
/// filter with `EnvFilter::from_default_env()`, whose default directive is `ERROR` when
/// `RUST_LOG` is unset — and a deploy that forgot the feature flag is exactly the deploy
/// likely to have forgotten `RUST_LOG` too, so a `warn!` there reaches nobody. `ERROR` is
/// the only level that survives regardless. The healthy-path summary stays `warn!`: it is
/// not an error, and every current deploy path sets `RUST_LOG`.
pub fn log_feature_flags() {
    emit_startup_lines(&FeatureFlags::resolved());
}

/// The emission itself, over flags the caller supplies rather than the process environment,
/// so a test can pin the LEVEL of each line without depending on ambient global flag state.
fn emit_startup_lines(flags: &[ResolvedFlag]) {
    warn!("{}", feature_flag_summary_line(flags));
    for flag in flags {
        if let Some(line) = unrecognised_value_warning(flag) {
            error!("{line}");
        }
    }
    if let Some(line) = flags
        .iter()
        .find(|flag| flag.name == MEETING_MANAGEMENT_FLAG)
        .and_then(deprecated_lobby_path_warning)
    {
        error!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use videocall_types::feature_flags::DATABASE_FLAG;

    const WEBSOCKET_MAIN: &str = include_str!("bin/websocket_server.rs");
    const WEBTRANSPORT_MAIN: &str = include_str!("bin/webtransport_server.rs");

    fn flag(name: &'static str, env_var: &str, raw: Option<&str>, enabled: bool) -> ResolvedFlag {
        ResolvedFlag {
            name,
            env_var: env_var.to_string(),
            raw: raw.map(str::to_string),
            enabled,
        }
    }

    fn meeting_management(raw: Option<&str>, enabled: bool) -> ResolvedFlag {
        flag(
            MEETING_MANAGEMENT_FLAG,
            "FEATURE_MEETING_MANAGEMENT",
            raw,
            enabled,
        )
    }

    /// Guards the ordering that made #2410 worth filing: a log emitted before
    /// `tracing_subscriber(..).init()` is dropped by the facade and never reaches stderr.
    fn assert_logged_after_subscriber_init(source: &str, binary: &str) {
        let subscriber = source.find("tracing_subscriber::fmt()").unwrap_or_else(|| {
            panic!("{binary}: no `tracing_subscriber::fmt()` — subscriber install moved?")
        });
        let init = source[subscriber..]
            .find(".init();")
            .map(|offset| subscriber + offset)
            .unwrap_or_else(|| panic!("{binary}: the subscriber builder is never `.init()`ed"));
        let call = source
            .find("log_feature_flags()")
            .unwrap_or_else(|| panic!("{binary}: never calls `log_feature_flags()`"));
        assert!(
            call > init,
            "{binary}: `log_feature_flags()` at byte {call} runs before `.init();` at byte {init}"
        );
    }

    /// Collects whatever a subscriber actually wrote, so a test can assert on emitted
    /// records rather than on the strings the formatters return.
    #[derive(Clone, Default)]
    struct CapturedLogs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedLogs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("log sink poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl tracing_subscriber::fmt::MakeWriter<'_> for CapturedLogs {
        type Writer = Self;

        fn make_writer(&self) -> Self::Writer {
            self.clone()
        }
    }

    /// Runs `log_feature_flags` under a WARN ceiling.
    fn startup_output_at_warn_level() -> String {
        let sink = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .with_writer(sink.clone())
            .finish();
        capture(subscriber, sink, log_feature_flags)
    }

    fn capture<S>(subscriber: S, sink: CapturedLogs, emit: impl FnOnce()) -> String
    where
        S: tracing::Subscriber + Send + Sync + 'static,
    {
        tracing::subscriber::with_default(subscriber, emit);
        let bytes = sink.0.lock().expect("log sink poisoned").clone();
        String::from_utf8(bytes).expect("subscriber wrote non-utf8")
    }

    /// Emit `flags` under the filter a relay gets when `RUST_LOG` is UNSET. Built
    /// explicitly rather than by mutating the environment, so this is parallel-safe, and
    /// driven from a supplied slice so `set_meeting_management_override` elsewhere in the
    /// suite cannot perturb it.
    fn output_with_rust_log_unset(flags: Vec<ResolvedFlag>) -> String {
        let sink = CapturedLogs::default();
        let filter = tracing_subscriber::EnvFilter::builder()
            .with_default_directive(tracing::level_filters::LevelFilter::ERROR.into())
            .parse_lossy("");
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(sink.clone())
            .finish();
        capture(subscriber, sink, || emit_startup_lines(&flags))
    }

    /// A deploy that forgot `FEATURE_MEETING_MANAGEMENT` is the one likely to have
    /// forgotten `RUST_LOG` too, so the vulnerability lines must clear the `ERROR` floor.
    #[test]
    fn the_deprecated_path_warning_survives_an_unset_rust_log() {
        let output = output_with_rust_log_unset(vec![meeting_management(None, false)]);
        assert!(
            output.contains("DEPRECATED TOKENLESS lobby path is ENABLED"),
            "the SECURITY line was filtered out when RUST_LOG is unset. Captured: {output:?}"
        );
    }

    /// Same for the typo case — a set-but-unrecognised value is also a live vulnerability.
    #[test]
    fn the_unrecognised_value_warning_survives_an_unset_rust_log() {
        let output = output_with_rust_log_unset(vec![meeting_management(Some("ture"), false)]);
        assert!(
            output.contains("not a recognised on/off value"),
            "the typo warning was filtered out when RUST_LOG is unset. Captured: {output:?}"
        );
    }

    /// #2410 checkbox 2. The relays run `RUST_LOG=warn`, so emitting the summary at `info!`
    /// makes it invisible on every cluster no matter how correct the string is. Asserting on
    /// `feature_flag_summary_line`'s return value cannot see that; only emitted output can.
    #[test]
    fn every_flag_is_visible_at_the_production_level() {
        let output = startup_output_at_warn_level();
        assert!(
            output.contains("feature flags resolved at startup:"),
            "summary did not survive a WARN filter. Captured: {output:?}"
        );
        assert!(
            output.contains("FEATURE_MEETING_MANAGEMENT="),
            "meeting_management missing from warn-level output. Captured: {output:?}"
        );
        assert!(
            output.contains("DATABASE_ENABLED="),
            "database missing from warn-level output. Captured: {output:?}"
        );
    }

    #[test]
    fn startup_lines_follow_subscriber_init() {
        assert_logged_after_subscriber_init(WEBSOCKET_MAIN, "websocket_server");
        assert_logged_after_subscriber_init(WEBTRANSPORT_MAIN, "webtransport_server");
    }

    /// Without this entry `log_feature_flags`'s `find` yields `None` and the security warning
    /// silently stops being emitted. Also pins the env var the deploy configs must set.
    #[test]
    fn resolved_flags_carry_the_meeting_management_entry() {
        let flags = FeatureFlags::resolved();
        let found = flags
            .iter()
            .find(|flag| flag.name == MEETING_MANAGEMENT_FLAG)
            .expect("FeatureFlags::resolved() must report meeting_management");
        assert_eq!(found.env_var, "FEATURE_MEETING_MANAGEMENT");
        assert!(
            flags.iter().any(|flag| flag.env_var == "DATABASE_ENABLED"),
            "every flag must appear in the startup summary"
        );
    }

    #[test]
    fn unset_flag_warns_that_the_deprecated_path_is_enabled() {
        let line = deprecated_lobby_path_warning(&meeting_management(None, false))
            .expect("an unset flag must produce the warning");
        assert!(line.contains("FEATURE_MEETING_MANAGEMENT=<unset>"));
        assert!(line.contains("DEPRECATED TOKENLESS lobby path is ENABLED"));
        assert!(line.contains("/lobby/{user_id}/{room}"));
    }

    #[test]
    fn typo_flag_warns_and_shows_the_raw_value() {
        let flag = meeting_management(Some("ture"), false);
        let path = deprecated_lobby_path_warning(&flag).expect("a false flag must warn");
        assert!(path.contains(r#"FEATURE_MEETING_MANAGEMENT="ture""#));
        let value =
            unrecognised_value_warning(&flag).expect("`ture` is in neither recognised value set");
        assert!(value.contains(r#"FEATURE_MEETING_MANAGEMENT="ture""#));
    }

    #[test]
    fn trailing_whitespace_is_visible_in_the_raw_value() {
        let line = deprecated_lobby_path_warning(&meeting_management(Some("true "), false))
            .expect("a false flag must warn");
        assert!(line.contains(r#"FEATURE_MEETING_MANAGEMENT="true ""#));
    }

    #[test]
    fn enabled_flag_produces_no_warning() {
        assert!(deprecated_lobby_path_warning(&meeting_management(Some("true"), true)).is_none());
        assert!(unrecognised_value_warning(&meeting_management(Some("true"), true)).is_none());
    }

    #[test]
    fn deliberately_falsy_values_do_not_warn_about_the_value() {
        assert!(unrecognised_value_warning(&meeting_management(Some("false"), false)).is_none());
        assert!(unrecognised_value_warning(&meeting_management(Some("0"), false)).is_none());
        assert!(unrecognised_value_warning(&meeting_management(None, false)).is_none());
    }

    #[test]
    fn summary_reports_every_flag_and_its_raw_value() {
        let line = feature_flag_summary_line(&[
            meeting_management(None, false),
            flag(DATABASE_FLAG, "DATABASE_ENABLED", Some("true"), true),
        ]);
        assert_eq!(
            line,
            "feature flags resolved at startup: FEATURE_MEETING_MANAGEMENT=<unset> -> false; \
             DATABASE_ENABLED=\"true\" -> true"
        );
    }
}
