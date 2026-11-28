// SPDX-License-Identifier: MIT OR Apache-2.0

//! The version gate: poll until each release reports the expected sha.
//!
//! A finished rollout only means a container started (and, without a
//! readiness probe, not even that it answers). The sha a service reports
//! shows the new build is up and serving. It is a liveness signal, not an
//! integrity control: the digest pin is the integrity control.

use crate::config::VersionCheck;
use crate::pin::ShortSha;
use crate::registry::{Method, Request, Transport, UreqTransport};
use std::collections::HashMap;
use std::time::Duration;

/// Timeout of one version fetch. Short on purpose: the gate checks its
/// deadline between rounds, so this bounds how far a round can overshoot.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Time between polls.
pub const INTERVAL: Duration = Duration::from_secs(5);

/// Fetch a URL body (network errors are values: the gate keeps polling).
pub trait Fetch {
    /// GET `url`; `Err` on network errors or non-2xx.
    fn get(&self, url: &str) -> Result<String, String>;
}

/// [`Fetch`] over HTTP, with a cache-busting query and `Cache-Control`.
pub struct HttpFetch(UreqTransport);

impl HttpFetch {
    /// A fetcher with [`FETCH_TIMEOUT`] per request.
    pub fn new() -> Self {
        HttpFetch(UreqTransport::new(FETCH_TIMEOUT, false))
    }
}

impl Fetch for HttpFetch {
    fn get(&self, url: &str) -> Result<String, String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let sep = if url.contains('?') { '&' } else { '?' };
        let request = Request {
            method: Method::Get,
            url: format!("{url}{sep}t={now}"),
            headers: vec![("Cache-Control", "no-cache".to_string())],
        };
        let resp = self.0.send(&request)?;
        match resp.status {
            200..=299 => resp.body,
            status => Err(format!("HTTP {status}")),
        }
    }
}

/// Time, so tests do not sleep.
pub trait Clock {
    /// Time since some fixed point.
    fn now(&self) -> Duration;
    /// Wait.
    fn sleep(&self, d: Duration);
}

/// Real time.
pub struct SystemClock(std::time::Instant);

impl SystemClock {
    /// Start counting now.
    pub fn new() -> Self {
        SystemClock(std::time::Instant::now())
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d)
    }
}

/// One release the gate waits for.
#[derive(Debug, Clone)]
pub struct Target {
    /// Name shown in logs (the release).
    pub label: String,
    /// Where and how it reports.
    pub check: VersionCheck,
    /// What it must report.
    pub expected: ShortSha,
}

/// Pull the reported sha out of an endpoint body. `None` if the pointer
/// leads nowhere, to a non-string, or the selector matches zero or several
/// elements.
pub fn reported(check: &VersionCheck, body: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(body).ok()?;
    let base = match &check.select {
        None => &json,
        Some(select) => {
            let mut hits = json.pointer(&select.array)?.as_array()?.iter().filter(|e| {
                e.get(&select.field).and_then(|v| v.as_str()) == Some(select.equals.as_str())
            });
            let one = hits.next()?;
            if hits.next().is_some() {
                return None;
            }
            one
        }
    };
    base.pointer(&check.pointer)?.as_str().map(str::to_string)
}

