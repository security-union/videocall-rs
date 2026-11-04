// SPDX-License-Identifier: MIT OR Apache-2.0

//! "What's new" disclosure listing the changes shipped in recent builds. Shared by
//! the About dialog and the in-meeting Meeting info dialog.

use crate::components::signal_quality::prefers_reduced_motion;
use dioxus::html::{ScrollBehavior, ScrollLogicalPosition, ScrollToOptions};
use dioxus::prelude::*;
use serde::Deserialize;
use std::cell::RefCell;
use std::rc::Rc;

const CHANGELOG_PATH: &str = "/assets/changelog.json";
const INITIAL_BUILDS: usize = 3;
const OLDER_BUILDS_STEP: usize = 10;
const MIN_SHA_PREFIX: usize = 7;

/// `(line prefix, pill label, CSS slug)` per kind of change, in display order.
const KINDS: [(&str, &str, &str); 6] = [
    ("Feature", "Features", "features"),
    ("UX", "UX", "ux"),
    ("Perf", "Performance", "performance"),
    ("Reliability", "Reliability", "reliability"),
    ("bug-fix", "Fixes", "fixes"),
    ("Security", "Security", "security"),
];
const OTHER_KIND: (&str, &str) = ("Miscellaneous", "miscellaneous");

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
struct ChangelogBuild {
    built: String,
    version: String,
    commit: String,
    pending: bool,
    changes: Vec<String>,
    #[serde(skip)]
    built_label: String,
    #[serde(skip)]
    change_list: ChangeList,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ChangeList {
    Flat(Vec<String>),
    Grouped(Vec<ChangeGroup>),
}

impl Default for ChangeList {
    fn default() -> Self {
        Self::Flat(Vec::new())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ChangeGroup {
    label: &'static str,
    slug: &'static str,
    changes: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ChangelogFile {
    builds: Vec<ChangelogBuild>,
}

type Builds = Rc<Vec<ChangelogBuild>>;

thread_local! {
    static CACHE: RefCell<Option<Builds>> = const { RefCell::new(None) };
}

fn cached_builds() -> Option<Builds> {
    CACHE.with(|cache| cache.borrow().clone())
}

fn cache_builds(builds: Vec<ChangelogBuild>) -> Builds {
    let builds = Rc::new(builds);
    CACHE.with(|cache| *cache.borrow_mut() = Some(builds.clone()));
    builds
}

/// Clears the page-session cache so each wasm test case fetches afresh.
#[doc(hidden)]
#[allow(dead_code)]
pub fn reset_changelog_cache_for_test() {
    CACHE.with(|cache| *cache.borrow_mut() = None);
}

fn parse_changelog(body: &str) -> Result<Vec<ChangelogBuild>, String> {
    let mut builds = serde_json::from_str::<ChangelogFile>(body)
        .map_err(|e| e.to_string())?
        .builds;
    for build in &mut builds {
        build
            .changes
            .retain(|change| !display_change(change).1.is_empty());
    }
    let mut builds = arrange_sections(builds);
    for build in &mut builds {
        build.change_list = group_changes(&std::mem::take(&mut build.changes));
    }
    Ok(builds)
}

/// The `KINDS` index a line's prefix names, and the line as shown: without
/// that prefix or a trailing ` (#N)`.
fn display_change(line: &str) -> (Option<usize>, &str) {
    let named = line.split_once(": ").and_then(|(key, rest)| {
        KINDS
            .iter()
            .position(|&(k, _, _)| k == key)
            .map(|i| (i, rest))
    });
    let (kind, text) = match named {
        Some((i, rest)) => (Some(i), rest),
        None => (None, line),
    };
    (kind, strip_pr_number(text).trim())
}

fn strip_pr_number(text: &str) -> &str {
    let Some((head, number)) = text.strip_suffix(')').and_then(|t| t.rsplit_once("(#")) else {
        return text;
    };
    let numbered = !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit());
    if numbered && (head.is_empty() || head.ends_with(' ')) {
        head
    } else {
        text
    }
}

fn group_changes(changes: &[String]) -> ChangeList {
    let mut buckets = vec![Vec::<String>::new(); KINDS.len() + 1];
    for change in changes {
        let (kind, text) = display_change(change);
        let bucket = &mut buckets[kind.unwrap_or(KINDS.len())];
        if !bucket.iter().any(|shown| shown == text) {
            bucket.push(text.to_string());
        }
    }
    if buckets[..KINDS.len()].iter().all(Vec::is_empty) {
        return ChangeList::Flat(buckets.pop().unwrap_or_default());
    }
    let pills = KINDS
        .iter()
        .map(|&(_, label, slug)| (label, slug))
        .chain([OTHER_KIND]);
    ChangeList::Grouped(
        pills
            .zip(buckets)
            .filter(|(_, changes)| !changes.is_empty())
            .map(|((label, slug), changes)| ChangeGroup {
                label,
                slug,
                changes,
            })
            .collect(),
    )
}

/// Every pending section merged into the first one (changes in file order,
/// exact duplicates dropped, omitted when empty), then dated builds that list
/// changes, newest first.
fn arrange_sections(builds: Vec<ChangelogBuild>) -> Vec<ChangelogBuild> {
    let (pending, mut dated): (Vec<_>, Vec<_>) = builds.into_iter().partition(|b| b.pending);
    dated.retain(|b| !b.changes.is_empty());
    dated.sort_by(|a, b| b.built.cmp(&a.built));
    let mut pending = pending.into_iter();
    let Some(mut unreleased) = pending.next() else {
        return dated;
    };
    let mut changes: Vec<String> = Vec::new();
    for change in std::mem::take(&mut unreleased.changes)
        .into_iter()
        .chain(pending.flat_map(|b| b.changes))
    {
        if !changes.contains(&change) {
            changes.push(change);
        }
    }
    if changes.is_empty() {
        return dated;
    }
    unreleased.changes = changes;
    std::iter::once(unreleased).chain(dated).collect()
}

fn initial_visible(total: usize) -> usize {
    total.min(INITIAL_BUILDS)
}

fn reveal_older(visible: usize, total: usize) -> usize {
    visible.saturating_add(OLDER_BUILDS_STEP).min(total)
}

fn show_older_label(visible: usize, total: usize) -> Option<String> {
    match total.saturating_sub(visible) {
        0 => None,
        1 => Some("Show 1 older build".to_string()),
        remaining => Some(format!(
            "Show {} older builds",
            remaining.min(OLDER_BUILDS_STEP)
        )),
    }
}

fn is_running_build(commit: &str, running_sha: &str) -> bool {
    let commit = commit.trim().to_ascii_lowercase();
    let running = running_sha.trim().to_ascii_lowercase();
    let usable =
        |sha: &str| sha.len() >= MIN_SHA_PREFIX && sha.bytes().all(|b| b.is_ascii_hexdigit());
    usable(&commit)
        && usable(&running)
        && (commit.starts_with(&running) || running.starts_with(&commit))
}

fn marks_running_build(build: &ChangelogBuild, running_sha: &str) -> bool {
    !build.pending && is_running_build(&build.commit, running_sha)
}

fn version_label(build: &ChangelogBuild) -> Option<String> {
    if build.pending {
        return None;
    }
    match build.version.trim().trim_start_matches('v') {
        "" => None,
        version => Some(format!("v{version}")),
    }
}

fn label_built_dates(
    builds: &mut [ChangelogBuild],
    mut format: impl FnMut(&str) -> Option<String>,
) {
    for build in builds {
        build.built_label = if build.pending {
            "Unreleased".to_string()
        } else {
            match format(&build.built) {
                Some(label) => label,
                None if build.built.trim().is_empty() => "Undated build".to_string(),
                None => build.built.clone(),
            }
        };
    }
}

async fn fetch_changelog() -> Result<Vec<ChangelogBuild>, String> {
    let origin = web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .ok_or("no window origin")?;
    let resp = reqwest::get(format!("{origin}{CHANGELOG_PATH}"))
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    let body = resp.text().await.map_err(|e| e.to_string())?;
    let mut builds = parse_changelog(&body)?;
    label_built_dates(
        &mut builds,
        crate::constants::build_datetime_local_minutes_formatter(),
    );
    Ok(builds)
}

#[derive(Clone, PartialEq)]
enum LogState {
    Idle,
    Loading,
    Ready(Builds),
    Unavailable,
}

fn load(mut state: Signal<LogState>) {
    state.set(LogState::Loading);
    spawn(async move {
        state.set(match fetch_changelog().await {
            Ok(builds) => LogState::Ready(cache_builds(builds)),
            Err(e) => {
                log::warn!("change log unavailable: {e}");
                LogState::Unavailable
            }
        });
    });
}

fn scroll_to_top_of_card(anchor: Rc<MountedData>) {
    let behavior = if prefers_reduced_motion() {
        ScrollBehavior::Instant
    } else {
        ScrollBehavior::Smooth
    };
    spawn(async move {
        let _ = anchor
            .scroll_to_with_options(ScrollToOptions {
                behavior,
                vertical: ScrollLogicalPosition::Start,
                horizontal: ScrollLogicalPosition::Nearest,
            })
            .await;
    });
}

#[component]
pub fn WhatsNew(id_prefix: &'static str) -> Element {
    let mut expanded = use_signal(|| false);
    let state = use_signal(|| cached_builds().map_or(LogState::Idle, LogState::Ready));
    let mut anchor = use_signal(|| None::<Rc<MountedData>>);
    let mut toggle_button = use_signal(|| None::<Rc<MountedData>>);
    let panel_id = format!("{id_prefix}-changelog-panel");

    use_effect(move || {
        let _ = state.read();
        if expanded() {
            if let Some(anchor) = anchor.peek().clone() {
                scroll_to_top_of_card(anchor);
            }
        }
    });

    let focus_toggle = move || {
        if let Some(button) = toggle_button.peek().clone() {
            spawn(async move {
                let _ = button.set_focus(true).await;
            });
        }
    };

    let toggle = move |_| {
        let open = !expanded();
        expanded.set(open);
        if !open {
            focus_toggle();
        } else if matches!(*state.peek(), LogState::Idle | LogState::Unavailable) {
            load(state);
        }
    };

    let retry = move |_| {
        focus_toggle();
        load(state);
    };

    let (message, failed) = status_message(&state.read()).unwrap_or(("", false));
    let builds = match &*state.read() {
        LogState::Ready(builds) if !builds.is_empty() => Some(builds.clone()),
        _ => None,
    };

    rsx! {
        div {
            class: "changelog",
            onmounted: move |element| anchor.set(Some(element.data())),
            button {
                r#type: "button",
                class: "btn-apple btn-secondary btn-sm changelog-toggle",
                "aria-expanded": "{expanded}",
                "aria-controls": "{panel_id}",
                "data-testid": "changelog-toggle",
                onmounted: move |element| toggle_button.set(Some(element.data())),
                onclick: toggle,
                "What's new"
                svg {
                    class: "changelog-chevron",
                    "aria-hidden": "true",
                    xmlns: "http://www.w3.org/2000/svg",
                    view_box: "0 0 24 24",
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "2",
                    stroke_linecap: "round",
                    stroke_linejoin: "round",
                    path { d: "m6 9 6 6 6-6" }
                }
            }
            if expanded() {
                div {
                    id: "{panel_id}",
                    class: "changelog-panel",
                    role: "region",
                    "aria-label": "What's new",
                    "data-testid": "changelog-panel",
                    p {
                        class: status_class(message, failed),
                        role: "status",
                        "data-testid": "changelog-status",
                        "{message}"
                    }
                    if failed {
                        button {
                            r#type: "button",
                            class: "btn-apple btn-secondary btn-sm changelog-retry",
                            "data-testid": "changelog-retry",
                            onclick: retry,
                            "Try again"
                        }
                    }
                    if let Some(builds) = builds {
                        ChangelogBuilds { builds, id_prefix }
                    }
                }
            }
        }
    }
}

/// The panel's status line and whether it reports a failure; `None` once
/// builds are listed.
fn status_message(state: &LogState) -> Option<(&'static str, bool)> {
    match state {
        LogState::Idle | LogState::Loading => Some(("Loading change log...", false)),
        LogState::Unavailable => Some(("Change log unavailable", true)),
        LogState::Ready(builds) if builds.is_empty() => Some(("No changes recorded yet", false)),
        LogState::Ready(_) => None,
    }
}

fn status_class(message: &str, failed: bool) -> &'static str {
    match (message.is_empty(), failed) {
        (true, _) => "changelog-status visually-hidden",
        (false, true) => "about-modal-status about-modal-status--error changelog-status",
        (false, false) => "about-modal-status changelog-status",
    }
}

/// The merged pending section, if any, and the dated builds after it.
fn split_unreleased(builds: &[ChangelogBuild]) -> (Option<&ChangelogBuild>, &[ChangelogBuild]) {
    match builds.split_first() {
        Some((first, dated)) if first.pending => (Some(first), dated),
        _ => (None, builds),
    }
}

fn build_section(
    build: &ChangelogBuild,
    key: String,
    heading_id: String,
    focus_on_mount: bool,
) -> Element {
    let version = version_label(build);
    let this_build = marks_running_build(build, env!("GIT_SHA"));
    rsx! {
        section {
            key: "{key}",
            class: "changelog-build",
            "data-testid": "changelog-build",
            "data-pending": build.pending.then_some("true"),
            h5 {
                id: "{heading_id}",
                class: "changelog-build-heading",
                tabindex: "-1",
                onmounted: move |element| {
                    if focus_on_mount {
                        let element = element.data();
                        spawn(async move {
                            let _ = element.set_focus(true).await;
                        });
                    }
                },
                span { class: "changelog-build-date", "{build.built_label}" }
                if let Some(version) = version {
                    span { class: "changelog-build-version", "{version}" }
                }
                if this_build {
                    span {
                        class: "changelog-this-build",
                        "data-testid": "changelog-this-build",
                        "This build"
                    }
                }
            }
            {change_list(&build.change_list)}
        }
    }
}

fn change_list(list: &ChangeList) -> Element {
    match list {
        ChangeList::Flat(changes) => change_items(changes),
        ChangeList::Grouped(groups) => rsx! {
            for group in groups.iter() {
                div {
                    key: "{group.slug}",
                    class: "changelog-group",
                    "data-testid": "changelog-group",
                    "data-kind": "{group.slug}",
                    h6 {
                        class: "changelog-kind changelog-kind--{group.slug}",
                        "data-testid": "changelog-kind",
                        "{group.label}"
                    }
                    {change_items(&group.changes)}
                }
            }
        },
    }
}

fn change_items(changes: &[String]) -> Element {
    rsx! {
        ul { class: "changelog-changes",
            for change in changes.iter() {
                li { "data-testid": "changelog-change", "{change}" }
            }
        }
    }
}

#[component]
fn ChangelogBuilds(builds: Builds, id_prefix: &'static str) -> Element {
    let (unreleased, dated) = split_unreleased(&builds);
    let total = dated.len();
    let mut visible = use_signal(|| initial_visible(total));
    let mut focus_index = use_signal(|| None::<usize>);
    let shown = visible().min(total);

    let unreleased = unreleased.map(|build| {
        build_section(
            build,
            "unreleased".to_string(),
            format!("{id_prefix}-changelog-unreleased"),
            false,
        )
    });
    let sections = dated.iter().take(shown).enumerate().map(|(i, build)| {
        build_section(
            build,
            i.to_string(),
            format!("{id_prefix}-changelog-build-{i}"),
            *focus_index.peek() == Some(i),
        )
    });

    rsx! {
        {unreleased}
        {sections}
        if let Some(label) = show_older_label(shown, total) {
            button {
                r#type: "button",
                class: "btn-apple btn-secondary btn-sm changelog-show-older",
                "data-testid": "changelog-show-older",
                onclick: move |_| {
                    focus_index.set(Some(shown));
                    visible.set(reveal_older(shown, total));
                },
                "{label}"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(built: &str, version: &str) -> ChangelogBuild {
        ChangelogBuild {
            built: built.to_string(),
            version: version.to_string(),
            changes: vec![format!("{version} change")],
            ..Default::default()
        }
    }

    fn parsed(built: &str, version: &str) -> ChangelogBuild {
        ChangelogBuild {
            built: built.to_string(),
            version: version.to_string(),
            change_list: ChangeList::Flat(vec![format!("{version} change")]),
            ..Default::default()
        }
    }

    fn strings(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|line| line.to_string()).collect()
    }

    fn group(label: &'static str, slug: &'static str, changes: &[&str]) -> ChangeGroup {
        ChangeGroup {
            label,
            slug,
            changes: strings(changes),
        }
    }

    fn shown(line: &str) -> (Option<&'static str>, &str) {
        let (kind, text) = display_change(line);
        (kind.map(|i| KINDS[i].0), text)
    }

    #[test]
    fn each_kind_gets_its_pill_in_display_order_with_miscellaneous_last() {
        let list = group_changes(&strings(&[
            "Uncategorised first (#11)",
            "Security: s",
            "bug-fix: f",
            "Reliability: r (#12)",
            "Perf: p",
            "UX: u",
            "Feature: n",
            "UX: u2",
        ]));
        assert_eq!(
            list,
            ChangeList::Grouped(vec![
                group("Features", "features", &["n"]),
                group("UX", "ux", &["u", "u2"]),
                group("Performance", "performance", &["p"]),
                group("Reliability", "reliability", &["r"]),
                group("Fixes", "fixes", &["f"]),
                group("Security", "security", &["s"]),
                group("Miscellaneous", "miscellaneous", &["Uncategorised first"]),
            ])
        );
    }

    #[test]
    fn a_build_with_no_categorized_line_stays_one_flat_list() {
        assert_eq!(
            group_changes(&strings(&[
                "Pinning moves a tile to the front of the grid (#28)",
                "Meeting-api: x",
                "Clippy::y",
            ])),
            ChangeList::Flat(strings(&[
                "Pinning moves a tile to the front of the grid",
                "Meeting-api: x",
                "Clippy::y",
            ]))
        );
    }

    #[test]
    fn only_an_exact_kind_prefix_categorizes_a_line() {
        let list = group_changes(&strings(&[
            "Meeting-api: x (#13)",
            "Clippy::y",
            "ux: lower case",
            "UX:no space",
            "Note: UX: not first",
            "UX: a: b",
        ]));
        assert_eq!(
            list,
            ChangeList::Grouped(vec![
                group("UX", "ux", &["a: b"]),
                group(
                    "Miscellaneous",
                    "miscellaneous",
                    &[
                        "Meeting-api: x",
                        "Clippy::y",
                        "ux: lower case",
                        "UX:no space",
                        "Note: UX: not first",
                    ]
                ),
            ])
        );
    }

    #[test]
    fn one_well_formed_trailing_pr_number_is_stripped() {
        assert_eq!(
            shown("UX: Clearer mute (#12)"),
            (Some("UX"), "Clearer mute")
        );
        assert_eq!(shown("bug-fix: (#12)"), (Some("bug-fix"), ""));
        assert_eq!(shown("Raw title (#12)"), (None, "Raw title"));
        assert_eq!(shown("Two (#1) (#2)"), (None, "Two (#1)"));
        assert_eq!(shown("Bad (#v2)"), (None, "Bad (#v2)"));
        assert_eq!(shown("Empty (#)"), (None, "Empty (#)"));
        assert_eq!(shown("Glued(#12)"), (None, "Glued(#12)"));
        assert_eq!(shown("Mid (#12) text"), (None, "Mid (#12) text"));
    }

    #[test]
    fn lines_with_nothing_left_to_show_are_dropped_with_any_build_they_empty() {
        let builds = parse_changelog(
            r#"{"builds":[
            {"built":"2026-09-29T00:00:00Z","version":"2","changes":["UX: ","UX: (#12)","Perf: kept"]},
            {"built":"2026-09-28T00:00:00Z","version":"1","changes":["(#13)","Security: "]}]}"#,
        )
        .unwrap();
        assert_eq!(builds.len(), 1, "build 1 has nothing left to show");
        assert_eq!(
            builds[0].change_list,
            ChangeList::Grouped(vec![group("Performance", "performance", &["kept"])])
        );
    }

    #[test]
    fn a_repeated_line_shows_once_per_group() {
        assert_eq!(
            group_changes(&strings(&[
                "UX: Same (#1)",
                "Perf: Same",
                "UX: Same (#2)",
                "UX: Next",
            ])),
            ChangeList::Grouped(vec![
                group("UX", "ux", &["Same", "Next"]),
                group("Performance", "performance", &["Same"]),
            ])
        );
    }

    #[test]
    fn the_merged_unreleased_section_is_grouped_once() {
        let builds = parse_changelog(
            r#"{"builds":[
            {"pending":true,"changes":["UX: a (#1)","Perf: b"]},
            {"built":"2026-09-29T00:00:00Z","version":"1","changes":["Shipped"]},
            {"pending":true,"changes":["UX: c","UX: a (#1)","Perf: b (#2)"]}]}"#,
        )
        .unwrap();
        assert_eq!(
            builds[0].change_list,
            ChangeList::Grouped(vec![
                group("UX", "ux", &["a", "c"]),
                group("Performance", "performance", &["b"]),
            ])
        );
        assert_eq!(
            builds[1].change_list,
            ChangeList::Flat(strings(&["Shipped"]))
        );
    }

