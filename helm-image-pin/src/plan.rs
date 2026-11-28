// SPDX-License-Identifier: MIT OR Apache-2.0

//! Deciding the new pins, and the PR title and body that describe them.
//! Pure logic over a [`Line`] and a [`Registry`], so every rule has a test
//! that needs no network and no repository.

use crate::config::Release;
use crate::error::{Error, Result};
use crate::git::{History, Line};
use crate::pin::{CurrentPin, Pin, ShortSha, TagScheme};
use crate::registry::{ImageRef, Registry};
use crate::values;
use std::collections::BTreeSet;

/// The newest published build of one image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Newest {
    /// The pin to write.
    pub pin: Pin,
    /// Position on the first-parent line, 0 = tip.
    pub position: usize,
    /// Full commit id the tag was built from.
    pub commit: String,
}

/// Walk the first-parent commits from the tip and return the first one with
/// a published build tag. Git order decides, never
/// push time, so re-running an old commit's publish job cannot win.
pub fn newest_build(
    line: &Line,
    builds: &BTreeSet<ShortSha>,
    image: &str,
    scheme: &TagScheme,
) -> Result<(usize, ShortSha, String)> {
    for (position, commit) in line.commits().iter().enumerate() {
        let Some(sha) = scheme.sha_of(commit) else {
            continue;
        };
        if !builds.contains(&sha) {
            continue;
        }
        // The tag names a commit only if the prefix is unique on the line.
        match line.find(&sha)? {
            Some((found, _)) if found == position => {}
            _ => {
                return Err(Error::Refused(format!(
                    "{} of {image} does not resolve to a single first-parent commit",
                    scheme.tag(&sha)
                )));
            }
        }
        return Ok((position, sha, commit.clone()));
    }
    Err(Error::Refused(format!(
        "no {} build of {image} anywhere on the first-parent history ({} commits); \
         check that the publish job pushes that tag",
        scheme.placeholder(),
        line.commits().len()
    )))
}

/// Resolve the newest build of `image` and confirm its digest with the
/// registry.
pub fn resolve(
    line: &Line,
    registry: &dyn Registry,
    image: &ImageRef,
    scheme: &TagScheme,
) -> Result<Newest> {
    let builds: BTreeSet<ShortSha> = registry
        .tags(image)?
        .iter()
        .filter_map(|tag| scheme.build_of(tag))
        .collect();
    let (position, sha, commit) = newest_build(line, &builds, &image.written, scheme)?;
    let digest = registry.digest(image, &scheme.tag(&sha))?;
    Ok(Newest {
        pin: scheme.pin_for(sha, digest),
        position,
        commit,
    })
}

/// Whether a values file changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Already pinned to the newest build.
    Unchanged,
    /// Moves to the newest build.
    Bump,
}

/// Decide what happens to one values file's current pin.
pub fn decide(
    file: &str,
    current: &str,
    newest: &Newest,
    line: &Line,
    scheme: &TagScheme,
) -> Result<Status> {
    let parsed = scheme.classify(current);
    match &parsed {
        CurrentPin::Pinned(pin) if pin.sha == newest.pin.sha => {
            if pin.digest == newest.pin.digest {
                Ok(Status::Unchanged)
            } else {
                Err(Error::Refused(format!(
                    "{file} pins {pin} but the registry now serves {} for {}: the tag was re-pushed. \
                     Find out who pushed it before pinning anything.",
                    newest.pin.digest, pin.tag
                )))
            }
        }
        CurrentPin::Bare(sha) if *sha == newest.pin.sha => Ok(Status::Bump),
        CurrentPin::Pinned(_) | CurrentPin::Bare(_) => {
            let sha = parsed.sha().expect("pinned forms carry a sha");
            match line.find(sha)? {
                None => Err(Error::Refused(format!(
                    "{file} pins {current}, which is not on the first-parent history; \
                     refusing to replace it automatically"
                ))),
                Some((position, _)) if position < newest.position => Err(Error::Refused(format!(
                    "{file} pins {current}, newer than the newest available build {}; refusing to downgrade",
                    newest.pin.tag
                ))),
                Some(_) => Ok(Status::Bump),
            }
        }
        CurrentPin::Other(_) => Ok(Status::Bump),
    }
}

