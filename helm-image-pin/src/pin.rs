// SPDX-License-Identifier: MIT OR Apache-2.0

//! Image pins: `<build tag>@sha256:<digest>`, as written in values.yaml.
//!
//! A build tag names the commit an image was built from. Its shape comes from
//! the config's tag template, `{branch}-{sha}` by default, where `{sha}` is the
//! first `sha_length` hex characters of the commit id.

use std::fmt;

/// The leading hex characters of a commit id, as they appear in a build tag.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShortSha(String);

impl ShortSha {
    /// Parse exactly `len` lowercase hex characters.
    pub fn parse(s: &str, len: usize) -> Option<Self> {
        (s.len() == len && is_lower_hex(s)).then(|| ShortSha(s.to_string()))
    }

    /// The first `len` characters of a full commit id.
    pub fn of_commit(commit: &str, len: usize) -> Option<Self> {
        commit.get(..len).and_then(|s| ShortSha::parse(s, len))
    }

    /// The characters.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ShortSha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A content digest, `sha256:<64 lowercase hex>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Digest(String);

impl Digest {
    /// Parse `sha256:` followed by 64 lowercase hex characters.
    pub fn parse(s: &str) -> Option<Self> {
        let hex = s.strip_prefix("sha256:")?;
        (hex.len() == 64 && is_lower_hex(hex)).then(|| Digest(s.to_string()))
    }

    /// The full `sha256:...` string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `sha256:` plus the first 12 hex characters, for tables.
    pub fn short(&self) -> &str {
        &self.0[..19]
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How build tags are spelled: `prefix`, the short sha, `suffix`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagScheme {
    prefix: String,
    suffix: String,
    len: usize,
}

/// Shortest and longest accepted `sha_length`.
pub const SHA_LENGTHS: std::ops::RangeInclusive<usize> = 7..=40;

impl TagScheme {
    /// Build a scheme from a template like `{branch}-{sha}`.
    pub fn new(template: &str, branch: &str, len: usize) -> Result<Self, String> {
        if !SHA_LENGTHS.contains(&len) {
            return Err(format!(
                "sha_length {len} is outside {}..={}",
                SHA_LENGTHS.start(),
                SHA_LENGTHS.end()
            ));
        }
        let filled = template.replace("{branch}", branch);
        let Some((prefix, suffix)) = filled.split_once("{sha}") else {
            return Err(format!("tag template {template:?} has no {{sha}}"));
        };
        if suffix.contains("{sha}") {
            return Err(format!("tag template {template:?} has {{sha}} twice"));
        }
        let tag_safe = |s: &str| {
            s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        };
        if !tag_safe(prefix) || !tag_safe(suffix) {
            return Err(format!(
                "tag template {template:?} with branch {branch:?} gives {filled:?}; \
                 a tag may only hold letters, digits, '_', '.' and '-'"
            ));
        }
        if prefix.starts_with(['.', '-']) || prefix.len() + len + suffix.len() > 128 {
            return Err(format!(
                "tag template {template:?} does not give a valid image tag"
            ));
        }
        Ok(TagScheme {
            prefix: prefix.to_string(),
            suffix: suffix.to_string(),
            len,
        })
    }

    /// The build tag for `sha`.
    pub fn tag(&self, sha: &ShortSha) -> String {
        format!("{}{}{}", self.prefix, sha, self.suffix)
    }

    /// The tag's shape for messages, e.g. `main-<sha8>`.
    pub fn placeholder(&self) -> String {
        format!("{}<sha{}>{}", self.prefix, self.len, self.suffix)
    }

    /// The short sha of a commit id under this scheme.
    pub fn sha_of(&self, commit: &str) -> Option<ShortSha> {
        ShortSha::of_commit(commit, self.len)
    }

    /// The commit a tag names, if the tag is exactly a build tag.
    pub fn build_of(&self, tag: &str) -> Option<ShortSha> {
        let sha = tag
            .strip_prefix(self.prefix.as_str())?
            .strip_suffix(self.suffix.as_str())?;
        ShortSha::parse(sha, self.len)
    }

    /// Parse `<build tag>@sha256:<digest>` strictly.
    pub fn pin(&self, value: &str) -> Option<Pin> {
        let (tag, digest) = value.split_once('@')?;
        let sha = self.build_of(tag)?;
        let digest = Digest::parse(digest)?;
        Some(Pin {
            sha,
            tag: tag.to_string(),
            digest,
        })
    }

    /// Classify what a values file's tag says.
    pub fn classify(&self, value: &str) -> CurrentPin {
        if let Some(pin) = self.pin(value) {
            return CurrentPin::Pinned(pin);
        }
        if let Some(sha) = self.build_of(value) {
            return CurrentPin::Bare(sha);
        }
        CurrentPin::Other(value.to_string())
    }

    /// A full pin for `sha` at `digest`.
    pub fn pin_for(&self, sha: ShortSha, digest: Digest) -> Pin {
        Pin {
            tag: self.tag(&sha),
            sha,
            digest,
        }
    }
}

/// What a values file's tag currently says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CurrentPin {
    /// `<build tag>@sha256:<digest>`: the only form a deploy accepts.
    Pinned(Pin),
    /// A build tag without a digest: an older pin, upgraded on the next run.
    Bare(ShortSha),
    /// Anything else (`latest`, another branch's tag): replaced on the next run.
    Other(String),
}

impl CurrentPin {
    /// The commit this pin claims to be built from, if it names one.
    pub fn sha(&self) -> Option<&ShortSha> {
        match self {
            CurrentPin::Pinned(pin) => Some(&pin.sha),
            CurrentPin::Bare(sha) => Some(sha),
            CurrentPin::Other(_) => None,
        }
    }
}

/// A full pin: a build by commit, plus the exact image digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    /// Commit the image was built from.
    pub sha: ShortSha,
    /// The build tag.
    pub tag: String,
    /// Manifest (or index) digest the registry served for the tag.
    pub digest: Digest,
}

impl Pin {
    /// `<tag>@sha256:<12 hex>`, for tables.
    pub fn short(&self) -> String {
        format!("{}@{}", self.tag, self.digest.short())
    }
}

impl fmt::Display for Pin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.tag, self.digest)
    }
}