    #[test]
    fn parse_reads_the_documented_shape() {
        let body = r#"{"builds":[{"built":"2026-09-29T05:00:37Z","version":"1.1.42",
            "commit":"1e7c5d00","changes":["Pinning moves a tile (#2872)","Second"]}]}"#;
        assert_eq!(
            parse_changelog(body).unwrap(),
            vec![ChangelogBuild {
                built: "2026-09-29T05:00:37Z".to_string(),
                version: "1.1.42".to_string(),
                commit: "1e7c5d00".to_string(),
                pending: false,
                changes: Vec::new(),
                built_label: String::new(),
                change_list: ChangeList::Flat(strings(&["Pinning moves a tile", "Second"])),
            }]
        );
    }

    #[test]
    fn built_dates_are_labelled_once_with_raw_and_undated_fallbacks() {
        let mut builds = vec![
            build("2026-09-29T05:00:37Z", "a"),
            build("not-a-date", "b"),
            build(" ", "c"),
        ];
        label_built_dates(&mut builds, |ts| {
            ts.starts_with("2026").then(|| format!("local {ts}"))
        });
        let labels: Vec<_> = builds.iter().map(|b| b.built_label.as_str()).collect();
        assert_eq!(
            labels,
            ["local 2026-09-29T05:00:37Z", "not-a-date", "Undated build"]
        );
    }

    #[test]
    fn parse_defaults_missing_fields_and_ignores_unknown_ones() {
        let builds = parse_changelog(
            r#"{"schema":2,"builds":[{"version":"1.0.0","changes":["1.0.0 change"],"extra":true}]}"#,
        )
        .unwrap();
        assert_eq!(builds, vec![parsed("", "1.0.0")]);
        assert_eq!(parse_changelog("{}").unwrap(), vec![]);
    }

    #[test]
    fn parse_rejects_an_html_fallback_page_and_other_non_json() {
        assert!(parse_changelog("<!DOCTYPE html><html><body></body></html>").is_err());
        assert!(parse_changelog("").is_err());
        assert!(parse_changelog("null").is_err());
        assert!(parse_changelog(r#"{"builds":"nope"}"#).is_err());
    }

    #[test]
    fn parse_drops_blank_change_lines() {
        let builds = parse_changelog(r#"{"builds":[{"changes":["  ","Kept",""]}]}"#).unwrap();
        assert_eq!(builds[0].change_list, ChangeList::Flat(strings(&["Kept"])));
    }

    #[test]
    fn parse_orders_builds_newest_first_whatever_the_file_order() {
        let body = r#"{"builds":[
            {"built":"2026-09-27T10:00:00Z","version":"a","changes":["x"]},
            {"built":"2026-09-29T05:00:37Z","version":"c","changes":["x"]},
            {"built":"","version":"undated","changes":["x"]},
            {"built":"2026-09-28T23:59:59Z","version":"b","changes":["x"]}]}"#;
        let versions: Vec<_> = parse_changelog(body)
            .unwrap()
            .into_iter()
            .map(|b| b.version)
            .collect();
        assert_eq!(versions, ["c", "b", "a", "undated"]);
    }

    #[test]
    fn dated_builds_without_changes_are_not_listed_or_counted() {
        let body = r#"{"builds":[
            {"built":"2026-09-26T00:00:00Z","version":"1","changes":["one"]},
            {"built":"2026-09-27T00:00:00Z","version":"2","changes":[]},
            {"built":"2026-09-28T00:00:00Z","version":"3","changes":["three"]},
            {"built":"2026-09-29T00:00:00Z","version":"4","changes":["  "]},
            {"built":"2026-09-30T00:00:00Z","version":"5","changes":["five"]},
            {"built":"2026-09-25T00:00:00Z","version":"0"}]}"#;
        let builds = parse_changelog(body).unwrap();
        let versions: Vec<_> = builds.iter().map(|b| b.version.as_str()).collect();
        assert_eq!(versions, ["5", "3", "1"]);
        let (_, dated) = split_unreleased(&builds);
        assert_eq!(initial_visible(dated.len()), 3);
        assert_eq!(show_older_label(3, dated.len()), None);
    }

    #[test]
    fn dated_builds_run_newest_first_with_undated_ones_last() {
        let builds = arrange_sections(vec![
            build("", "undated"),
            build("2026-09-28T00:00:00Z", "older"),
            build("2026-09-29T00:00:00Z", "newer"),
        ]);
        let versions: Vec<_> = builds.iter().map(|b| b.version.as_str()).collect();
        assert_eq!(versions, ["newer", "older", "undated"]);
    }

    #[test]
    fn three_builds_show_first_then_ten_more_per_step() {
        assert_eq!(initial_visible(0), 0);
        assert_eq!(initial_visible(2), 2);
        assert_eq!(initial_visible(16), 3);
        assert_eq!(reveal_older(3, 16), 13);
        assert_eq!(reveal_older(13, 16), 16);
        assert_eq!(reveal_older(3, 4), 4);
    }

    #[test]
    fn show_older_label_counts_what_the_next_step_reveals() {
        assert_eq!(show_older_label(3, 3), None);
        assert_eq!(show_older_label(2, 2), None);
        assert_eq!(
            show_older_label(3, 4).as_deref(),
            Some("Show 1 older build")
        );
        assert_eq!(
            show_older_label(3, 6).as_deref(),
            Some("Show 3 older builds")
        );
        assert_eq!(
            show_older_label(3, 13).as_deref(),
            Some("Show 10 older builds")
        );
        assert_eq!(
            show_older_label(3, 40).as_deref(),
            Some("Show 10 older builds")
        );
        assert_eq!(
            show_older_label(13, 16).as_deref(),
            Some("Show 3 older builds")
        );
    }

    #[test]
    fn running_build_matches_on_a_shared_sha_prefix_either_way() {
        assert!(is_running_build("1e7c5d00", "1e7c5d0"));
        assert!(is_running_build("1e7c5d0", "1e7c5d00"));
        assert!(is_running_build("1E7C5D00", "1e7c5d00"));
        assert!(is_running_build(
            "1e7c5d00a4b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5",
            "1e7c5d00"
        ));
        assert!(!is_running_build("1e7c5d00", "1e7c5d11"));
    }

    #[test]
    fn short_or_unknown_shas_never_match() {
        assert!(!is_running_build("1e7c5d", "1e7c5d"));
        assert!(!is_running_build("", ""));
        assert!(!is_running_build("unknown", "unknown"));
        assert!(!is_running_build("1e7c5d00", ""));
        assert!(!is_running_build("", "1e7c5d00"));
    }

    #[test]
    fn version_label_prefixes_one_v_and_omits_a_missing_version() {
        assert_eq!(
            version_label(&build("", "1.1.42")).as_deref(),
            Some("v1.1.42")
        );
        assert_eq!(
            version_label(&build("", "v1.1.42")).as_deref(),
            Some("v1.1.42")
        );
        assert_eq!(version_label(&build("", "  ")), None);
    }

    fn pending(changes: &[&str]) -> ChangelogBuild {
        ChangelogBuild {
            pending: true,
            changes: changes.iter().map(|c| c.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn parse_reads_pending_sections_and_defaults_pending_to_false() {
        let body = r#"{"builds":[
            {"pending":true,"through":"1e7c5d00","prs":[2890],"changes":["Soon (#2890)"]},
            {"built":"2026-09-29T05:00:37Z","version":"1.1.42","changes":["Shipped"]}]}"#;
        let mut builds = parse_changelog(body).unwrap();
        assert_eq!(
            std::mem::take(&mut builds[0].change_list),
            ChangeList::Flat(strings(&["Soon"]))
        );
        assert_eq!(builds[0], pending(&[]));
        assert!(!builds[1].pending);
    }

    #[test]
    fn pending_sections_merge_into_one_first_section_without_duplicate_lines() {
        let first = ChangelogBuild {
            commit: "1e7c5d00".to_string(),
            ..pending(&["A", "Shared"])
        };
        let builds = arrange_sections(vec![
            build("2026-09-27T00:00:00Z", "old"),
            first,
            build("2026-09-29T00:00:00Z", "new"),
            pending(&["B", "Shared", "B"]),
            build("", "undated"),
        ]);
        assert_eq!(builds.len(), 4, "one merged section plus three builds");
        assert!(builds[0].pending);
        assert_eq!(builds[0].changes, ["A", "Shared", "B"]);
        assert_eq!(
            builds[0].commit, "1e7c5d00",
            "the merge keeps the first section"
        );
        let versions: Vec<_> = builds[1..].iter().map(|b| b.version.as_str()).collect();
        assert_eq!(versions, ["new", "old", "undated"]);
        assert!(builds[1..].iter().all(|b| !b.pending));
    }

    #[test]
    fn pending_sections_without_changes_are_dropped() {
        let builds = parse_changelog(
            r#"{"builds":[{"pending":true,"changes":["  "]},{"pending":true},
            {"built":"2026-09-29T00:00:00Z","version":"1.1.42","changes":["1.1.42 change"]}]}"#,
        )
        .unwrap();
        assert_eq!(builds, vec![parsed("2026-09-29T00:00:00Z", "1.1.42")]);
    }

    #[test]
    fn pending_sections_read_unreleased_with_no_version_and_are_never_this_build() {
        let mut section = ChangelogBuild {
            built: "2026-09-29T05:00:37Z".to_string(),
            version: "1.1.42".to_string(),
            commit: "1e7c5d00".to_string(),
            ..pending(&["Soon"])
        };
        let mut formatted = 0;
        label_built_dates(std::slice::from_mut(&mut section), |_| {
            formatted += 1;
            Some("formatted".to_string())
        });
        assert_eq!(section.built_label, "Unreleased");
        assert_eq!(formatted, 0, "a pending section's date is never formatted");
        assert_eq!(version_label(&section), None);
        assert!(!marks_running_build(&section, "1e7c5d00"));
        section.pending = false;
        assert!(
            marks_running_build(&section, "1e7c5d00"),
            "premise: the same commit marks a dated build"
        );
    }

    #[test]
    fn the_status_line_reports_loading_failure_and_an_empty_log() {
        assert_eq!(
            status_message(&LogState::Idle),
            Some(("Loading change log...", false))
        );
        assert_eq!(
            status_message(&LogState::Loading),
            Some(("Loading change log...", false))
        );
        assert_eq!(
            status_message(&LogState::Unavailable),
            Some(("Change log unavailable", true))
        );
        assert_eq!(
            status_message(&LogState::Ready(Rc::new(vec![]))),
            Some(("No changes recorded yet", false))
        );
        assert_eq!(
            status_message(&LogState::Ready(Rc::new(vec![build("", "1")]))),
            None
        );
    }

    #[test]
    fn an_empty_status_line_is_hidden_but_stays_a_styled_line_otherwise() {
        assert_eq!(status_class("", false), "changelog-status visually-hidden");
        assert!(status_class("Loading change log...", false).contains("about-modal-status"));
        assert!(status_class("Change log unavailable", true).contains("about-modal-status--error"));
    }

    #[test]
    fn the_unreleased_section_is_split_off_so_only_dated_builds_count() {
        let builds = arrange_sections(vec![
            build("2026-09-28T00:00:00Z", "old"),
            pending(&["Soon"]),
            build("2026-09-29T00:00:00Z", "new"),
        ]);
        let (unreleased, dated) = split_unreleased(&builds);
        assert_eq!(
            unreleased.map(|b| b.changes.clone()),
            Some(vec!["Soon".to_string()])
        );
        let versions: Vec<_> = dated.iter().map(|b| b.version.as_str()).collect();
        assert_eq!(versions, ["new", "old"]);

        let only_dated = [build("2026-09-29T00:00:00Z", "new")];
        let (unreleased, dated) = split_unreleased(&only_dated);
        assert!(unreleased.is_none());
        assert_eq!(dated.len(), 1);
    }
}