/// One values file in the plan.
#[derive(Debug, Clone)]
pub struct Row {
    /// Image repository.
    pub image: String,
    /// Values file, relative to the config's directory.
    pub values_file: String,
    /// Tag as it is now.
    pub current: String,
    /// Tag after this run.
    pub new: String,
    /// Whether it changes.
    pub status: Status,
    /// The values file's full new contents (only when it changes).
    pub new_contents: Option<String>,
    /// Commit of the new pin, for the PR body.
    pub new_commit: String,
    /// Position of the new pin on the line.
    pub new_position: usize,
}

/// A values file to plan for.
#[derive(Debug)]
pub struct Input<'a> {
    /// Which release.
    pub release: &'a Release,
    /// Its current contents.
    pub contents: &'a str,
}

/// Plan every file. Fails on the first refusal, before anything is written.
pub fn plan(
    inputs: &[Input<'_>],
    line: &Line,
    registry: &dyn Registry,
    scheme: &TagScheme,
) -> Result<Vec<Row>> {
    let mut resolved: Vec<(String, Newest)> = Vec::new();
    let mut rows = Vec::new();
    for input in inputs {
        let release = input.release;
        let file = &release.values_file;
        let block = values::read_image(file, input.contents, &release.image_key)?;
        if block.repository != release.image.written {
            return Err(Error::Refused(format!(
                "{file}: {}.repository is {:?}, expected {:?}",
                release.image_key_display(),
                block.repository,
                release.image.written
            )));
        }
        let newest = match resolved
            .iter()
            .find(|(repo, _)| *repo == release.image.written)
        {
            Some((_, newest)) => newest.clone(),
            None => {
                let newest = resolve(line, registry, &release.image, scheme)?;
                eprintln!(
                    "{}: newest {} build is {} ({} commit(s) behind the tip)",
                    release.image,
                    scheme.placeholder(),
                    newest.pin.short(),
                    newest.position
                );
                resolved.push((release.image.written.clone(), newest.clone()));
                newest
            }
        };
        let status = decide(file, &block.tag, &newest, line, scheme)?;
        let new = newest.pin.to_string();
        let new_contents = match status {
            Status::Bump => Some(values::rewrite_tag(
                file,
                input.contents,
                &release.image_key,
                &new,
            )?),
            Status::Unchanged => None,
        };
        rows.push(Row {
            image: release.image.written.clone(),
            values_file: file.clone(),
            current: block.tag,
            new,
            status,
            new_contents,
            new_commit: newest.commit.clone(),
            new_position: newest.position,
        });
    }
    Ok(rows)
}

// ---- PR text ----

/// Most commits listed in the PR body.
const MAX_COMMITS_LISTED: usize = 60;

/// What the body needs besides the rows.
pub struct BodyContext<'a> {
    /// The first-parent line the pins were resolved on.
    pub line: &'a Line,
    /// For commit subjects.
    pub history: &'a dyn History,
    /// How build tags are spelled.
    pub scheme: &'a TagScheme,
    /// Branch the builds come from.
    pub branch: &'a str,
    /// `owner/name`, for a compare link.
    pub github_repo: Option<&'a str>,
    /// Text appended at the end (the config's `pr_note`).
    pub note: Option<&'a str>,
}

fn short(scheme: &TagScheme, tag: &str) -> String {
    match scheme.pin(tag) {
        Some(pin) => pin.short(),
        None => tag.to_string(),
    }
}

/// `deploy: pin images to <build tag of the newest new pin>`.
pub fn title(rows: &[Row], scheme: &TagScheme) -> String {
    let newest = rows
        .iter()
        .min_by_key(|row| row.new_position)
        .and_then(|row| scheme.sha_of(&row.new_commit))
        .map(|sha| scheme.tag(&sha))
        .unwrap_or_else(|| "unknown".into());
    format!("deploy: pin images to {newest}")
}