fn is_lower_hex(s: &str) -> bool {
    s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: &str = "sha256:e1af8a1063203b62fbcc418c2cfede548cce0e1952d1d62dade9a6b51301626b";

    fn main8() -> TagScheme {
        TagScheme::new("{branch}-{sha}", "main", 8).unwrap()
    }

    #[test]
    fn parses_a_full_pin() {
        let pin = main8().pin(&format!("main-8011b395@{D}")).unwrap();
        assert_eq!(pin.sha.as_str(), "8011b395");
        assert_eq!(pin.tag, "main-8011b395");
        assert_eq!(pin.digest.as_str(), D);
        assert_eq!(pin.to_string(), format!("main-8011b395@{D}"));
        assert_eq!(pin.short(), "main-8011b395@sha256:e1af8a106320");
    }

    #[test]
    fn rejects_near_misses() {
        let scheme = main8();
        for bad in [
            "main-8011b395".to_string(),
            "main-8011B395@".to_string(),
            format!("main-8011B395@{D}"),
            format!("main-8011b39@{D}"),
            format!("main-8011b3955@{D}"),
            format!("pr-12345678@{D}"),
            format!("feature-main-8011b395@{D}"),
            "main-8011b395@sha256:abc".to_string(),
            format!("main-8011b395@{}", D.replace("sha256", "sha512")),
            format!("main-8011b395@{D}0"),
            format!(" main-8011b395@{D}"),
        ] {
            assert!(scheme.pin(&bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn classifies_current_pins() {
        let scheme = main8();
        assert!(matches!(
            scheme.classify(&format!("main-8011b395@{D}")),
            CurrentPin::Pinned(_)
        ));
        assert_eq!(
            scheme.classify("main-8011b395"),
            CurrentPin::Bare(ShortSha::parse("8011b395", 8).unwrap())
        );
        assert_eq!(
            scheme.classify("latest"),
            CurrentPin::Other("latest".into())
        );
        assert_eq!(
            scheme.classify("main-8011b395-dirty"),
            CurrentPin::Other("main-8011b395-dirty".into())
        );
    }

    #[test]
    fn templates() {
        let scheme = main8();
        let sha = scheme
            .sha_of("8011b395d598a78c14efe791190aa906ae675c17")
            .unwrap();
        assert_eq!(scheme.tag(&sha), "main-8011b395");
        assert_eq!(scheme.placeholder(), "main-<sha8>");
        assert!(scheme.sha_of("8011b39").is_none());

        let custom = TagScheme::new("build-{sha}-{branch}", "trunk", 12).unwrap();
        let sha = custom
            .sha_of("8011b395d598a78c14efe791190aa906ae675c17")
            .unwrap();
        assert_eq!(custom.tag(&sha), "build-8011b395d598-trunk");
        assert_eq!(custom.build_of("build-8011b395d598-trunk"), Some(sha));
        assert_eq!(custom.build_of("build-8011b395-trunk"), None);

        for (template, branch, len) in [
            ("{branch}", "main", 8),
            ("{sha}{sha}", "main", 8),
            ("{branch}-{sha}", "feature/x", 8),
            ("{branch}-{sha}", "main", 6),
            ("{branch}-{sha}", "main", 41),
            ("-{sha}", "main", 8),
            ("{branch} {sha}", "main", 8),
        ] {
            assert!(
                TagScheme::new(template, branch, len).is_err(),
                "{template} {branch} {len}"
            );
        }
    }
}
