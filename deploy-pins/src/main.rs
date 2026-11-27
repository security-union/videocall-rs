// SPDX-License-Identifier: MIT OR Apache-2.0

//! `deploy-pins`: keeps Helm values pinned to an exact image (tag plus
//! digest), verifies the pins before a deploy, and waits for each service to
//! report the new build. What runs where comes from `deploy-pins.toml`
//! ([`config`]); exit codes are in [`error`].

mod config;
mod error;
mod gate;
mod git;
mod pin;
mod plan;
mod registry;
mod values;
mod verify;

use clap::{Parser, Subcommand};
use config::{Config, Release};
use error::{EXIT_ERROR, EXIT_NOT_CONVERGED, EXIT_OK, Error, Result};
use git::History;
use registry::Registry;
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

/// Longest any `--*-secs` option may be: a day.
const MAX_SECS: u64 = 86_400;

/// Pin Helm values to an exact image tag and digest, verify the pins before
/// a deploy, and wait for each service to report the new build.
#[derive(Debug, Parser)]
#[command(
    name = "deploy-pins",
    version,
    after_help = "Exit codes: 0 ok, 1 not converged, 2 error (retry), 3 refused (needs a human)."
)]
struct Cli {
    /// The config file; relative paths in it resolve against its directory.
    #[arg(long, value_name = "FILE", default_value = config::FILE_NAME, global = true)]
    config: PathBuf,
    /// Most time spent on registry requests, in seconds.
    #[arg(
        long,
        value_name = "N",
        default_value_t = 240,
        value_parser = clap::value_parser!(u64).range(1..=MAX_SECS),
        global = true
    )]
    registry_budget_secs: u64,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum Command {
    /// Pin every release to the newest build of its image.
    Pin {
        /// Write nothing; exit 1 if a pin would change.
        #[arg(long)]
        check: bool,
        /// Also write the PR title.txt and body.md here.
        #[arg(long, value_name = "DIR")]
        report_dir: Option<PathBuf>,
    },
    /// Deploy preflight: every pin is on the history and is what the
    /// registry serves.
    Verify {
        /// `helm template` output as <name>.yaml; every rendered image must
        /// be exactly the pin.
        #[arg(long, value_name = "DIR")]
        rendered_dir: Option<PathBuf>,
    },
    /// Wait until the running build reports its pinned sha.
    #[command(group(clap::ArgGroup::new("which").required(true).args(["release", "all"])))]
    WaitVersion {
        /// The release to wait for.
        #[arg(long, value_name = "NAME")]
        release: Option<String>,
        /// Wait for every release with a version check.
        #[arg(long)]
        all: bool,
        /// Give up after this many seconds.
        #[arg(
            long,
            value_name = "N",
            default_value_t = 90,
            value_parser = clap::value_parser!(u64).range(0..=MAX_SECS)
        )]
        timeout_secs: u64,
    },
    /// Print the deploy order: name, Helm release, chart dir.
    Releases {
        /// Print each release with its current pin and version check as
        /// JSON, for a deploy job that runs no code from this tool.
        #[arg(long)]
        json: bool,
    },
}

/// Write to stdout. A closed pipe (`releases | head -1`) becomes an error
/// that stops the command quietly, where `print!` would panic.
fn out(text: &str) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(Error::io("stdout"))
}

/// Everything a command talks to, so tests can swap it.
struct Deps {
    registry: Box<dyn Fn() -> Result<Box<dyn Registry>>>,
    history: Box<dyn History>,
    fetch: Box<dyn gate::Fetch>,
    clock: Box<dyn gate::Clock>,
}

impl Deps {
    fn real(config: &Config, budget: Duration) -> Self {
        Deps {
            registry: Box::new(move || {
                Ok(Box::new(registry::OciRegistry::from_env(Some(budget))?))
            }),
            history: Box::new(git::GitCli::new(&config.root)),
            fetch: Box::new(gate::HttpFetch::new()),
            clock: Box::new(gate::SystemClock::new()),
        }
    }
}

