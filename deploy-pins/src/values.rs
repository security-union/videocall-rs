// SPDX-License-Identifier: MIT OR Apache-2.0

//! Structural reads and minimal edits of a chart's values.yaml.
//!
//! The pin lives at `<image_key>.tag`, next to `<image_key>.repository`. Two
//! parsers agree before anything is trusted:
//!
//! - the loaded YAML tree is the authority for what those two values are;
//! - a walk over the parser's events finds the one source position of the
//!   tag, so an edit can replace exactly those bytes and keep comments, key
//!   order, quoting and a missing final newline.
//!
//! If the event walk and the tree disagree, the file is refused. So are the
//! constructs that let a walk and a tree (or Helm's YAML parser and ours)
//! see different documents: aliases or collections used as mapping keys,
//! merge keys (`<<`), anchors or tags on the pin, more than one document.
//! After an edit the result is parsed again and must equal the original tree
//! with that one scalar changed, or nothing is written.
//!
//! YAML comes from `yaml-rust2`, a pure-Rust YAML 1.2 parser maintained by the
//! yaml-rust2 project (the maintained successor of `yaml-rust`), with no
//! unsafe C code. `serde_yaml` is deprecated and was not considered.

use crate::error::{Error, Result};
use yaml_rust2::parser::{Event, Parser};
use yaml_rust2::scanner::TScalarStyle;
use yaml_rust2::{Yaml, YamlLoader};

/// The image block of one values file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageBlock {
    /// `<image_key>.repository`.
    pub repository: String,
    /// `<image_key>.tag`.
    pub tag: String,
    tag_at: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    start: usize,
    end: usize,
    quote: Option<char>,
}

#[derive(Debug)]
enum Frame {
    Map {
        key: Option<String>,
        expecting_key: bool,
    },
    Seq,
}

#[derive(Debug)]
struct Hit {
    value: String,
    style: TScalarStyle,
    index: usize,
    decorated: bool,
}

fn key_path(key: &[String], leaf: &str) -> Vec<String> {
    let mut out = key.to_vec();
    out.push(leaf.to_string());
    out
}

/// Collect every scalar at the given key paths in the only document.
fn scalars_at(path: &str, src: &str, targets: &[Vec<String>]) -> Result<Vec<Vec<Hit>>> {
    let mut hits: Vec<Vec<Hit>> = targets.iter().map(|_| Vec::new()).collect();
    let mut parser = Parser::new_from_str(src);
    // Open collections, plus the key each one sits under (None in sequences).
    let mut frames: Vec<Frame> = Vec::new();
    let mut under: Vec<Option<String>> = Vec::new();
    let mut documents = 0;

    let current_path = |frames: &[Frame], under: &[Option<String>]| -> Option<Vec<String>> {
        let mut out = Vec::new();
        for seg in under.iter().skip(1) {
            out.push(seg.clone()?);
        }
        match frames.last() {
            Some(Frame::Map { key: Some(k), .. }) => out.push(k.clone()),
            _ => return None,
        }
        Some(out)
    };
    // After a value node completes, the enclosing map expects a key again.
    fn advance(frames: &mut [Frame]) {
        if let Some(Frame::Map { expecting_key, key }) = frames.last_mut()
            && !*expecting_key
        {
            *expecting_key = true;
            *key = None;
        }
    }
    let in_key_position = |frames: &[Frame]| {
        matches!(
            frames.last(),
            Some(Frame::Map {
                expecting_key: true,
                ..
            })
        )
    };
    let refuse = |what: &str| {
        Error::values(
            path,
            format!(
                "uses {what}; deploy-pins only edits files where every mapping key is a plain \
                 scalar, so the document it reads is the one Helm reads"
            ),
        )
    };

    loop {
        let (event, mark) = parser
            .next_token()
            .map_err(|e| Error::values(path, format!("not valid YAML: {e}")))?;
        match event {
            Event::StreamEnd => break,
            Event::DocumentStart => {
                documents += 1;
                if documents > 1 {
                    return Err(Error::values(path, "has more than one YAML document"));
                }
            }
            Event::Scalar(value, style, anchor, tag) => {
                if in_key_position(&frames) {
                    if value == "<<" && style == TScalarStyle::Plain {
                        return Err(refuse("a merge key (<<)"));
                    }
                    if let Some(Frame::Map { key, expecting_key }) = frames.last_mut() {
                        *key = Some(value);
                        *expecting_key = false;
                    }
                    continue;
                }
                if let Some(here) = current_path(&frames, &under) {
                    for (i, target) in targets.iter().enumerate() {
                        if here == *target {
                            hits[i].push(Hit {
                                value: value.clone(),
                                style,
                                index: mark.index(),
                                decorated: anchor != 0 || tag.is_some(),
                            });
                        }
                    }
                }
                advance(&mut frames);
            }
            Event::Alias(_) => {
                if in_key_position(&frames) {
                    return Err(refuse("an alias as a mapping key"));
                }
                if let Some(here) = current_path(&frames, &under)
                    && targets.contains(&here)
                {
                    return Err(Error::values(
                        path,
                        format!(
                            "{} is a YAML alias; pin it as a plain value",
                            here.join(".")
                        ),
                    ));
                }
                advance(&mut frames);
            }
            Event::MappingStart(..) | Event::SequenceStart(..) => {
                if in_key_position(&frames) {
                    return Err(refuse("a collection as a mapping key"));
                }
                let name = match frames.last() {
                    Some(Frame::Map { key, .. }) => key.clone(),
                    _ => None,
                };
                under.push(name);
                frames.push(if matches!(event, Event::MappingStart(..)) {
                    Frame::Map {
                        key: None,
                        expecting_key: true,
                    }
                } else {
                    Frame::Seq
                });
            }
            Event::MappingEnd | Event::SequenceEnd => {
                frames.pop();
                under.pop();
                advance(&mut frames);
            }
            _ => {}
        }
    }
    if documents != 1 {
        return Err(Error::values(path, "must hold exactly one YAML document"));
    }
    Ok(hits)
}