/// Whether a reported sha names the expected commit: the same short sha, or
/// a longer hex id that starts with it.
pub fn matches(reported: &str, expected: &ShortSha) -> bool {
    reported == expected.as_str()
        || (reported.len() > expected.as_str().len()
            && reported.len() <= 40
            && reported.starts_with(expected.as_str())
            && reported
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// How the wait ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Every target reported its expected sha.
    Converged,
    /// The deadline passed; what each lagging target last reported.
    TimedOut(Vec<(String, String)>),
}

/// Poll every `interval` until all targets match or `timeout` passes. Each
/// distinct URL is fetched once per round. The deadline is checked after
/// each round, so a run can overshoot it by one round of fetches (at most
/// [`FETCH_TIMEOUT`] per distinct URL with [`HttpFetch`]).
pub fn wait(
    targets: &[Target],
    timeout: Duration,
    interval: Duration,
    fetch: &dyn Fetch,
    clock: &dyn Clock,
) -> Outcome {
    let deadline = clock.now().saturating_add(timeout);
    loop {
        let mut bodies: HashMap<&str, Result<String, String>> = HashMap::new();
        let mut lagging = Vec::new();
        for target in targets {
            let url = target.check.url.as_str();
            let body = bodies.entry(url).or_insert_with(|| fetch.get(url));
            let seen = match body {
                Ok(body) => reported(&target.check, body).unwrap_or_else(|| "missing".into()),
                Err(e) => format!("unreachable ({e})"),
            };
            eprintln!("{} reports {seen}, want {}", target.label, target.expected);
            if !matches(&seen, &target.expected) {
                lagging.push((target.label.clone(), seen));
            }
        }
        if lagging.is_empty() {
            return Outcome::Converged;
        }
        if clock.now() >= deadline {
            return Outcome::TimedOut(lagging);
        }
        clock.sleep(
            interval
                .min(deadline.saturating_sub(clock.now()))
                .max(Duration::from_millis(1)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Select;
    use std::cell::{Cell, RefCell};

    const VERSIONS: &str = "https://api.example.com/versions";
    const UI: &str = "https://app.example.com/version.json";

    pub struct FakeClock(pub Cell<Duration>);
    impl Clock for FakeClock {
        fn now(&self) -> Duration {
            self.0.get()
        }
        fn sleep(&self, d: Duration) {
            self.0.set(self.0.get() + d);
        }
    }

    /// Serves a scripted sequence of bodies per URL; repeats the last one.
    struct Script(
        RefCell<HashMap<&'static str, Vec<Result<String, String>>>>,
        Cell<u32>,
    );
    impl Fetch for Script {
        fn get(&self, url: &str) -> Result<String, String> {
            self.1.set(self.1.get() + 1);
            let mut map = self.0.borrow_mut();
            let seq = map.get_mut(url).expect("unexpected url");
            if seq.len() > 1 {
                seq.remove(0)
            } else {
                seq[0].clone()
            }
        }
    }

    fn versions(ws: &str, wt: &str) -> Result<String, String> {
        Ok(format!(
            r#"{{"components":[{{"service":"api","git_sha":"8011b395"}},{{"service":"websocket-relay","git_sha":"{ws}"}},{{"service":"webtransport-relay","git_sha":"{wt}"}}]}}"#
        ))
    }

    fn component(service: &str) -> VersionCheck {
        VersionCheck {
            url: VERSIONS.into(),
            pointer: "/git_sha".into(),
            select: Some(Select {
                array: "/components".into(),
                field: "service".into(),
                equals: service.into(),
            }),
        }
    }

    fn ui() -> VersionCheck {
        VersionCheck {
            url: UI.into(),
            pointer: "/git_sha".into(),
            select: None,
        }
    }

    fn target(label: &str, check: VersionCheck) -> Target {
        Target {
            label: label.into(),
            check,
            expected: ShortSha::parse("8011b395", 8).unwrap(),
        }
    }

    #[test]
    fn extracts_reported_shas() {
        let body = versions("aaaaaaaa", "bbbbbbbb").unwrap();
        assert_eq!(
            reported(&component("websocket-relay"), &body).as_deref(),
            Some("aaaaaaaa")
        );
        assert_eq!(reported(&component("dioxus-ui"), &body), None);
        assert_eq!(
            reported(&ui(), r#"{"git_sha":"8011b395"}"#).as_deref(),
            Some("8011b395")
        );
        assert_eq!(reported(&ui(), "<html>"), None);
        assert_eq!(reported(&ui(), r#"{"git_sha":8011}"#), None);
        let twice =
            r#"{"components":[{"service":"a","git_sha":"1"},{"service":"a","git_sha":"2"}]}"#;
        assert_eq!(reported(&component("a"), twice), None, "ambiguous selector");
        let nested = VersionCheck {
            pointer: "/build/commit".into(),
            ..ui()
        };
        assert_eq!(
            reported(&nested, r#"{"build":{"commit":"8011b395d5"}}"#).as_deref(),
            Some("8011b395d5")
        );
    }

    #[test]
    fn full_and_short_shas_match() {
        let want = ShortSha::parse("8011b395", 8).unwrap();
        assert!(matches("8011b395", &want));
        assert!(matches("8011b395d598a78c14efe791190aa906ae675c17", &want));
        assert!(!matches("8011b39", &want));
        assert!(!matches("8011b395-dirty", &want));
        assert!(!matches("missing", &want));
    }

    #[test]
    fn waits_through_old_build_and_outage_then_converges() {
        let script = Script(
            RefCell::new(HashMap::from([(
                VERSIONS,
                vec![
                    versions("11111111", "8011b395"),
                    Err("connection reset".into()),
                    versions("8011b395", "8011b395"),
                ],
            )])),
            Cell::new(0),
        );
        let clock = FakeClock(Cell::new(Duration::ZERO));
        let got = wait(
            &[target("websocket", component("websocket-relay"))],
            Duration::from_secs(90),
            Duration::from_secs(5),
            &script,
            &clock,
        );
        assert_eq!(got, Outcome::Converged);
        assert_eq!(clock.now(), Duration::from_secs(10));
    }

    #[test]
    fn times_out_on_a_crash_looping_build_and_says_what_it_saw() {
        let script = Script(
            RefCell::new(HashMap::from([(
                VERSIONS,
                vec![versions("11111111", "8011b395")],
            )])),
            Cell::new(0),
        );
        let clock = FakeClock(Cell::new(Duration::ZERO));
        let got = wait(
            &[target("websocket", component("websocket-relay"))],
            Duration::from_secs(90),
            Duration::from_secs(5),
            &script,
            &clock,
        );
        assert_eq!(
            got,
            Outcome::TimedOut(vec![("websocket".into(), "11111111".into())])
        );
        assert_eq!(
            clock.now(),
            Duration::from_secs(90),
            "never overshoots the deadline"
        );
    }

    #[test]
    fn missing_component_never_counts_as_converged() {
        let script = Script(
            RefCell::new(HashMap::from([(
                VERSIONS,
                vec![Ok(r#"{"components":[]}"#.into())],
            )])),
            Cell::new(0),
        );
        let clock = FakeClock(Cell::new(Duration::ZERO));
        let got = wait(
            &[target("api", component("api"))],
            Duration::from_secs(10),
            Duration::from_secs(5),
            &script,
            &clock,
        );
        assert_eq!(
            got,
            Outcome::TimedOut(vec![("api".into(), "missing".into())])
        );
    }

    #[test]
    fn each_url_is_fetched_once_per_round() {
        let script = Script(
            RefCell::new(HashMap::from([
                (VERSIONS, vec![versions("8011b395", "8011b395")]),
                (UI, vec![Ok(r#"{"git_sha":"8011b395"}"#.into())]),
            ])),
            Cell::new(0),
        );
        let clock = FakeClock(Cell::new(Duration::ZERO));
        let targets = [
            target("api", component("api")),
            target("websocket", component("websocket-relay")),
            target("webtransport", component("webtransport-relay")),
            target("ui", ui()),
        ];
        let got = wait(
            &targets,
            Duration::ZERO,
            Duration::from_secs(5),
            &script,
            &clock,
        );
        assert_eq!(got, Outcome::Converged);
        assert_eq!(script.1.get(), 2);
    }
}