/// The full PR body.
pub fn body(rows: &[Row], ctx: &BodyContext<'_>) -> Result<String> {
    let scheme = ctx.scheme;
    let mut out = String::new();
    out.push_str(&format!(
        "Pins the production Helm values to the newest images built from {}, by tag and \
         digest (digests shortened below; the diff has them in full).\n\n",
        ctx.branch
    ));
    out.push_str("| Image | Values file | Current pin | New pin |\n|---|---|---|---|\n");
    for row in rows {
        let new = format!("`{}`", short(scheme, &row.new));
        let new = if row.status == Status::Bump {
            format!("**{new}**")
        } else {
            new
        };
        out.push_str(&format!(
            "| `{}` | `{}` | `{}` | {} |\n",
            row.image,
            row.values_file,
            short(scheme, &row.current),
            new
        ));
    }
    out.push('\n');

    // Oldest current pin that is a build on this line starts the range.
    let mut oldest: Option<(usize, String)> = None;
    for row in rows {
        if let Some(sha) = scheme.classify(&row.current).sha()
            && let Some((position, id)) = ctx.line.find(sha)?
            && oldest.as_ref().is_none_or(|(p, _)| position > *p)
        {
            oldest = Some((position, id.to_string()));
        }
    }
    let newest = rows.iter().min_by_key(|row| row.new_position);
    match (oldest, newest) {
        (Some((_, from)), Some(to)) => {
            match ctx.github_repo {
                Some(repo) => out.push_str(&format!(
                    "What ships: https://github.com/{repo}/compare/{from}...{}\n\n",
                    to.new_commit
                )),
                None => out.push_str(&format!(
                    "What ships: `{}..{}`\n\n",
                    &from[..12],
                    &to.new_commit[..12]
                )),
            }
            out.push_str("<details><summary>First-parent commits in that range</summary>\n\n```\n");
            for (sha8, subject) in
                ctx.history
                    .subjects(&from, &to.new_commit, MAX_COMMITS_LISTED)?
            {
                // A backtick could close the fence; swap it out.
                out.push_str(&format!("{sha8} {}\n", subject.replace('`', "'")));
            }
            out.push_str("```\n\n</details>\n");
        }
        _ => out.push_str(&format!(
            "No compare link: no current pin is a {} build from this history.\n",
            scheme.placeholder()
        )),
    }
    if let Some(note) = ctx.note {
        out.push('\n');
        out.push_str(note.trim_end());
        out.push('\n');
    }
    Ok(out)
}