/// A file to replace: its new contents and what it held when it was read.
struct Replace<'a> {
    rel: &'a str,
    new: &'a str,
    old: &'a str,
}

type Rename<'a> = dyn FnMut(&Path, &Path) -> std::io::Result<()> + 'a;

/// Write `contents` to a new temp file next to `path`, with `path`'s mode.
fn stage(path: &Path, contents: &str) -> std::io::Result<PathBuf> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.deploy-pins-{}.tmp", std::process::id()));
    let result = (|| {
        let perms = std::fs::metadata(path)?.permissions();
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        std::fs::set_permissions(&tmp, perms)
    })();
    match result {
        Ok(()) => Ok(tmp),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Replace every file or none.
///
/// Each file is replaced by an atomic rename of a fully written, synced temp
/// file with the original mode, so no file is ever half-written. Every temp
/// file is written before the first rename, and a file that changed since it
/// was read stops the run. If a rename fails part way, the files already
/// replaced get their old contents back the same way. Only if that restore
/// fails too (the disk went away) are files left changed: that is a refusal
/// (exit 3) naming them, because a person has to look.
fn write_all(root: &Path, files: &[Replace<'_>], rename: &mut Rename<'_>) -> Result<()> {
    let mut staged: Vec<(PathBuf, PathBuf, &Replace<'_>)> = Vec::new();
    let cleanup = |staged: &[(PathBuf, PathBuf, &Replace<'_>)]| {
        for (tmp, _, _) in staged {
            let _ = std::fs::remove_file(tmp);
        }
    };
    for file in files {
        let path = root.join(file.rel);
        let current = std::fs::symlink_metadata(&path).and_then(|meta| {
            if meta.file_type().is_file() {
                std::fs::read_to_string(&path).map(Some)
            } else {
                Ok(None)
            }
        });
        let ready = match current {
            Err(e) => Err(Error::io(&path)(e)),
            Ok(None) => Err(Error::Refused(format!(
                "{} is not a regular file (a symlink?); nothing written",
                file.rel
            ))),
            Ok(Some(current)) if current != file.old => Err(Error::Refused(format!(
                "{} changed while deploy-pins was running; nothing written",
                file.rel
            ))),
            Ok(_) => stage(&path, file.new).map_err(Error::io(&path)),
        };
        match ready {
            Ok(tmp) => staged.push((tmp, path, file)),
            Err(e) => {
                cleanup(&staged);
                return Err(e);
            }
        }
    }
    for i in 0..staged.len() {
        let (tmp, path, file) = &staged[i];
        if let Err(source) = rename(tmp, path) {
            cleanup(&staged[i..]);
            let mut left_changed = Vec::new();
            for (_, done_path, done) in &staged[..i] {
                let restored = stage(done_path, done.old).and_then(|tmp| {
                    rename(&tmp, done_path).inspect_err(|_| {
                        let _ = std::fs::remove_file(&tmp);
                    })
                });
                if restored.is_err() {
                    left_changed.push(done.rel);
                }
            }
            if left_changed.is_empty() {
                return Err(Error::io(path)(source));
            }
            return Err(Error::Refused(format!(
                "replacing {} failed ({source}) and {} could not be restored: they hold the \
                 new pins, the rest the old ones",
                file.rel,
                left_changed.join(", ")
            )));
        }
    }
    Ok(())
}

fn cmd_pin(config: &Config, deps: &Deps, check: bool, report_dir: Option<&Path>) -> Result<u8> {
    let line = git::line(deps.history.as_ref())?;
    eprintln!(
        "searching the first-parent history from HEAD ({})",
        &line.tip()[..8]
    );
    let contents: Vec<String> = config
        .releases
        .iter()
        .map(|r| config.read(&r.values_file))
        .collect::<Result<_>>()?;
    let inputs: Vec<_> = config
        .releases
        .iter()
        .zip(&contents)
        .map(|(release, contents)| plan::Input { release, contents })
        .collect();
    let registry = (deps.registry)()?;
    let rows = plan::plan(&inputs, &line, registry.as_ref(), &config.scheme)?;
    out(&plan::tsv(&rows))?;
    if let Some(dir) = report_dir {
        std::fs::create_dir_all(dir).map_err(Error::io(dir))?;
        let ctx = plan::BodyContext {
            line: &line,
            history: deps.history.as_ref(),
            scheme: &config.scheme,
            branch: &config.branch,
            github_repo: config.github_repo.as_deref(),
            note: config.pr_note.as_deref(),
        };
        let title = format!("{}\n", plan::title(&rows, &config.scheme));
        for (name, text) in [("title.txt", title), ("body.md", plan::body(&rows, &ctx)?)] {
            let path = dir.join(name);
            std::fs::write(&path, text).map_err(Error::io(path))?;
        }
    }
    let bumps: Vec<Replace<'_>> = rows
        .iter()
        .zip(&contents)
        .filter_map(|(row, old)| {
            let new = row.new_contents.as_deref()?;
            Some(Replace {
                rel: &row.values_file,
                new,
                old,
            })
        })
        .collect();
    if bumps.is_empty() {
        eprintln!("all pins current");
        return Ok(EXIT_OK);
    }
    if check {
        eprintln!(
            "{} pin(s) out of date (--check: nothing written)",
            bumps.len()
        );
        return Ok(EXIT_NOT_CONVERGED);
    }
    write_all(&config.root, &bumps, &mut |from, to| {
        std::fs::rename(from, to)
    })?;
    for row in rows.iter().filter(|r| r.new_contents.is_some()) {
        eprintln!("{}: {} -> {}", row.values_file, row.current, row.new);
    }
    Ok(EXIT_OK)
}

fn cmd_verify(config: &Config, deps: &Deps, rendered_dir: Option<&Path>) -> Result<u8> {
    let line = git::line(deps.history.as_ref())?;
    let registry = (deps.registry)()?;
    let mut failures = Vec::new();
    for release in &config.releases {
        let outcome = (|| {
            let values = config.read(&release.values_file)?;
            let rendered = match rendered_dir {
                Some(dir) => {
                    let path = dir.join(format!("{}.yaml", release.name));
                    Some(std::fs::read_to_string(&path).map_err(Error::io(path))?)
                }
                None => None,
            };
            verify::check(
                release,
                &values,
                rendered.as_deref(),
                &line,
                registry.as_ref(),
                &config.scheme,
            )
        })();
        match outcome {
            Ok(pin) => out(&format!(
                "{}\t{}\t{}:{pin}\tok\n",
                release.name, release.helm_release, release.image
            ))?,
            Err(e) => {
                eprintln!("error: {e}");
                failures.push(e);
            }
        }
    }
    // Each failure was printed above. A refusal outranks a retryable error:
    // retrying will not fix it.
    let total = config.releases.len();
    let refused = failures
        .iter()
        .filter(|e| e.exit_code() == error::EXIT_REFUSED)
        .count();
    if failures.is_empty() {
        Ok(EXIT_OK)
    } else if refused > 0 {
        Err(Error::Refused(format!(
            "{refused} of {total} releases failed preflight; nothing may deploy"
        )))
    } else {
        Err(Error::Registry(format!(
            "{} of {total} releases could not be checked; retry",
            failures.len()
        )))
    }
}

/// The full pin a release's values file holds now.
fn current_pin(config: &Config, release: &Release) -> Result<pin::Pin> {
    let file = &release.values_file;
    let block = values::read_image(file, &config.read(file)?, &release.image_key)?;
    if block.repository != release.image.written {
        return Err(Error::Refused(format!(
            "{file}: repository {:?} is not the configured {:?}",
            block.repository, release.image.written
        )));
    }
    config
        .scheme
        .pin(&block.tag)
        .ok_or_else(|| Error::Refused(format!("{file}: {:?} is not a full pin", block.tag)))
}

/// `releases --json`: everything a deploy job needs, as data.
fn releases_json(config: &Config) -> Result<String> {
    let mut releases = Vec::new();
    for release in &config.releases {
        let pin = current_pin(config, release)?;
        let version = release.version.as_ref().map(|v| {
            json!({
                "url": v.url,
                "pointer": v.pointer,
                "select": v.select.as_ref().map(|s| json!({
                    "array": s.array,
                    "field": s.field,
                    "equals": s.equals,
                })),
            })
        });
        releases.push(json!({
            "name": release.name,
            "helm_release": release.helm_release,
            "chart_dir": release.chart_dir,
            "values_file": release.values_file,
            "repository": release.image.written,
            "pin": pin.to_string(),
            "sha": pin.sha.as_str(),
            "version": version,
        }));
    }
    let text = serde_json::to_string_pretty(&json!({ "releases": releases }))
        .map_err(|e| Error::Refused(e.to_string()))?;
    Ok(text + "\n")
}

fn cmd_wait(config: &Config, deps: &Deps, only: Option<&str>, timeout: u64) -> Result<u8> {
    let releases: Vec<&Release> = match only {
        None => config.releases.iter().collect(),
        Some(name) => vec![
            config
                .release(name)
                .ok_or_else(|| Error::Refused(format!("unknown release {name:?}")))?,
        ],
    };
    let mut targets = Vec::new();
    for release in releases {
        let Some(check) = &release.version else {
            eprintln!(
                "{}: no version check; only the rollout is checked",
                release.name
            );
            continue;
        };
        targets.push(gate::Target {
            label: release.name.clone(),
            check: check.clone(),
            expected: current_pin(config, release)?.sha,
        });
    }
    if targets.is_empty() {
        return Ok(EXIT_OK);
    }
    let outcome = gate::wait(
        &targets,
        Duration::from_secs(timeout),
        gate::INTERVAL,
        deps.fetch.as_ref(),
        deps.clock.as_ref(),
    );
    match outcome {
        gate::Outcome::Converged => {
            eprintln!("every target reports its pinned sha");
            Ok(EXIT_OK)
        }
        gate::Outcome::TimedOut(lagging) => {
            for (label, seen) in lagging {
                eprintln!("error: {label} still reports {seen} after {timeout}s");
            }
            Ok(EXIT_NOT_CONVERGED)
        }
    }
}

fn execute(command: Command, config: &Config, deps: &Deps) -> Result<u8> {
    match command {
        Command::Pin { check, report_dir } => cmd_pin(config, deps, check, report_dir.as_deref()),
        Command::Verify { rendered_dir } => cmd_verify(config, deps, rendered_dir.as_deref()),
        Command::WaitVersion {
            release,
            timeout_secs,
            ..
        } => cmd_wait(config, deps, release.as_deref(), timeout_secs),
        Command::Releases { json: true } => out(&releases_json(config)?).map(|()| EXIT_OK),
        Command::Releases { json: false } => {
            let mut text = String::new();
            for r in &config.releases {
                text.push_str(&format!(
                    "{}\t{}\t{}\n",
                    r.name, r.helm_release, r.chart_dir
                ));
            }
            out(&text).map(|()| EXIT_OK)
        }
    }
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            // Help and version go to stdout with exit 0, usage errors to
            // stderr with exit 2. Neither may panic on a closed pipe.
            let _ = e.print();
            return ExitCode::from(if e.use_stderr() { EXIT_ERROR } else { EXIT_OK });
        }
    };
    let budget = Duration::from_secs(cli.registry_budget_secs);
    let result = config::load(&cli.config).and_then(|config| {
        let deps = Deps::real(&config, budget);
        execute(cli.command, &config, &deps)
    });
    match result {
        Ok(code) => ExitCode::from(code),
        Err(e) if e.is_broken_pipe() => ExitCode::from(EXIT_ERROR),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(e.exit_code())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::{TestDir, project, write_config};
    use crate::error::EXIT_REFUSED;
    use crate::plan::tests::{FakeRegistry, commit, pin_of};
    use clap::CommandFactory;
    use std::cell::Cell;
    use std::os::unix::fs::PermissionsExt;

    const CONFIG: &str = r#"
branch = "main"

[[release]]
name = "api"
helm_release = "api-prod"
chart_dir = "charts/app"
image_key = "app.image"
repository = "acme/app"

[release.version]
url = "https://app.example.com/version.json"
pointer = "/git_sha"
"#;

    fn parse(args: &[&str]) -> std::result::Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("deploy-pins").chain(args.iter().copied()))
    }

    #[test]
    fn the_command_line_keeps_its_defaults_and_refuses_the_rest() {
        Cli::command().debug_assert();
        let cli = parse(&["pin", "--check", "--report-dir", "out"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Pin {
                check: true,
                report_dir: Some("out".into())
            }
        );
        assert_eq!(cli.config, PathBuf::from("deploy-pins.toml"));
        assert_eq!(cli.registry_budget_secs, 240);
        // Global options go before or after the command.
        let cli = parse(&[
            "verify",
            "--config",
            "x.toml",
            "--registry-budget-secs",
            "60",
        ])
        .unwrap();
        assert_eq!(
            (cli.config, cli.registry_budget_secs),
            ("x.toml".into(), 60)
        );
        let wait = parse(&["wait-version", "--all"]).unwrap().command;
        assert!(matches!(
            wait,
            Command::WaitVersion {
                timeout_secs: 90,
                all: true,
                ..
            }
        ));
        assert_eq!(
            parse(&["releases", "--json"]).unwrap().command,
            Command::Releases { json: true }
        );
        for bad in [
            &[][..],
            &["deploy"],
            // A flag is never taken as a value: this was --report-dir="--check".
            &["pin", "--report-dir", "--check"],
            &["pin", "--rendered-dir", "x"],
            &["--check", "pin"],
            &["wait-version"],
            &["wait-version", "--all", "--release", "x"],
            &["wait-version", "--all", "--timeout-secs", "soon"],
            &["wait-version", "--all", "--timeout-secs", "86401"],
            &["--registry-budget-secs", "18446744073709551615", "releases"],
            &["--registry-budget-secs", "0", "releases"],
            &["pin", "verify"],
        ] {
            let err = parse(bad).map(|c| c.command).unwrap_err();
            assert_eq!(err.exit_code(), i32::from(EXIT_ERROR), "{bad:?}");
            assert!(err.use_stderr(), "{bad:?}");
        }
        let help = parse(&["--help"]).unwrap_err();
        assert!(!help.use_stderr() && help.exit_code() == 0);
    }

    struct FixedHistory {
        commits: Vec<String>,
        shallow: bool,
    }
    impl History for FixedHistory {
        fn first_parent(&self) -> Result<Vec<String>> {
            Ok(self.commits.clone())
        }
        fn subjects(&self, _: &str, _: &str, _: usize) -> Result<Vec<(String, String)>> {
            Ok(vec![])
        }
        fn is_shallow(&self) -> Result<bool> {
            Ok(self.shallow)
        }
    }

    struct ConstFetch(String);
    impl gate::Fetch for ConstFetch {
        fn get(&self, _: &str) -> std::result::Result<String, String> {
            Ok(self.0.clone())
        }
    }

    struct FakeClock(Cell<Duration>);
    impl gate::Clock for FakeClock {
        fn now(&self) -> Duration {
            self.0.get()
        }
        fn sleep(&self, d: Duration) {
            self.0.set(self.0.get() + d);
        }
    }

    /// Commits 0..10 (tip 9); the registry comes from `make_registry`.
    fn deps(make_registry: fn() -> FakeRegistry, reported: &str, shallow: bool) -> Deps {
        Deps {
            registry: Box::new(move || Ok(Box::new(make_registry()))),
            history: Box::new(FixedHistory {
                commits: (0..10).rev().map(commit).collect(),
                shallow,
            }),
            fetch: Box::new(ConstFetch(format!(r#"{{"git_sha":"{reported}"}}"#))),
            clock: Box::new(FakeClock(Cell::new(Duration::ZERO))),
        }
    }

    fn builds() -> FakeRegistry {
        FakeRegistry::default().with("acme/app", &[(3, 3), (6, 6)])
    }

    fn re_pushed() -> FakeRegistry {
        FakeRegistry::default().with("acme/app", &[(3, 3), (6, 66)])
    }

    fn gone() -> FakeRegistry {
        FakeRegistry::default().with("acme/app", &[(3, 3)])
    }

    fn down() -> Deps {
        struct Down;
        impl Registry for Down {
            fn tags(&self, _: &registry::ImageRef) -> Result<Vec<String>> {
                Err(Error::Registry(
                    "HTTP 503 (gave up after 5 attempts)".into(),
                ))
            }
            fn digest(&self, _: &registry::ImageRef, _: &str) -> Result<pin::Digest> {
                Err(Error::Registry(
                    "HTTP 503 (gave up after 5 attempts)".into(),
                ))
            }
        }
        Deps {
            registry: Box::new(|| Ok(Box::new(Down))),
            ..deps(builds, "", false)
        }
    }

    fn values(tag: &str) -> String {
        format!("app:\n  image:\n    repository: acme/app\n    tag: {tag}\n")
    }

    /// Run a command line against a project holding `tag`; returns the exit
    /// code and the values file afterwards.
    fn exit_code(tag: &str, args: &[&str], deps: Deps) -> (u8, String) {
        let dir = project(&values(tag));
        let config = config::load(&write_config(&dir, CONFIG)).unwrap();
        let cli = parse(args).unwrap();
        let code = match execute(cli.command, &config, &deps) {
            Ok(code) => code,
            Err(e) => e.exit_code(),
        };
        let after = std::fs::read_to_string(dir.0.join("charts/app/values.yaml")).unwrap();
        (code, after)
    }

    #[test]
    fn pin_exit_codes() {
        let (current, stale) = (pin_of(6, 6), pin_of(3, 3));
        let d = || deps(builds, "", false);
        assert_eq!(exit_code(&current, &["pin", "--check"], d()).0, EXIT_OK);
        let (code, after) = exit_code(&stale, &["pin", "--check"], d());
        assert_eq!(
            (code, after),
            (EXIT_NOT_CONVERGED, values(&stale)),
            "--check writes nothing"
        );
        assert_eq!(
            exit_code(&stale, &["pin"], d()),
            (EXIT_OK, values(&current))
        );
        assert_eq!(
            exit_code(&current, &["pin"], deps(re_pushed, "", false)),
            (EXIT_REFUSED, values(&current))
        );
        assert_eq!(
            exit_code(&pin_of(8, 8), &["pin", "--check"], d()).0,
            EXIT_REFUSED
        );
        assert_eq!(exit_code(&stale, &["pin", "--check"], down()).0, EXIT_ERROR);
        let shallow = deps(builds, "", true);
        assert_eq!(
            exit_code(&stale, &["pin", "--check"], shallow).0,
            EXIT_ERROR
        );
    }

    #[test]
    fn verify_exit_codes() {
        let current = pin_of(6, 6);
        let d = |r| deps(r, "", false);
        assert_eq!(exit_code(&current, &["verify"], d(builds)).0, EXIT_OK);
        assert_eq!(
            exit_code(&current, &["verify"], d(re_pushed)).0,
            EXIT_REFUSED
        );
        assert_eq!(exit_code(&current, &["verify"], d(gone)).0, EXIT_REFUSED);
        assert_eq!(exit_code("latest", &["verify"], d(builds)).0, EXIT_REFUSED);
        assert_eq!(exit_code(&current, &["verify"], down()).0, EXIT_ERROR);
        let no_render = ["verify", "--rendered-dir", "/nonexistent-render-dir"];
        assert_eq!(exit_code(&current, &no_render, d(builds)).0, EXIT_ERROR);
    }

    #[test]
    fn wait_version_exit_codes() {
        let current = pin_of(6, 6);
        let sha8 = |n| commit(n)[..8].to_string();
        let one = ["wait-version", "--release", "api", "--timeout-secs", "10"];
        assert_eq!(
            exit_code(&current, &one, deps(builds, &sha8(6), false)).0,
            EXIT_OK
        );
        assert_eq!(
            exit_code(&current, &one, deps(builds, &sha8(3), false)).0,
            EXIT_NOT_CONVERGED
        );
        let all = ["wait-version", "--all", "--timeout-secs", "0"];
        assert_eq!(
            exit_code(&current, &all, deps(builds, &commit(6), false)).0,
            EXIT_OK
        );
        let unknown = ["wait-version", "--release", "nope"];
        assert_eq!(
            exit_code(&current, &unknown, deps(builds, "", false)).0,
            EXIT_REFUSED
        );
        assert_eq!(
            exit_code("latest", &one, deps(builds, &sha8(6), false)).0,
            EXIT_REFUSED
        );
    }

    #[test]
    fn releases_json_is_the_plan_a_deploy_job_reads() {
        let current = pin_of(6, 6);
        let dir = project(&values(&current));
        let config = config::load(&write_config(&dir, CONFIG)).unwrap();
        let plan: serde_json::Value =
            serde_json::from_str(&releases_json(&config).unwrap()).unwrap();
        assert_eq!(
            plan,
            json!({"releases": [{
                "name": "api",
                "helm_release": "api-prod",
                "chart_dir": "charts/app",
                "values_file": "charts/app/values.yaml",
                "repository": "acme/app",
                "pin": current,
                "sha": &commit(6)[..8],
                "version": {"url": "https://app.example.com/version.json", "pointer": "/git_sha", "select": null},
            }]})
        );
        // Only full pins, and only the configured repository.
        let (code, _) = exit_code("latest", &["releases", "--json"], deps(builds, "", false));
        assert_eq!(code, EXIT_REFUSED);
        let dir = project(&values(&current).replace("acme/app", "acme/other"));
        let config = config::load(&write_config(
            &dir,
            &CONFIG.replace("acme/app", "acme/other"),
        ))
        .unwrap();
        assert!(releases_json(&config).is_ok());
        std::fs::write(dir.0.join("charts/app/values.yaml"), values(&current)).unwrap();
        assert_eq!(
            releases_json(&config).unwrap_err().exit_code(),
            EXIT_REFUSED
        );
    }

    fn three_files() -> (TestDir, Vec<(String, String, String)>) {
        let dir = TestDir::new();
        let mut files = Vec::new();
        for (i, mode) in [0o644, 0o600, 0o664].into_iter().enumerate() {
            let rel = format!("f{i}.yaml");
            let path = dir.0.join(&rel);
            std::fs::write(&path, format!("old {i}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            files.push((rel, format!("new {i}\n"), format!("old {i}\n")));
        }
        (dir, files)
    }

    fn replaces(files: &[(String, String, String)]) -> Vec<Replace<'_>> {
        files
            .iter()
            .map(|(rel, new, old)| Replace { rel, new, old })
            .collect()
    }

    fn contents(dir: &Path) -> Vec<String> {
        (0..3)
            .map(|i| std::fs::read_to_string(dir.join(format!("f{i}.yaml"))).unwrap())
            .collect()
    }

    fn entries(dir: &Path) -> usize {
        std::fs::read_dir(dir).unwrap().count()
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn rename(from: &Path, to: &Path) -> std::io::Result<()> {
        std::fs::rename(from, to)
    }

    #[test]
    fn write_all_replaces_every_file_and_keeps_modes() {
        let (dir, files) = three_files();
        write_all(&dir.0, &replaces(&files), &mut rename).unwrap();
        assert_eq!(contents(&dir.0), ["new 0\n", "new 1\n", "new 2\n"]);
        assert_eq!(mode(&dir.0.join("f1.yaml")), 0o600);
        assert_eq!(entries(&dir.0), 3, "no temp files left");
    }

    #[test]
    fn write_all_restores_earlier_files_when_a_later_rename_fails() {
        let (dir, files) = three_files();
        let mut calls = 0;
        let mut flaky = |from: &Path, to: &Path| {
            calls += 1;
            if calls == 2 {
                Err(std::io::Error::other("disk full"))
            } else {
                rename(from, to)
            }
        };
        let err = write_all(&dir.0, &replaces(&files), &mut flaky).unwrap_err();
        assert!(err.to_string().contains("disk full"), "{err}");
        assert_eq!(contents(&dir.0), ["old 0\n", "old 1\n", "old 2\n"]);
        assert_eq!(mode(&dir.0.join("f0.yaml")), 0o644);
        assert_eq!(entries(&dir.0), 3, "no temp files left");
    }

    #[test]
    fn write_all_refuses_and_names_the_files_it_could_not_restore() {
        let (dir, files) = three_files();
        let mut calls = 0;
        // f0 is replaced, f1 fails, and putting f0 back fails too.
        let mut broken = |from: &Path, to: &Path| {
            calls += 1;
            if calls >= 2 {
                Err(std::io::Error::other("disk gone"))
            } else {
                rename(from, to)
            }
        };
        let err = write_all(&dir.0, &replaces(&files), &mut broken).unwrap_err();
        assert_eq!(err.exit_code(), EXIT_REFUSED, "{err}");
        let message = err.to_string();
        assert!(message.contains("f1.yaml failed (disk gone)"), "{message}");
        assert!(
            message.contains("f0.yaml could not be restored"),
            "{message}"
        );
        assert_eq!(contents(&dir.0), ["new 0\n", "old 1\n", "old 2\n"]);
        assert_eq!(entries(&dir.0), 3, "no temp files left");
    }

    #[test]
    fn write_all_writes_nothing_when_staging_fails_or_a_file_moved_on() {
        let (dir, mut files) = three_files();
        files.push(("missing/f.yaml".into(), "x".into(), "y".into()));
        assert!(write_all(&dir.0, &replaces(&files), &mut rename).is_err());
        assert_eq!(contents(&dir.0), ["old 0\n", "old 1\n", "old 2\n"]);
        assert_eq!(entries(&dir.0), 3, "no temp files left");

        let (dir, files) = three_files();
        let target = dir.0.join("elsewhere.yaml");
        std::fs::rename(dir.0.join("f2.yaml"), &target).unwrap();
        std::os::unix::fs::symlink(&target, dir.0.join("f2.yaml")).unwrap();
        let err = write_all(&dir.0, &replaces(&files), &mut rename).unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old 2\n");
        assert_eq!(contents(&dir.0)[0], "old 0\n");

        let (dir, files) = three_files();
        std::fs::write(dir.0.join("f2.yaml"), "edited meanwhile\n").unwrap();
        let err = write_all(&dir.0, &replaces(&files), &mut rename).unwrap_err();
        assert_eq!(err.exit_code(), EXIT_REFUSED);
        assert_eq!(contents(&dir.0)[0], "old 0\n");
        assert_eq!(entries(&dir.0), 3, "no temp files left");
    }
}
