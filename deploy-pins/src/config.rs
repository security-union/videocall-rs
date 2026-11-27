// SPDX-License-Identifier: MIT OR Apache-2.0

//! `deploy-pins.toml`: which Helm release runs which image, in deploy order.
//!
//! Everything project-specific lives in that file. Paths in it are relative
//! to its own directory, which must be inside the git checkout. It is checked
//! as it loads: unknown keys, wrong types, duplicate names, a missing chart
//! or values file, or an image key that is not in the values file all stop
//! the run with exit code 3.

use crate::error::{Error, Result};
use crate::pin::TagScheme;
use crate::registry::ImageRef;
use crate::values;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use toml::{Table, Value};

pub const FILE_NAME: &str = "deploy-pins.toml";

/// Where a running build reports its commit, and how to read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionCheck {
    /// Endpoint to GET. A cache-busting `t=<unix time>` query is added.
    pub url: String,
    /// JSON pointer (RFC 6901) to the sha: from the root, or from the
    /// element `select` picks.
    pub pointer: String,
    /// `array[field == equals]`; exactly one element must match.
    pub select: Option<Select>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Select {
    pub array: String,
    pub field: String,
    pub equals: String,
}

/// One Helm release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub name: String,
    pub helm_release: String,
    pub chart_dir: String,
    pub values_file: String,
    /// Key path of the image block; `repository` and `tag` live under it.
    pub image_key: Vec<String>,
    /// The only repository this release may pull.
    pub image: ImageRef,
    pub version: Option<VersionCheck>,
}

impl Release {
    pub fn image_key_display(&self) -> String {
        self.image_key.join(".")
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// The config file's directory; relative paths resolve against it.
    pub root: PathBuf,
    /// Branch the builds come from, as it appears in tags.
    pub branch: String,
    pub scheme: TagScheme,
    /// `owner/name` on GitHub, for compare links in the PR body.
    pub github_repo: Option<String>,
    /// Appended to the PR body.
    pub pr_note: Option<String>,
    /// In deploy order.
    pub releases: Vec<Release>,
}

impl Config {
    pub fn release(&self, name: &str) -> Option<&Release> {
        self.releases.iter().find(|r| r.name == name)
    }

    /// Read a file named relative to the config's directory.
    pub fn read(&self, rel: &str) -> Result<String> {
        let path = self.root.join(rel);
        std::fs::read_to_string(&path).map_err(Error::io(path))
    }
}

/// A TOML table with its allowed keys checked, for precise errors.
struct Fields<'a> {
    table: &'a Table,
    at: String,
}

impl<'a> Fields<'a> {
    fn new(value: &'a Value, at: String, allowed: &[&str]) -> Result<Self> {
        let table = value
            .as_table()
            .ok_or_else(|| Error::Config(format!("{at} must be a table")))?;
        if let Some(key) = table.keys().find(|k| !allowed.contains(&k.as_str())) {
            return Err(Error::Config(format!(
                "{at}: unknown key {key:?} (allowed: {})",
                allowed.join(", ")
            )));
        }
        Ok(Fields { table, at })
    }

    fn err(&self, message: impl std::fmt::Display) -> Error {
        Error::Config(format!("{}: {message}", self.at))
    }

    fn opt(&self, key: &str) -> Result<Option<&'a str>> {
        match self.table.get(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s)),
            Some(_) => Err(self.err(format!("{key} must be a string"))),
        }
    }

    fn req(&self, key: &str) -> Result<&'a str> {
        self.opt(key)?
            .filter(|s| !s.is_empty())
            .ok_or_else(|| self.err(format!("{key} is required")))
    }
}

/// `owner/name`.
pub fn check_github_repo(repo: &str) -> bool {
    let ok = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    };
    matches!(repo.split_once('/'), Some((owner, name)) if ok(owner) && ok(name))
}

/// A path a deploy job can pass to helm without quoting surprises:
/// `[A-Za-z0-9._/-]`, relative, no `..`.
fn plain_relative(rel: &str) -> bool {
    !rel.is_empty()
        && rel
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b))
        && Path::new(rel)
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