/// Tab-separated rows: image, values file, current tag, new tag, status.
pub fn tsv(rows: &[Row]) -> String {
    let mut out = String::from("image\tvalues_file\tcurrent_tag\tnew_tag\tstatus\n");
    for row in rows {
        let status = match row.status {
            Status::Bump => "bump",
            Status::Unchanged => "unchanged",
        };
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\n",
            row.image, row.values_file, row.current, row.new, status
        ));
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::pin::Digest;
    use std::cell::Cell;
    use std::collections::HashMap;

    pub fn commit(n: u32) -> String {
        format!("{n:08x}{:032x}", n)
    }

    pub fn line(n: u32) -> Line {
        Line::new((0..n).rev().map(commit).collect())
    }

    pub fn digest(n: u32) -> Digest {
        Digest::parse(&format!("sha256:{n:064x}")).unwrap()
    }

    pub fn sha(n: u32) -> ShortSha {
        ShortSha::of_commit(&commit(n), 8).unwrap()
    }

    pub fn scheme() -> TagScheme {
        TagScheme::new("{branch}-{sha}", "main", 8).unwrap()
    }

    pub fn pin_of(n: u32, d: u32) -> String {
        format!("main-{}@{}", sha(n), digest(d))
    }

    pub fn release(name: &str, repository: &str) -> Release {
        Release {
            name: name.into(),
            helm_release: format!("{name}-prod"),
            chart_dir: format!("charts/{name}"),
            values_file: format!("charts/{name}/values.yaml"),
            image_key: vec![name.into(), "image".into()],
            image: ImageRef::parse(repository).unwrap(),
            version: None,
        }
    }

    /// Registry fake: repository path -> (tag -> digest).
    #[derive(Default)]
    pub struct FakeRegistry {
        pub repos: HashMap<String, HashMap<String, Digest>>,
        pub digest_calls: Cell<u32>,
    }

    impl FakeRegistry {
        /// Builds `(commit, digest)` of `repo` under the main-<sha8> scheme,
        /// plus a few unrelated tags.
        pub fn with(mut self, repo: &str, builds: &[(u32, u32)]) -> Self {
            let mut map: HashMap<String, Digest> = builds
                .iter()
                .map(|(c, d)| (format!("main-{}", sha(*c)), digest(*d)))
                .collect();
            map.insert("latest".into(), digest(1000));
            map.insert(format!("pr-{}", sha(1)), digest(1001));
            self.repos.insert(repo.to_string(), map);
            self
        }
    }

    impl Registry for FakeRegistry {
        fn tags(&self, image: &ImageRef) -> Result<Vec<String>> {
            self.repos
                .get(&image.path)
                .map(|m| m.keys().cloned().collect())
                .ok_or_else(|| Error::NotFound(format!("tags/list for {image}")))
        }
        fn digest(&self, image: &ImageRef, tag: &str) -> Result<Digest> {
            self.digest_calls.set(self.digest_calls.get() + 1);
            self.repos
                .get(&image.path)
                .and_then(|m| m.get(tag))
                .cloned()
                .ok_or_else(|| Error::NotFound(format!("manifest {image}:{tag}")))
        }
    }

    fn acme() -> ImageRef {
        ImageRef::parse("acme/x").unwrap()
    }

    fn newest(line: &Line, builds: &[u32]) -> Newest {
        let fake = FakeRegistry::default().with(
            "acme/x",
            &builds.iter().map(|b| (*b, *b)).collect::<Vec<_>>(),
        );
        resolve(line, &fake, &acme(), &scheme()).unwrap()
    }

    #[test]
    fn picks_the_newest_built_commit_even_when_the_tip_has_no_image() {
        // Commits 0..10, tip = 9. This image is path-filtered: only 3 and 6 built.
        let l = line(10);
        let n = newest(&l, &[3, 6]);
        assert_eq!(n.pin.sha, sha(6));
        assert_eq!(n.pin.tag, format!("main-{}", sha(6)));
        assert_eq!(n.position, 3);
        assert_eq!(n.pin.digest, digest(6));
    }

    #[test]
    fn push_order_does_not_matter_only_git_order() {
        // A re-run of commit 2's publish happened last; 7 still wins.
        let l = line(10);
        assert_eq!(newest(&l, &[2, 7]).pin.sha, sha(7));
    }

    #[test]
    fn the_whole_history_is_searched() {
        let l = line(500);
        let fake = FakeRegistry::default().with("acme/x", &[(1, 1)]);
        let found = resolve(&l, &fake, &acme(), &scheme()).unwrap();
        assert_eq!(found.position, 498);
        let none = FakeRegistry::default().with("acme/x", &[]);
        let err = resolve(&l, &none, &acme(), &scheme()).unwrap_err();
        assert!(
            err.to_string()
                .contains("no main-<sha8> build of acme/x anywhere"),
            "{err}"
        );
    }

    #[test]
    fn other_tag_schemes_work() {
        let custom = TagScheme::new("build-{sha}", "trunk", 12).unwrap();
        let l = line(5);
        let mut fake = FakeRegistry::default();
        fake.repos.insert(
            "acme/x".into(),
            HashMap::from([(format!("build-{}", &commit(2)[..12]), digest(2))]),
        );
        let n = resolve(&l, &fake, &acme(), &custom).unwrap();
        assert_eq!(
            n.pin.to_string(),
            format!("build-{}@{}", &commit(2)[..12], digest(2))
        );
    }

    #[test]
    fn ambiguous_build_sha_is_refused() {
        let a = format!("abcdef12{}", "1".repeat(32));
        let b = format!("abcdef12{}", "2".repeat(32));
        let l = Line::new(vec![a, commit(5), b]);
        let builds = BTreeSet::from([ShortSha::parse("abcdef12", 8).unwrap()]);
        assert!(matches!(
            newest_build(&l, &builds, "r", &scheme()),
            Err(Error::Refused(_))
        ));
    }

    #[test]
    fn decisions() {
        let l = line(10);
        let n = newest(&l, &[6]);
        let s = scheme();
        // Same pin: unchanged.
        assert_eq!(
            decide("f", &pin_of(6, 6), &n, &l, &s).unwrap(),
            Status::Unchanged
        );
        // Older pin: bump.
        assert_eq!(
            decide("f", &pin_of(3, 3), &n, &l, &s).unwrap(),
            Status::Bump
        );
        // Bare tag of the same build: bump to tag@digest.
        assert_eq!(
            decide("f", &format!("main-{}", sha(6)), &n, &l, &s).unwrap(),
            Status::Bump
        );
        // Bare older tag, and `latest`: bump.
        assert_eq!(
            decide("f", &format!("main-{}", sha(2)), &n, &l, &s).unwrap(),
            Status::Bump
        );
        assert_eq!(decide("f", "latest", &n, &l, &s).unwrap(), Status::Bump);
    }

    #[test]
    fn re_pushed_tag_is_refused() {
        let l = line(10);
        let n = newest(&l, &[6]);
        let err = decide("f", &pin_of(6, 99), &n, &l, &scheme()).unwrap_err();
        assert!(err.to_string().contains("re-pushed"), "{err}");
        assert_eq!(err.exit_code(), crate::error::EXIT_REFUSED);
    }

    #[test]
    fn downgrade_is_refused() {
        // Pin is at 8 but the newest available build is 6 (8's tag vanished).
        let l = line(10);
        let n = newest(&l, &[6]);
        let err = decide("f", &pin_of(8, 8), &n, &l, &scheme()).unwrap_err();
        assert!(err.to_string().contains("downgrade"), "{err}");
    }

    #[test]
    fn off_history_pin_is_refused() {
        let l = line(10);
        let n = newest(&l, &[6]);
        let err = decide("f", "main-deadbeef", &n, &l, &scheme()).unwrap_err();
        assert!(
            err.to_string().contains("not on the first-parent history"),
            "{err}"
        );
    }

    #[test]
    fn ambiguous_current_pin_is_refused() {
        let a = format!("abcdef12{}", "1".repeat(32));
        let b = format!("abcdef12{}", "2".repeat(32));
        let l = Line::new(vec![commit(9), a, b]);
        let n = Newest {
            pin: scheme().pin_for(sha(9), digest(9)),
            position: 0,
            commit: commit(9),
        };
        assert!(matches!(
            decide("f", "main-abcdef12", &n, &l, &scheme()),
            Err(Error::Refused(_))
        ));
    }

    pub fn values_doc(name: &str, repo: &str, tag: &str) -> String {
        format!("{name}:\n  image:\n    repository: {repo}\n    tag: {tag}\n")
    }

    #[test]
    fn plan_resolves_each_image_once_and_rewrites_both_relays() {
        let releases = [release("ws", "acme/relay"), release("wt", "acme/relay")];
        let docs: Vec<_> = releases
            .iter()
            .map(|r| values_doc(&r.name, "acme/relay", &pin_of(3, 3)))
            .collect();
        let inputs: Vec<_> = releases
            .iter()
            .zip(&docs)
            .map(|(release, contents)| Input { release, contents })
            .collect();
        let fake = FakeRegistry::default().with("acme/relay", &[(3, 3), (6, 6)]);
        let rows = plan(&inputs, &line(10), &fake, &scheme()).unwrap();
        assert_eq!(fake.digest_calls.get(), 1);
        assert!(
            rows.iter()
                .all(|r| r.status == Status::Bump && r.new == pin_of(6, 6))
        );
        assert!(
            rows.iter()
                .all(|r| r.new_contents.as_ref().unwrap().contains(&pin_of(6, 6)))
        );
    }

    #[test]
    fn plan_refuses_a_wrong_repository() {
        let r = release("api", "acme/api");
        let doc = values_doc("api", "acme/relay", "latest");
        let err = plan(
            &[Input {
                release: &r,
                contents: &doc,
            }],
            &line(5),
            &FakeRegistry::default(),
            &scheme(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("expected \"acme/api\""), "{err}");
    }

    #[test]
    fn registry_errors_propagate_as_retryable() {
        let r = release("api", "acme/api");
        let doc = values_doc("api", "acme/api", "latest");
        struct Down;
        impl Registry for Down {
            fn tags(&self, _: &ImageRef) -> Result<Vec<String>> {
                Err(Error::Registry(
                    "HTTP 503 (gave up after 5 attempts)".into(),
                ))
            }
            fn digest(&self, _: &ImageRef, _: &str) -> Result<Digest> {
                unreachable!()
            }
        }
        let err = plan(
            &[Input {
                release: &r,
                contents: &doc,
            }],
            &line(5),
            &Down,
            &scheme(),
        )
        .unwrap_err();
        assert_eq!(err.exit_code(), crate::error::EXIT_ERROR);
    }

    // ---- PR text ----

    struct FakeHistory;
    impl History for FakeHistory {
        fn first_parent(&self) -> Result<Vec<String>> {
            unreachable!()
        }
        fn subjects(&self, from: &str, to: &str, max: usize) -> Result<Vec<(String, String)>> {
            assert_eq!(max, MAX_COMMITS_LISTED);
            Ok(vec![
                (to[..8].to_string(), "feat: `code` in subject (#1)".into()),
                (
                    from[..8].to_string(),
                    "never listed: from is exclusive".into(),
                ),
            ])
        }
        fn is_shallow(&self) -> Result<bool> {
            Ok(false)
        }
    }

    fn row(name: &str, current: String, new_commit: u32, status: Status) -> Row {
        Row {
            image: "acme/x".into(),
            values_file: format!("charts/{name}/values.yaml"),
            current,
            new: format!("main-{}@{}", sha(new_commit), digest(new_commit)),
            status,
            new_contents: None,
            new_commit: commit(new_commit),
            new_position: 9 - new_commit as usize,
        }
    }

    fn ctx<'a>(l: &'a Line, s: &'a TagScheme, repo: Option<&'a str>) -> BodyContext<'a> {
        BodyContext {
            line: l,
            history: &FakeHistory,
            scheme: s,
            branch: "main",
            github_repo: repo,
            note: Some("**Merging this PR deploys.**\n"),
        }
    }

    #[test]
    fn body_has_table_compare_link_and_the_note() {
        let rows = vec![
            row(
                "websocket",
                format!("main-{}@{}", sha(3), digest(3)),
                8,
                Status::Bump,
            ),
            row("website", "latest".into(), 5, Status::Bump),
            row(
                "api",
                format!("main-{}@{}", sha(8), digest(8)),
                8,
                Status::Unchanged,
            ),
        ];
        let l = line(10);
        let s = scheme();
        let text = body(&rows, &ctx(&l, &s, Some("acme/repo"))).unwrap();
        assert!(
            text.starts_with(
                "Pins the production Helm values to the newest images built from main,"
            )
        );
        assert!(text.contains(&format!(
            "| `acme/x` | `charts/websocket/values.yaml` | `main-{}@sha256:000000000000` | **`main-{}@sha256:000000000000`** |",
            sha(3),
            sha(8)
        )));
        assert!(text.contains("| `latest` | **`main-"));
        assert!(text.contains(&format!(
            "https://github.com/acme/repo/compare/{}...{}",
            commit(3),
            commit(8)
        )));
        assert!(text.contains("feat: 'code' in subject"));
        assert!(text.ends_with("</details>\n\n**Merging this PR deploys.**\n"));
        assert_eq!(
            title(&rows, &s),
            format!("deploy: pin images to main-{}", sha(8))
        );

        let text = body(&rows, &ctx(&l, &s, None)).unwrap();
        assert!(text.contains(&format!(
            "What ships: `{}..{}`",
            &commit(3)[..12],
            &commit(8)[..12]
        )));
    }

    #[test]
    fn body_without_any_build_pin_says_so() {
        let rows = vec![row("api", "latest".into(), 8, Status::Bump)];
        let l = line(10);
        let s = scheme();
        let text = body(&rows, &ctx(&l, &s, Some("o/r"))).unwrap();
        assert!(text.contains("No compare link: no current pin is a main-<sha8> build"));
    }

    #[test]
    fn tsv_columns() {
        let rows = vec![row("websocket", "latest".into(), 8, Status::Bump)];
        let out = tsv(&rows);
        assert!(out.starts_with("image\tvalues_file\tcurrent_tag\tnew_tag\tstatus\n"));
        assert!(out.ends_with("\tbump\n"));
    }
}