fn exactly_one(path: &str, key: &[String], mut hits: Vec<Hit>) -> Result<Hit> {
    match hits.len() {
        1 => Ok(hits.remove(0)),
        0 => Err(Error::values(path, format!("{} not found", key.join(".")))),
        n => Err(Error::values(
            path,
            format!("{} appears {n} times", key.join(".")),
        )),
    }
}

fn load_one(path: &str, src: &str) -> Result<Yaml> {
    let mut docs = YamlLoader::load_from_str(src)
        .map_err(|e| Error::values(path, format!("not valid YAML: {e}")))?;
    if docs.len() != 1 {
        return Err(Error::values(path, "must hold exactly one YAML document"));
    }
    Ok(docs.remove(0))
}

/// The string at `keys` in the tree, walking mappings only.
fn tree_string<'a>(doc: &'a Yaml, keys: &[String]) -> Option<&'a str> {
    let mut node = doc;
    for key in keys {
        match node {
            Yaml::Hash(map) => node = map.get(&Yaml::String(key.clone()))?,
            _ => return None,
        }
    }
    node.as_str()
}

/// Find `<image_key>.{repository,tag}` and where the tag's bytes are.
pub fn read_image(path: &str, src: &str, image_key: &[String]) -> Result<ImageBlock> {
    let repo_key = key_path(image_key, "repository");
    let tag_key = key_path(image_key, "tag");
    let mut found = scalars_at(path, src, &[repo_key.clone(), tag_key.clone()])?;
    let tag = exactly_one(path, &tag_key, found.pop().unwrap_or_default())?;
    let repository = exactly_one(path, &repo_key, found.pop().unwrap_or_default())?;

    // The tree decides what the values are; the walk only says where.
    let tree = load_one(path, src)?;
    for (key, hit) in [(&repo_key, &repository), (&tag_key, &tag)] {
        match tree_string(&tree, key) {
            Some(value) if value == hit.value => {}
            Some(value) => {
                return Err(Error::values(
                    path,
                    format!(
                        "{} reads as {value:?} in the parsed document but {:?} in the source; \
                         refusing to guess",
                        key.join("."),
                        hit.value
                    ),
                ));
            }
            None => {
                return Err(Error::values(
                    path,
                    format!(
                        "{} is not a string in the parsed document (quote it)",
                        key.join(".")
                    ),
                ));
            }
        }
    }

    let tag_name = tag_key.join(".");
    if tag.decorated {
        return Err(Error::values(
            path,
            format!("{tag_name} carries an anchor or tag; pin it as a plain value"),
        ));
    }
    let quote = match tag.style {
        TScalarStyle::Plain => None,
        TScalarStyle::SingleQuoted => Some('\''),
        TScalarStyle::DoubleQuoted => Some('"'),
        _ => {
            return Err(Error::values(
                path,
                format!("{tag_name} is a block scalar; use a one-line value"),
            ));
        }
    };
    // The source bytes must be exactly the value (plus its quotes), so the
    // edit is a pure replacement. Escapes or folding fail here, loudly.
    let expected = match quote {
        Some(q) => format!("{q}{}{q}", tag.value),
        None => tag.value.clone(),
    };
    // yaml-rust2's Marker::index counts characters, not bytes (its doc
    // comment says bytes; the non-ASCII test below pins the behaviour).
    let start = char_to_byte(src, tag.index).ok_or_else(|| {
        Error::values(path, "parser reported a position past the end of the file")
    })?;
    let end = start + expected.len();
    if src.get(start..end) != Some(expected.as_str()) {
        return Err(Error::values(
            path,
            format!(
                "{tag_name} is not written as a simple one-line value at byte {start}; edit it by hand"
            ),
        ));
    }
    Ok(ImageBlock {
        repository: repository.value,
        tag: tag.value,
        tag_at: Span { start, end, quote },
    })
}