/// `rel` exists under `root` with no symlink on the way, and resolves inside
/// it. A symlink could point a chart or a values file anywhere, and a pin
/// written through one would land outside the checkout.
fn no_symlinks(root: &Path, rel: &str) -> std::result::Result<std::fs::Metadata, String> {
    let mut at = root.to_path_buf();
    let mut meta = None;
    for part in Path::new(rel).components() {
        at.push(part);
        let m = std::fs::symlink_metadata(&at).map_err(|e| format!("{}: {e}", at.display()))?;
        if m.file_type().is_symlink() {
            return Err(format!("{} is a symlink", at.display()));
        }
        meta = Some(m);
    }
    let real = |p: &Path| {
        p.canonicalize()
            .map_err(|e| format!("{}: {e}", p.display()))
    };
    if !real(&at)?.starts_with(real(root)?) {
        return Err(format!(
            "{} resolves outside {}",
            at.display(),
            root.display()
        ));
    }
    meta.ok_or_else(|| "empty path".to_string())
}

fn version(value: &Value, at: &str) -> Result<VersionCheck> {
    let at = format!("{at}.version");
    let f = Fields::new(value, at.clone(), &["url", "pointer", "select"])?;
    let url = f.req("url")?;
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Err(f.err(format!("url {url:?} is not http(s)")));
    }
    let pointer_ok = |p: &str| p.is_empty() || p.starts_with('/');
    let pointer = f.opt("pointer")?.unwrap_or_default();
    let select = match f.table.get("select") {
        None => None,
        Some(v) => {
            let s = Fields::new(v, format!("{at}.select"), &["array", "field", "equals"])?;
            Some(Select {
                array: s.opt("array")?.unwrap_or_default().to_string(),
                field: s.req("field")?.to_string(),
                equals: s.req("equals")?.to_string(),
            })
        }
    };
    if !pointer_ok(pointer) || select.as_ref().is_some_and(|s| !pointer_ok(&s.array)) {
        return Err(f.err("pointers must be JSON pointers (empty or starting with '/')"));
    }
    Ok(VersionCheck {
        url: url.to_string(),
        pointer: pointer.to_string(),
        select,
    })
}

fn release(value: &Value, index: usize, root: &Path) -> Result<Release> {
    let allowed = [
        "name",
        "helm_release",
        "chart_dir",
        "values_file",
        "image_key",
        "repository",
        "version",
    ];
    let f = Fields::new(value, format!("release #{}", index + 1), &allowed)?;
    let name = f.req("name")?;
    let f = Fields::new(value, format!("release {name:?}"), &allowed)?;
    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_.".contains(&b))
        || name.starts_with('.')
    {
        return Err(f.err("name may only hold a-z, 0-9, '-', '_', '.'"));
    }
    let helm_release = f.req("helm_release")?;
    // Helm's rule, which also keeps it inside [A-Za-z0-9._/-].
    let helm_ok = helm_release.len() <= 53
        && helm_release
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !helm_release.starts_with('-')
        && !helm_release.ends_with('-');
    if !helm_ok {
        return Err(f.err(format!(
            "helm_release {helm_release:?} is not a valid Helm release name"
        )));
    }
    let chart_dir = f.req("chart_dir")?.trim_end_matches('/').to_string();
    let values_file = match f.opt("values_file")? {
        Some(file) => file.to_string(),
        None => format!("{chart_dir}/values.yaml"),
    };
    for (what, rel, dir) in [
        ("chart_dir", &chart_dir, true),
        ("values_file", &values_file, false),
    ] {
        if !plain_relative(rel) {
            return Err(f.err(format!(
                "{what} {rel:?} must be a relative path of A-Z a-z 0-9 . _ / - without '..'"
            )));
        }
        let meta = no_symlinks(root, rel).map_err(|e| f.err(format!("{what} {rel:?}: {e}")))?;
        if meta.is_dir() != dir || !(meta.is_dir() || meta.is_file()) {
            let kind = if dir { "directory" } else { "regular file" };
            return Err(f.err(format!("{what} {rel:?} is not a {kind}")));
        }
    }
    if !root.join(&chart_dir).join("Chart.yaml").is_file() {
        return Err(f.err(format!("chart_dir {chart_dir:?} has no Chart.yaml")));
    }
    let src = std::fs::read_to_string(root.join(&values_file))
        .map_err(|e| f.err(format!("values_file {values_file:?}: {e}")))?;
    let image_key: Vec<String> = f.req("image_key")?.split('.').map(String::from).collect();
    if image_key.iter().any(String::is_empty) {
        return Err(f.err("image_key is not a dotted key path"));
    }
    // The key path must lead to an image block with a repository and a tag.
    values::read_image(&values_file, &src, &image_key).map_err(|e| f.err(e))?;
    let image = ImageRef::parse(f.req("repository")?).map_err(|e| f.err(e))?;
    let version = match f.table.get("version") {
        Some(v) => Some(version(v, &f.at)?),
        None => None,
    };
    Ok(Release {
        name: name.to_string(),
        helm_release: helm_release.to_string(),
        chart_dir,
        values_file,
        image_key,
        image,
        version,
    })
}

