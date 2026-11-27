// SPDX-License-Identifier: MIT OR Apache-2.0

//! First-parent history of the branch builds come from.
//!
//! This shells out to `git` instead of linking a git library. It needs two
//! read-only commands, git is already on every machine that has a checkout,
//! and the alternatives cost more than they buy here: `git2` builds libgit2
//! from C, and `gix` adds a large dependency tree to a cold CI build. The
//! output is validated strictly instead.

use crate::error::{Error, Result};
use crate::pin::ShortSha;
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;

/// Read access to the repository history.
pub trait History {
    /// Full commit ids on the first-parent line from HEAD, newest first.
    fn first_parent(&self) -> Result<Vec<String>>;
    /// `(sha8, subject)` for first-parent commits in `from..to`, newest
    /// first, at most `max`.
    fn subjects(&self, from: &str, to: &str, max: usize) -> Result<Vec<(String, String)>>;
    /// Whether the checkout is shallow, so the history is cut off.
    fn is_shallow(&self) -> Result<bool>;
}

/// [`History`] backed by the `git` binary.
pub struct GitCli {
    root: PathBuf,
}

impl GitCli {
    /// Run git in `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        GitCli { root: root.into() }
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .output()
            .map_err(|e| Error::Git(format!("cannot run git: {e}")))?;
        if !output.status.success() {
            return Err(Error::Git(format!(
                "`git {}` failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        String::from_utf8(output.stdout)
            .map_err(|_| Error::Git(format!("`git {}` printed non-UTF-8", args.join(" "))))
    }
}

fn check_rev(rev: &str) -> Result<()> {
    if rev.is_empty() || rev.starts_with('-') || rev.chars().any(char::is_whitespace) {
        return Err(Error::Git(format!("refusing suspicious revision {rev:?}")));
    }
    Ok(())
}

fn is_commit_id(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl History for GitCli {
    fn first_parent(&self) -> Result<Vec<String>> {
        let out = self.run(&["rev-list", "--first-parent", "HEAD"])?;
        parse_rev_list(&out)
    }

    fn subjects(&self, from: &str, to: &str, max: usize) -> Result<Vec<(String, String)>> {
        check_rev(from)?;
        check_rev(to)?;
        let range = format!("{from}..{to}");
        let max = format!("-n{max}");
        let out = self.run(&[
            "log",
            "--first-parent",
            &max,
            "--format=%H%x00%s",
            "--end-of-options",
            &range,
        ])?;
        out.lines()
            .map(|line| {
                let (id, subject) = line
                    .split_once('\0')
                    .filter(|(id, _)| is_commit_id(id))
                    .ok_or_else(|| Error::Git(format!("unexpected git log line {line:?}")))?;
                Ok((id[..8].to_string(), subject.to_string()))
            })
            .collect()
    }

    fn is_shallow(&self) -> Result<bool> {
        match self.run(&["rev-parse", "--is-shallow-repository"])?.trim() {
            "true" => Ok(true),
            "false" => Ok(false),
            other => Err(Error::Git(format!(
                "unexpected --is-shallow-repository output {other:?}"
            ))),
        }
    }
}

/// The first-parent line from HEAD, refusing a shallow clone: its history
/// stops early, so builds and pins below the cut would look off-history.
pub fn line(history: &dyn History) -> Result<Line> {
    if history.is_shallow()? {
        return Err(Error::Git(
            "this is a shallow clone, so the first-parent history is cut off; \
             fetch the full history (actions/checkout: fetch-depth: 0, or `git fetch --unshallow`)"
                .into(),
        ));
    }
    Ok(Line::new(history.first_parent()?))
}

/// Validate `git rev-list` output: one full lowercase commit id per line,
/// no duplicates, at least one.
pub fn parse_rev_list(out: &str) -> Result<Vec<String>> {
    let mut seen = HashSet::new();
    let mut commits = Vec::new();
    for line in out.lines() {
        if !is_commit_id(line) {
            return Err(Error::Git(format!("unexpected rev-list line {line:?}")));
        }
        if !seen.insert(line) {
            return Err(Error::Git(format!("rev-list repeated {line}")));
        }
        commits.push(line.to_string());
    }
    if commits.is_empty() {
        return Err(Error::Git("rev-list returned no commits".into()));
    }
    Ok(commits)
}

/// The first-parent line, newest first, with sha8 lookups that refuse to
/// guess.
pub struct Line {
    commits: Vec<String>,
}

impl Line {
    /// Wrap validated commit ids, newest first.
    pub fn new(commits: Vec<String>) -> Self {
        Line { commits }
    }

    /// The tip commit.
    pub fn tip(&self) -> &str {
        &self.commits[0]
    }

    /// Every commit, newest first.
    pub fn commits(&self) -> &[String] {
        &self.commits
    }

    /// Position (0 = tip) and full id of the one commit whose id starts with
    /// `sha`. `None` if no commit on the line matches; an error if several do,
    /// since a tag could then name either.
    pub fn find(&self, sha: &ShortSha) -> Result<Option<(usize, &str)>> {
        let mut hits = self
            .commits
            .iter()
            .enumerate()
            .filter(|(_, id)| id.starts_with(sha.as_str()));
        let first = hits.next();
        if let Some((_, other)) = hits.next() {
            return Err(Error::Refused(format!(
                "{sha} is ambiguous: {} and {other} both start with it on the first-parent line",
                first.map(|(_, id)| id.as_str()).unwrap_or_default()
            )));
        }
        Ok(first.map(|(pos, id)| (pos, id.as_str())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(prefix: &str) -> String {
        format!("{prefix:0<40}")
    }

    #[test]
    fn rev_list_output_is_validated() {
        let good = format!("{}\n{}\n", id("aa"), id("bb"));
        assert_eq!(parse_rev_list(&good).unwrap().len(), 2);
        for bad in [
            String::new(),
            format!("{}\n{}\n", id("aa"), id("aa")),
            "8011b395\n".to_string(),
            format!("{}\n", id("AA")),
            format!("{} extra\n", id("aa")),
            "warning: something\n".to_string(),
        ] {
            assert!(parse_rev_list(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn find_is_exact_and_refuses_ambiguity() {
        let line = Line::new(vec![id("11111111a"), id("22222222"), id("11111111b")]);
        let sha = |s: &str| ShortSha::parse(s, 8).unwrap();
        assert_eq!(line.find(&sha("22222222")).unwrap().unwrap().0, 1);
        assert!(line.find(&sha("33333333")).unwrap().is_none());
        let err = line.find(&sha("11111111")).unwrap_err();
        assert!(matches!(err, Error::Refused(_)), "{err}");
    }

    #[test]
    fn suspicious_revisions_are_refused() {
        for rev in ["", "--all", "main extra", "-n1"] {
            assert!(check_rev(rev).is_err(), "{rev}");
        }
        assert!(check_rev("HEAD").is_ok());
        assert!(check_rev("origin/main").is_ok());
    }
}