fn char_to_byte(src: &str, chars: usize) -> Option<usize> {
    src.char_indices()
        .map(|(byte, _)| byte)
        .chain(std::iter::once(src.len()))
        .nth(chars)
}

fn set_path(doc: &mut Yaml, keys: &[String], value: Yaml) -> bool {
    let Some((first, rest)) = keys.split_first() else {
        *doc = value;
        return true;
    };
    match doc {
        Yaml::Hash(map) => match map.get_mut(&Yaml::String(first.clone())) {
            Some(child) => set_path(child, rest, value),
            None => false,
        },
        _ => false,
    }
}

/// Return `src` with `<image_key>.tag` set to `new_tag`, after proving the
/// result differs from `src` in that one scalar and nothing else.
pub fn rewrite_tag(path: &str, src: &str, image_key: &[String], new_tag: &str) -> Result<String> {
    if new_tag.is_empty() || new_tag.contains(['\n', '\r', '"', '\'', '#', ' ']) {
        return Err(Error::values(
            path,
            format!("refusing to write tag {new_tag:?}"),
        ));
    }
    let block = read_image(path, src, image_key)?;
    let replacement = match block.tag_at.quote {
        Some(q) => format!("{q}{new_tag}{q}"),
        None => new_tag.to_string(),
    };
    let mut out = String::with_capacity(src.len() + replacement.len());
    out.push_str(&src[..block.tag_at.start]);
    out.push_str(&replacement);
    out.push_str(&src[block.tag_at.end..]);

    let mut expected = load_one(path, src)?;
    if !set_path(
        &mut expected,
        &key_path(image_key, "tag"),
        Yaml::String(new_tag.to_string()),
    ) {
        return Err(Error::values(path, "tag path vanished while editing"));
    }
    let actual = load_one(path, &out)?;
    if actual != expected {
        return Err(Error::values(
            path,
            "edited document is not the original with only the tag changed; nothing written",
        ));
    }
    let reread = read_image(path, &out, image_key)?;
    if reread.tag != new_tag || reread.repository != block.repository {
        return Err(Error::values(
            path,
            "edited tag does not read back; nothing written",
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEW: &str =
        "main-8011b395@sha256:e1af8a1063203b62fbcc418c2cfede548cce0e1952d1d62dade9a6b51301626b";

    fn key(s: &str) -> Vec<String> {
        s.split('.').map(str::to_string).collect()
    }

    fn only_tag_line_changed(before: &str, after: &str) {
        let b: Vec<_> = before.split('\n').collect();
        let a: Vec<_> = after.split('\n').collect();
        assert_eq!(a.len(), b.len());
        let changed: Vec<_> = (0..a.len()).filter(|i| a[*i] != b[*i]).collect();
        assert_eq!(changed.len(), 1, "exactly one line differs");
        assert!(a[changed[0]].trim_start().starts_with("tag:"));
    }

    #[test]
    fn reads_and_rewrites_every_real_values_file_minimally() {
        // Different from every current pin.
        const NEW: &str =
            "main-0123abcd@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let config = crate::config::tests::repo_config();
        for release in &config.releases {
            let file = &release.values_file;
            let src = config.read(file).unwrap();
            let block = read_image(file, &src, &release.image_key).unwrap();
            assert_eq!(block.repository, release.image.written);
            assert!(
                config.scheme.pin(&block.tag).is_some(),
                "{file} holds a full pin"
            );
            let out = rewrite_tag(file, &src, &release.image_key, NEW).unwrap();
            only_tag_line_changed(&src, &out);
            assert_eq!(src.ends_with('\n'), out.ends_with('\n'), "{file}");
            assert_eq!(read_image(file, &out, &release.image_key).unwrap().tag, NEW);
            // Idempotent: writing the same value again is a no-op.
            assert_eq!(
                rewrite_tag(file, &out, &release.image_key, NEW).unwrap(),
                out
            );
        }
    }

    #[test]
    fn missing_final_newline_stays_missing() {
        let src = "sub:\n  image:\n    repository: a/b\n    tag: old";
        let out = rewrite_tag("doc", src, &key("sub.image"), "latest").unwrap();
        assert_eq!(out, "sub:\n  image:\n    repository: a/b\n    tag: latest");
    }

    const DOC: &str = "\
# top comment
tag: top-level-decoy
sub:
  replicaCount: 1  # comment
  image:
    repository: securityunion/x
    pullPolicy: IfNotPresent
    tag: old  # keep me
  sidecar:
    image:
      tag: decoy
  list:
    - tag: decoy
other:
  image:
    tag: decoy
";

    #[test]
    fn finds_the_right_tag_among_decoys_and_keeps_the_comment() {
        let out = rewrite_tag("doc", DOC, &key("sub.image"), NEW).unwrap();
        assert!(out.contains(&format!("    tag: {NEW}  # keep me\n")));
        assert_eq!(out.matches("decoy").count(), 4);
        only_tag_line_changed(DOC, &out);
    }

    #[test]
    fn nested_and_top_level_image_keys() {
        let doc = "image:\n  repository: a/b\n  tag: old\n";
        assert_eq!(read_image("d", doc, &key("image")).unwrap().tag, "old");
        let deep = "a:\n  b:\n    c:\n      repository: a/b\n      tag: old\n";
        assert_eq!(read_image("d", deep, &key("a.b.c")).unwrap().tag, "old");
    }

    #[test]
    fn keeps_quote_style() {
        let dq = DOC.replace("tag: old ", "tag: \"old\" ");
        let out = rewrite_tag("doc", &dq, &key("sub.image"), NEW).unwrap();
        assert!(out.contains(&format!("tag: \"{NEW}\"  # keep me")));
        let sq = DOC.replace("tag: old ", "tag: 'old' ");
        let out = rewrite_tag("doc", &sq, &key("sub.image"), NEW).unwrap();
        assert!(out.contains(&format!("tag: '{NEW}'  # keep me")));
    }

    #[test]
    fn byte_offsets_survive_non_ascii_text_before_the_tag() {
        let doc = DOC.replace("# top comment", "# Grüße, ☃ and 日本語");
        let out = rewrite_tag("doc", &doc, &key("sub.image"), NEW).unwrap();
        only_tag_line_changed(&doc, &out);
    }

    /// The reproduction from the code review: an alias used as a key shifted
    /// the old event walk by one, so it read `repository` and `tag` from the
    /// wrong scalars. The walk now refuses alias and collection keys, and the
    /// parsed tree has the last word on both values.
    #[test]
    fn alias_key_shift_from_review_is_refused() {
        let digest = "sha256:4a57e6f98e5d98ec04407c1aa40340cf134e1c3c54652fad54d479107058f1bb";
        let doc = format!(
            "meeting-api:\n  image:\n    a: &a foo\n    *a : repository\n    securityunion/videocall-meeting-api: tag\n    main-8011b395@{digest}: zzz\n    repository: evil/image\n    pullPolicy: IfNotPresent\n    tag: evil\n"
        );
        let err = read_image("doc", &doc, &key("meeting-api.image")).unwrap_err();
        assert_eq!(err.exit_code(), crate::error::EXIT_REFUSED);
        assert!(err.to_string().contains("alias as a mapping key"), "{err}");
        assert!(rewrite_tag("doc", &doc, &key("meeting-api.image"), NEW).is_err());
    }

    #[test]
    fn complex_and_merge_keys_are_refused() {
        for doc in [
            "sub:\n  image:\n    ? [a, b]\n    : x\n    repository: a/b\n    tag: old\n",
            "sub:\n  image:\n    ? {a: 1}\n    : x\n    repository: a/b\n    tag: old\n",
            "base: &b {tag: evil}\nsub:\n  image:\n    <<: *b\n    repository: a/b\n    tag: old\n",
        ] {
            let err = read_image("doc", doc, &key("sub.image")).unwrap_err();
            assert!(matches!(err, Error::Values { .. }), "{doc}: {err}");
        }
    }

    #[test]
    fn tree_and_walk_must_agree() {
        // The image block arrives through an alias: the tree sees a tag the
        // walk cannot place, so the walk finds nothing and stops.
        let doc = "x: &img\n  repository: a/b\n  tag: old\nsub:\n  image: *img\n";
        assert!(read_image("doc", doc, &key("sub.image")).is_err());
        // A tag the tree reads as a number: quote it first.
        let num = "sub:\n  image:\n    repository: a/b\n    tag: 1.0\n";
        let err = read_image("doc", num, &key("sub.image")).unwrap_err();
        assert!(err.to_string().contains("not a string"), "{err}");
    }

    #[test]
    fn refuses_duplicate_missing_and_odd_tags() {
        let k = key("sub.image");
        let dup = DOC.replace(
            "    tag: old  # keep me\n",
            "    tag: old\n    tag: again\n",
        );
        assert!(rewrite_tag("doc", &dup, &k, NEW).is_err());

        let missing = DOC.replace("    tag: old  # keep me\n", "");
        let err = rewrite_tag("doc", &missing, &k, NEW).unwrap_err();
        assert!(err.to_string().contains("sub.image.tag not found"), "{err}");

        let block = DOC.replace("    tag: old  # keep me\n", "    tag: |\n      old\n");
        assert!(rewrite_tag("doc", &block, &k, NEW).is_err());

        let escaped = DOC.replace("tag: old ", "tag: \"o\\u006cd\" ");
        assert!(rewrite_tag("doc", &escaped, &k, NEW).is_err());

        let anchored = DOC.replace("tag: old ", "tag: &t old ");
        assert!(rewrite_tag("doc", &anchored, &k, NEW).is_err());

        let aliased = format!("x: &t old\n{}", DOC.replace("tag: old ", "tag: *t "));
        assert!(rewrite_tag("doc", &aliased, &k, NEW).is_err());

        let two_docs = format!("{DOC}---\nmore: 1\n");
        assert!(rewrite_tag("doc", &two_docs, &k, NEW).is_err());

        assert!(rewrite_tag("doc", "sub: [unclosed", &k, NEW).is_err());
        assert!(rewrite_tag("doc", DOC, &key("nope.image"), NEW).is_err());
    }

    #[test]
    fn multi_line_plain_tag_is_refused() {
        let folded = DOC.replace(
            "    tag: old  # keep me\n",
            "    tag: old\n      continued\n",
        );
        let err = rewrite_tag("doc", &folded, &key("sub.image"), NEW).unwrap_err();
        assert!(matches!(err, Error::Values { .. }), "{err}");
    }

    #[test]
    fn refuses_to_write_values_that_need_quoting() {
        for bad in ["", "a b", "a#b", "a\nb", "a\"b"] {
            assert!(
                rewrite_tag("doc", DOC, &key("sub.image"), bad).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn flow_style_image_block_is_handled() {
        let doc = "sub:\n  image: {repository: securityunion/x, tag: old}\n";
        let out = rewrite_tag("doc", doc, &key("sub.image"), NEW).unwrap();
        assert_eq!(
            out,
            format!("sub:\n  image: {{repository: securityunion/x, tag: {NEW}}}\n")
        );
    }
}