/// Load and check a config file.
pub fn load(path: &Path) -> Result<Config> {
    let at = |m: String| Error::Config(format!("{}: {m}", path.display()));
    let text = std::fs::read_to_string(path).map_err(|e| at(format!("cannot read: {e}")))?;
    let doc: Value = text
        .parse::<Table>()
        .map(Value::Table)
        .map_err(|e| at(e.to_string()))?;
    let allowed = [
        "branch",
        "tag",
        "sha_length",
        "github_repo",
        "pr_note",
        "release",
    ];
    let top = Fields::new(&doc, path.display().to_string(), &allowed)?;
    let root = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let branch = top.req("branch")?;
    let sha_length = match top.table.get("sha_length") {
        None => 8,
        Some(Value::Integer(n)) => usize::try_from(*n).unwrap_or(0),
        Some(_) => return Err(top.err("sha_length must be an integer")),
    };
    let template = top.opt("tag")?.unwrap_or("{branch}-{sha}");
    let scheme = TagScheme::new(template, branch, sha_length).map_err(|e| top.err(e))?;
    let github_repo = top.opt("github_repo")?;
    if let Some(repo) = github_repo
        && !check_github_repo(repo)
    {
        return Err(top.err(format!("github_repo {repo:?} is not owner/name")));
    }
    let entries = match top.table.get("release") {
        Some(Value::Array(entries)) if !entries.is_empty() => entries,
        _ => return Err(top.err("needs at least one [[release]]")),
    };
    let mut releases = Vec::new();
    let (mut names, mut helm_names) = (HashSet::new(), HashSet::new());
    for (i, entry) in entries.iter().enumerate() {
        let r = release(entry, i, &root)?;
        if !names.insert(r.name.clone()) {
            return Err(top.err(format!("release {:?}: duplicate name", r.name)));
        }
        if !helm_names.insert(r.helm_release.clone()) {
            return Err(top.err(format!(
                "release {:?}: duplicate helm_release {:?}",
                r.name, r.helm_release
            )));
        }
        releases.push(r);
    }
    Ok(Config {
        root,
        branch: branch.to_string(),
        scheme,
        github_repo: github_repo.map(String::from),
        pr_note: top.opt("pr_note")?.map(String::from),
        releases,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// This repository's own config.
    pub fn repo_config() -> Config {
        load(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join(FILE_NAME),
        )
        .unwrap()
    }

    /// A directory removed on drop.
    #[derive(Debug)]
    pub struct TestDir(pub PathBuf);

    impl TestDir {
        pub fn new() -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "deploy-pins-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            TestDir(dir)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A throwaway project: one chart with a values file.
    pub fn project(values: &str) -> TestDir {
        let dir = TestDir::new();
        std::fs::create_dir_all(dir.0.join("charts/app")).unwrap();
        std::fs::write(dir.0.join("charts/app/Chart.yaml"), "name: app\n").unwrap();
        std::fs::write(dir.0.join("charts/app/values.yaml"), values).unwrap();
        dir
    }

    pub fn write_config(dir: &TestDir, text: &str) -> PathBuf {
        let path = dir.0.join(FILE_NAME);
        std::fs::write(&path, text).unwrap();
        path
    }

    pub const APP_VALUES: &str = "app:\n  image:\n    repository: acme/app\n    tag: latest\n";

    pub const APP_CONFIG: &str = r#"
branch = "main"

[[release]]
name = "app"
helm_release = "app-prod"
chart_dir = "charts/app"
image_key = "app.image"
repository = "acme/app"

[release.version]
url = "https://app.example.com/version.json"
pointer = "/git_sha"
"#;

    #[test]
    fn this_repos_config_loads_in_deploy_order() {
        let config = repo_config();
        let order: Vec<_> = config.releases.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            order,
            [
                "meeting-api",
                "webtransport",
                "websocket",
                "videocall-ui",
                "videocall-website"
            ]
        );
        assert_eq!(config.scheme.placeholder(), "main-<sha8>");
        assert!(
            config
                .releases
                .iter()
                .all(|r| r.image.path.starts_with("securityunion/"))
        );
    }

    #[test]
    fn a_minimal_config_loads_with_defaults() {
        let dir = project(APP_VALUES);
        let config = load(&write_config(&dir, APP_CONFIG)).unwrap();
        let release = config.release("app").unwrap();
        assert_eq!(release.values_file, "charts/app/values.yaml");
        assert_eq!(release.image.path, "acme/app");
        assert_eq!(config.scheme.placeholder(), "main-<sha8>");
    }

    #[test]
    fn bad_configs_are_refused_with_exit_3() {
        let dir = project(APP_VALUES);
        let dup = "\n[[release]]\nname = \"app\"\nhelm_release = \"b\"\nchart_dir = \"charts/app\"\nimage_key = \"app.image\"\nrepository = \"acme/app\"\n";
        let cases = [
            (
                APP_CONFIG.replace("branch = \"main\"", "branch = \"main\"\ncolour = 1"),
                "unknown key \"colour\"",
            ),
            (
                APP_CONFIG.replace("pointer =", "pointr ="),
                "release \"app\".version: unknown key \"pointr\"",
            ),
            (format!("{APP_CONFIG}{dup}"), "duplicate name"),
            (
                APP_CONFIG.replace("charts/app\"", "charts/nope\""),
                "charts/nope: No such file",
            ),
            (
                APP_CONFIG.replace(
                    "chart_dir = \"charts/app\"",
                    "chart_dir = \"charts\"\nvalues_file = \"charts/app/values.yaml\"",
                ),
                "no Chart.yaml",
            ),
            (
                APP_CONFIG.replace(
                    "image_key",
                    "values_file = \"charts/app/prod.yaml\"\nimage_key",
                ),
                "prod.yaml",
            ),
            (APP_CONFIG.replace("app.image", "app.img"), "not found"),
            (
                APP_CONFIG.replace("app.image", "app..image"),
                "dotted key path",
            ),
            (
                APP_CONFIG.replace("acme/app", "acme/app:1.0"),
                "repository name",
            ),
            (
                APP_CONFIG.replace("charts/app\"", "../charts/app\""),
                "without '..'",
            ),
            (
                APP_CONFIG.replace("\"charts/app\"", "\"/\""),
                "relative path",
            ),
            (
                APP_CONFIG.replace("\"charts/app\"", "\"charts/app $(id)\""),
                "relative path",
            ),
            (
                APP_CONFIG.replace("app-prod", "App_Prod"),
                "Helm release name",
            ),
            (
                APP_CONFIG.replace("branch = \"main\"", "branch = \"a/b\""),
                "may only hold",
            ),
            (
                APP_CONFIG.replace("branch = \"main\"", "branch = 1"),
                "branch must be a string",
            ),
            (APP_CONFIG.replace("/git_sha", "git_sha"), "JSON pointers"),
            (
                "branch = \"main\"\n".to_string(),
                "at least one [[release]]",
            ),
            ("not toml [".to_string(), "deploy-pins.toml"),
        ];
        for (text, needle) in cases {
            let err = load(&write_config(&dir, &text)).unwrap_err();
            assert_eq!(err.exit_code(), crate::error::EXIT_REFUSED, "{err}");
            assert!(err.to_string().contains(needle), "{needle}: {err}");
        }
    }

    #[test]
    fn symlinked_charts_and_values_files_are_refused() {
        use std::os::unix::fs::symlink;
        let outside = project(APP_VALUES);
        // The values file is a link to a file outside the project.
        let dir = project("");
        let values = dir.0.join("charts/app/values.yaml");
        std::fs::remove_file(&values).unwrap();
        symlink(outside.0.join("charts/app/values.yaml"), &values).unwrap();
        let err = load(&write_config(&dir, APP_CONFIG)).unwrap_err();
        assert_eq!(err.exit_code(), crate::error::EXIT_REFUSED);
        assert!(err.to_string().contains("is a symlink"), "{err}");
        // The chart directory is a link.
        let dir = TestDir::new();
        std::fs::create_dir_all(dir.0.join("charts")).unwrap();
        symlink(outside.0.join("charts/app"), dir.0.join("charts/app")).unwrap();
        let err = load(&write_config(&dir, APP_CONFIG)).unwrap_err();
        assert!(err.to_string().contains("is a symlink"), "{err}");
    }
}
