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

//! Receive-side parsing of server-authored live captions (`CAPTION` packets).

use crate::client::reactions::resolve_reaction_display_name;
use log::debug;
use protobuf::Message;
use unicode_normalization::char::is_combining_mark;
use videocall_types::protos::caption_packet::{caption_packet::Kind, CaptionPacket};
use videocall_types::protos::packet_wrapper::{packet_wrapper::PacketType, PacketWrapper};
use videocall_types::SYSTEM_USER_ID;

/// Upper bound on rendered caption text, in characters.
pub const CAPTION_TEXT_MAX_CHARS: usize = 500;

/// Upper bound on a BCP-47 language tag, in characters.
pub const CAPTION_LANGUAGE_MAX_CHARS: usize = 35;

/// Upper bound on a resolved speaker or starter name, in characters.
pub const CAPTION_NAME_MAX_CHARS: usize = 64;

/// Marks the start of an interim whose head was cut off.
pub const CAPTION_TRUNCATION_MARK: char = '\u{2026}';

/// Upper bound on the translations kept from one caption.
pub const CAPTION_MAX_TRANSLATIONS: usize = 8;
/// Translation entries of a packet that are sanitized; the rest are dropped.
pub const CAPTION_MAX_TRANSLATIONS_SCANNED: usize = 32;

/// One server translation of a caption.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptionTranslation {
    pub language: String,
    pub text: String,
}

/// One sanitized caption update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptionEvent {
    pub speaker_session_id: u64,
    pub speaker_name: String,
    /// The speaker is this client's own session.
    pub is_local: bool,
    pub utterance_id: u64,
    pub revision: u32,
    pub is_final: bool,
    /// Sanitized caption text. Empty when the packet's text was nothing but
    /// stripped characters: a consumer then withdraws the utterance's line.
    pub text: String,
    pub text_language: String,
    pub source_text: String,
    pub source_language: String,
    pub timestamp_ms: u64,
    /// The caption in each language the producer rendered, sanitized like
    /// `text`; at most [`CAPTION_MAX_TRANSLATIONS`], one per language.
    pub translations: Vec<CaptionTranslation>,
}

/// Room-level transcription state from a STATUS_ACTIVE / STATUS_STOPPED packet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptionStatus {
    pub active: bool,
    pub started_by_user_id: String,
    pub started_by_name: String,
    /// `started_by_user_id` is this client's own user.
    pub started_by_local: bool,
}

/// What an inbound `CAPTION` carried, delivered to `on_caption`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptionUpdate {
    Text(CaptionEvent),
    Status(CaptionStatus),
}

/// Upper bound on consecutive combining marks after one base character. Real
/// text needs more than 4: Burmese stacks 5 on one consonant and pointed Hebrew
/// with cantillation 5 or more, so 8 keeps them while still capping zalgo.
pub const CAPTION_MAX_COMBINING_MARKS: usize = 8;

/// Unicode format (Cf), bidi control and other invisible characters, which can
/// reorder, hide or pad rendered text. ZWNJ and ZWJ are kept for Persian and
/// Indic shaping. Tag characters and variation selectors (VS16 included) are
/// stripped, so emoji subdivision flags and forced emoji presentation are lost.
pub fn is_invisible_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{034F}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{115F}'..='\u{1160}'
            | '\u{180B}'..='\u{180F}'
            | '\u{200B}'
            | '\u{200E}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{2800}'
            | '\u{3164}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}'
            | '\u{FFA0}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
            | '\u{E0100}'..='\u{E01EF}'
    )
}

fn clean(raw: &str) -> String {
    let mut marks = 0;
    let mut joiners = 0;
    raw.chars()
        .filter_map(|c| match c {
            '\n' | '\r' | '\t' | '\u{000B}' | '\u{000C}' | '\u{0085}' | '\u{2028}' | '\u{2029}' => {
                Some(' ')
            }
            c if c.is_control() || is_invisible_format_char(c) => None,
            c => Some(c),
        })
        .filter(|&c| {
            // ZWNJ/ZWJ are kept for Persian, Indic and emoji sequences, but a
            // joiner only ever joins its two neighbours: a run of them is an
            // invisible payload, so only the first of a run survives.
            if matches!(c, '\u{200C}' | '\u{200D}') {
                joiners += 1;
                return joiners <= 1;
            }
            joiners = 0;
            if !is_combining_mark(c) {
                marks = 0;
                return true;
            }
            marks += 1;
            marks <= CAPTION_MAX_COMBINING_MARKS
        })
        .collect()
}

/// How far into a cut interim tail a word boundary is still worth snapping
/// to; past that the cut stays where it is (a long unspaced run would
/// otherwise collapse to its last few words).
const CAPTION_TAIL_SNAP_WINDOW: usize = 24;

/// Strips control, format, bidi and invisible characters (line breaks become
/// spaces) and combining marks past [`CAPTION_MAX_COMBINING_MARKS`], trims, and
/// keeps the first `max_chars` characters, trimmed again after the cut.
pub fn sanitize_caption_text(raw: &str, max_chars: usize) -> String {
    let cut: String = clean(raw).trim().chars().take(max_chars).collect();
    cut.trim_end().to_string()
}

/// A language tag for display and matching: lowercased, or empty when it is
/// blank, longer than [`CAPTION_LANGUAGE_MAX_CHARS`] or not `[A-Za-z0-9-]`.
pub fn sanitize_language_tag(raw: &str) -> String {
    let tag = sanitize_caption_text(raw, CAPTION_LANGUAGE_MAX_CHARS + 1);
    if tag.chars().count() > CAPTION_LANGUAGE_MAX_CHARS
        || !tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return String::new();
    }
    tag.to_ascii_lowercase()
}

/// Like [`sanitize_caption_text`] but keeps the LAST `max_chars` characters,
/// starting at a word boundary when there is one, behind
/// [`CAPTION_TRUNCATION_MARK`].
pub fn sanitize_caption_tail(raw: &str, max_chars: usize) -> String {
    let cleaned = clean(raw);
    let trimmed = cleaned.trim();
    let total = trimmed.chars().count();
    if total <= max_chars {
        return trimmed.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let tail: String = trimmed.chars().skip(total - keep).collect();
    let window_end = tail
        .char_indices()
        .nth(CAPTION_TAIL_SNAP_WINDOW)
        .map_or(tail.len(), |(i, _)| i);
    let tail = match tail[..window_end].find(char::is_whitespace) {
        Some(i)
            if !trimmed
                .chars()
                .nth(total - keep - 1)
                .is_some_and(char::is_whitespace) =>
        {
            tail[i..].trim_start()
        }
        _ => tail.trim_start(),
    };
    // A cut can land inside a cluster; its marks would render on the ellipsis.
    let tail = tail.trim_start_matches(is_combining_mark);
    format!("{CAPTION_TRUNCATION_MARK}{tail}")
}

/// Sanitized name for display: the cached display name, else the in-packet
/// user id, else the unknown-sender placeholder.
pub fn sanitize_caption_name(cached: Option<String>, user_id: &[u8]) -> String {
    let cached = cached.map(|n| sanitize_caption_text(&n, CAPTION_NAME_MAX_CHARS));
    let user_id = sanitize_caption_text(&String::from_utf8_lossy(user_id), CAPTION_NAME_MAX_CHARS);
    resolve_reaction_display_name(cached, user_id.as_bytes())
}

/// Turns an inbound `CAPTION` wrapper into a [`CaptionUpdate`].
///
/// Returns `None` for anything that is not a parseable, server-authored
/// caption of a known kind. `name_by_session` / `name_by_user` resolve display
/// names; `is_local_session` / `local_user_id` identify us.
pub fn caption_update_from_wrapper(
    wrapper: &PacketWrapper,
    name_by_session: impl FnOnce(u64) -> Option<String>,
    name_by_user: impl FnOnce(&str) -> Option<String>,
    is_local_session: impl FnOnce(u64) -> bool,
    local_user_id: &str,
) -> Option<CaptionUpdate> {
    if wrapper.packet_type != PacketType::CAPTION.into()
        || wrapper.user_id != SYSTEM_USER_ID.as_bytes()
        || wrapper.session_id != 0
    {
        return None;
    }
    let caption = match CaptionPacket::parse_from_bytes(&wrapper.data) {
        Ok(c) => c,
        Err(e) => {
            debug!("Failed to parse CAPTION packet: {e}");
            return None;
        }
    };
    let active = match caption.kind.enum_value() {
        Ok(Kind::TEXT) => None,
        Ok(Kind::STATUS_ACTIVE) => Some(true),
        Ok(Kind::STATUS_STOPPED) => Some(false),
        Ok(Kind::KIND_UNSPECIFIED) | Err(_) => return None,
    };
    if let Some(active) = active {
        let user_id = String::from_utf8_lossy(&caption.started_by);
        let starter = active && !user_id.is_empty();
        let (started_by_user_id, started_by_name) = if starter {
            (
                sanitize_caption_text(&user_id, CAPTION_NAME_MAX_CHARS),
                sanitize_caption_name(name_by_user(&user_id), &caption.started_by),
            )
        } else {
            Default::default()
        };
        return Some(CaptionUpdate::Status(CaptionStatus {
            active,
            started_by_user_id,
            started_by_name,
            started_by_local: starter && user_id == local_user_id,
        }));
    }
    let speaker = caption.speaker_session_id;
    // An interim grows at its end, so keep the tail of both texts.
    let body = |raw: &str| {
        if caption.is_final {
            sanitize_caption_text(raw, CAPTION_TEXT_MAX_CHARS)
        } else {
            sanitize_caption_tail(raw, CAPTION_TEXT_MAX_CHARS)
        }
    };
    let text_language = sanitize_language_tag(&caption.text_language);
    let source_language = sanitize_language_tag(&caption.source_language);
    let mut seen = std::collections::HashSet::new();
    let mut translations: Vec<CaptionTranslation> = caption
        .translations
        .iter()
        .take(CAPTION_MAX_TRANSLATIONS_SCANNED)
        .map(|t| CaptionTranslation {
            language: sanitize_language_tag(&t.language),
            text: body(&t.text),
        })
        .filter(|t| !t.language.is_empty() && !t.text.is_empty())
        .filter(|t| seen.insert(t.language.clone()))
        .collect();
    // The cap must not drop the shown text's own language or the spoken one.
    translations.sort_by_key(|t| match &t.language {
        l if *l == text_language => 0,
        l if *l == source_language => 1,
        _ => 2,
    });
    translations.truncate(CAPTION_MAX_TRANSLATIONS);
    Some(CaptionUpdate::Text(CaptionEvent {
        speaker_session_id: speaker,
        speaker_name: sanitize_caption_name(name_by_session(speaker), &caption.speaker_user_id),
        is_local: is_local_session(speaker),
        utterance_id: caption.utterance_id,
        revision: caption.revision,
        is_final: caption.is_final,
        text: body(&caption.text),
        text_language,
        source_text: body(&caption.source_text),
        source_language,
        timestamp_ms: caption.timestamp_ms,
        translations,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::reactions::REACTION_UNKNOWN_SENDER_NAME;
    use videocall_types::protos::caption_packet::caption_packet;

    fn wrapper(user_id: &[u8], caption: &CaptionPacket) -> PacketWrapper {
        PacketWrapper {
            packet_type: PacketType::CAPTION.into(),
            user_id: user_id.to_vec(),
            session_id: 0,
            data: caption.write_to_bytes().unwrap(),
            ..Default::default()
        }
    }

    fn sample() -> CaptionPacket {
        CaptionPacket {
            speaker_session_id: 77,
            speaker_user_id: b"maria@example.com".to_vec(),
            utterance_id: 5,
            revision: 3,
            is_final: true,
            text: "Good morning".to_string(),
            text_language: "en".to_string(),
            source_text: "Buongiorno".to_string(),
            source_language: "it".to_string(),
            timestamp_ms: 1234,
            kind: Kind::TEXT.into(),
            ..Default::default()
        }
    }

    fn status(kind: Kind, started_by: &[u8]) -> CaptionPacket {
        CaptionPacket {
            kind: kind.into(),
            started_by: started_by.to_vec(),
            ..Default::default()
        }
    }

    fn parse(
        packet: &CaptionPacket,
        by_session: Option<&str>,
        by_user: Option<&str>,
    ) -> Option<CaptionUpdate> {
        caption_update_from_wrapper(
            &wrapper(SYSTEM_USER_ID.as_bytes(), packet),
            |_| by_session.map(str::to_string),
            |_| by_user.map(str::to_string),
            |_| false,
            "me@example.com",
        )
    }

    fn text(update: Option<CaptionUpdate>) -> CaptionEvent {
        match update {
            Some(CaptionUpdate::Text(e)) => e,
            other => panic!("expected text, got {other:?}"),
        }
    }

    fn status_of(update: Option<CaptionUpdate>) -> CaptionStatus {
        match update {
            Some(CaptionUpdate::Status(s)) => s,
            other => panic!("expected status, got {other:?}"),
        }
    }

    #[test]
    fn parses_every_field_and_prefers_the_cached_name() {
        let event = text(caption_update_from_wrapper(
            &wrapper(SYSTEM_USER_ID.as_bytes(), &sample()),
            |sid| (sid == 77).then(|| "Maria".to_string()),
            |_| None,
            |_| false,
            "me@example.com",
        ));
        assert_eq!(
            event,
            CaptionEvent {
                speaker_session_id: 77,
                speaker_name: "Maria".to_string(),
                is_local: false,
                utterance_id: 5,
                revision: 3,
                is_final: true,
                text: "Good morning".to_string(),
                text_language: "en".to_string(),
                source_text: "Buongiorno".to_string(),
                source_language: "it".to_string(),
                timestamp_ms: 1234,
                translations: vec![],
            }
        );
    }

    #[test]
    fn falls_back_to_sanitized_speaker_user_id() {
        let mut packet = sample();
        packet.speaker_user_id = b"\x1b[31mmaria@example.com\n".to_vec();
        assert_eq!(
            text(parse(&packet, None, None)).speaker_name,
            "[31mmaria@example.com"
        );

        packet.speaker_user_id = Vec::new();
        assert_eq!(
            text(parse(&packet, None, None)).speaker_name,
            REACTION_UNKNOWN_SENDER_NAME
        );
    }

    #[test]
    fn marks_the_local_speaker() {
        let event = text(caption_update_from_wrapper(
            &wrapper(SYSTEM_USER_ID.as_bytes(), &sample()),
            |_| None,
            |_| None,
            |sid| sid == 77,
            "me@example.com",
        ));
        assert!(event.is_local);
    }

    #[test]
    fn rejects_captions_not_authored_by_the_server() {
        assert!(caption_update_from_wrapper(
            &wrapper(b"mallory@example.com", &sample()),
            |_| None,
            |_| None,
            |_| false,
            "me@example.com",
        )
        .is_none());
    }

    #[test]
    fn rejects_a_system_caption_carrying_a_session_id() {
        let mut w = wrapper(SYSTEM_USER_ID.as_bytes(), &sample());
        w.session_id = 77;
        assert!(caption_update_from_wrapper(&w, |_| None, |_| None, |_| false, "").is_none());
    }

    #[test]
    fn rejects_other_packet_types_garbage_and_unknown_kinds() {
        let mut w = wrapper(SYSTEM_USER_ID.as_bytes(), &sample());
        w.packet_type = PacketType::REACTION.into();
        assert!(caption_update_from_wrapper(&w, |_| None, |_| None, |_| false, "").is_none());

        let mut w = wrapper(SYSTEM_USER_ID.as_bytes(), &sample());
        w.data = vec![0xff, 0xff, 0xff];
        assert!(caption_update_from_wrapper(&w, |_| None, |_| None, |_| false, "").is_none());

        let mut future = sample();
        future.kind = protobuf::EnumOrUnknown::from_i32(9);
        assert!(parse(&future, None, None).is_none());

        let mut unset = sample();
        unset.kind = Kind::KIND_UNSPECIFIED.into();
        assert!(parse(&unset, None, None).is_none());
    }

    fn translation(language: &str, text: &str) -> caption_packet::Translation {
        caption_packet::Translation {
            language: language.to_string(),
            text: text.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn translations_are_sanitized_deduplicated_and_capped() {
        let mut packet = sample();
        packet.translations = vec![
            translation("en", "Good\u{202E} morning\n"),
            translation("it", "Buongiorno"),
            translation("en", "Second English"),
            translation("", "no language"),
            translation("fr", "  "),
            translation(&"x".repeat(200), &"y".repeat(2000)),
        ];
        packet
            .translations
            .extend((0..20).map(|i| translation(&format!("l{i}"), "z")));
        packet.translations.push(translation("EN", "Third English"));
        let event = text(parse(&packet, None, None));
        let langs: Vec<&str> = event
            .translations
            .iter()
            .map(|t| t.language.as_str())
            .collect();
        assert_eq!(
            &langs[..3],
            ["en", "it", "l0"],
            "first per language wins, case-insensitively; blanks and bad tags dropped"
        );
        assert_eq!(event.translations[0].text, "Good morning");
        assert_eq!(event.translations.len(), CAPTION_MAX_TRANSLATIONS);
        assert!(!langs.iter().any(|l| l.starts_with('x')), "{langs:?}");
    }

    #[test]
    fn a_tail_cut_in_a_long_unspaced_run_keeps_the_run() {
        // The only space is far from the cut: snapping to it would leave "OK".
        let raw = format!("{} OK", "中".repeat(600));
        let tail = sanitize_caption_tail(&raw, CAPTION_TEXT_MAX_CHARS);
        assert_eq!(tail.chars().count(), CAPTION_TEXT_MAX_CHARS);
        assert!(tail.ends_with(" OK") && tail.starts_with(CAPTION_TRUNCATION_MARK));
        // A space inside the window is still used.
        let raw = format!("{} near the cut {}", "x".repeat(600), "y".repeat(480));
        let tail = sanitize_caption_tail(&raw, CAPTION_TEXT_MAX_CHARS);
        assert!(
            tail.starts_with(&format!("{CAPTION_TRUNCATION_MARK}near the cut ")),
            "{tail}"
        );
    }

    #[test]
    fn joiner_runs_are_cut_to_one() {
        assert_eq!(
            sanitize_caption_text("a\u{200D}\u{200D}\u{200C}b", 50),
            "a\u{200D}b"
        );
        assert_eq!(
            sanitize_caption_text(&format!("x{}y", "\u{200C}".repeat(4000)), 50),
            "x\u{200C}y"
        );
    }

    #[test]
    fn translation_entries_past_the_scan_limit_are_not_read() {
        let mut packet = sample();
        packet.text_language = "en".into();
        packet.translations = (0..CAPTION_MAX_TRANSLATIONS_SCANNED + 5)
            .map(|i| translation(&format!("l{i}"), "z"))
            .collect();
        packet.translations.push(translation("en", "late"));
        let event = text(parse(&packet, None, None));
        assert_eq!(event.translations.len(), CAPTION_MAX_TRANSLATIONS);
        assert!(
            !event.translations.iter().any(|t| t.language == "en"),
            "an entry past the scan limit is ignored, even the shown language's"
        );
    }

    #[test]
    fn language_tags_are_lowercased_and_only_well_formed_ones_kept() {
        assert_eq!(sanitize_language_tag(" EN-us "), "en-us");
        assert_eq!(sanitize_language_tag("zh-Hant-TW"), "zh-hant-tw");
        assert_eq!(
            sanitize_language_tag("e\u{202E}n"),
            "en",
            "invisibles go first"
        );
        for bad in ["", "en us", "en_US", "e\u{0301}n", &"a".repeat(36)] {
            assert_eq!(sanitize_language_tag(bad), "", "{bad:?}");
        }
        let mut packet = sample();
        packet.text_language = "IT".into();
        packet.source_language = "a".repeat(36);
        let event = text(parse(&packet, None, None));
        assert_eq!(event.text_language, "it");
        assert_eq!(event.source_language, "");
    }

    #[test]
    fn the_cap_keeps_the_shown_and_spoken_languages() {
        let mut packet = sample();
        packet.text_language = "en".into();
        packet.source_language = "es".into();
        packet.translations = (0..CAPTION_MAX_TRANSLATIONS)
            .map(|i| translation(&format!("l{i}"), "z"))
            .chain([translation("es", "hola"), translation("en", "hello")])
            .collect();
        let event = text(parse(&packet, None, None));
        let langs: Vec<&str> = event
            .translations
            .iter()
            .map(|t| t.language.as_str())
            .collect();
        assert_eq!(langs.len(), CAPTION_MAX_TRANSLATIONS);
        assert_eq!(&langs[..2], ["en", "es"]);
        assert_eq!(&langs[2..], ["l0", "l1", "l2", "l3", "l4", "l5"]);
    }

    #[test]
    fn every_line_break_control_becomes_a_space() {
        assert_eq!(
            sanitize_caption_text("one\u{0085}two\u{000B}three\u{000C}four", 50),
            "one two three four"
        );
    }

    #[test]
    fn a_tail_cut_inside_a_cluster_drops_the_orphaned_marks() {
        let raw = format!("{}e\u{301}{}", "x".repeat(200), "y".repeat(498));
        let tail = sanitize_caption_tail(&raw, CAPTION_TEXT_MAX_CHARS);
        assert_eq!(
            tail,
            format!("{CAPTION_TRUNCATION_MARK}{}", "y".repeat(498))
        );
    }

    #[test]
    fn an_interim_translation_keeps_its_tail_like_the_text() {
        let mut packet = sample();
        packet.is_final = false;
        packet.translations = vec![translation(
            "en",
            &format!("{} newest words", "old ".repeat(400)),
        )];
        let event = text(parse(&packet, None, None));
        let t = &event.translations[0].text;
        assert!(
            t.starts_with(CAPTION_TRUNCATION_MARK) && t.ends_with("newest words"),
            "{t}"
        );
    }

    #[test]
    fn caps_and_cleans_untrusted_text() {
        let mut packet = sample();
        packet.text = format!("line one\nline\u{0007} two{}", "x".repeat(2000));
        packet.text_language = "e".repeat(200);
        let event = text(parse(&packet, None, None));
        assert!(event.text.starts_with("line one line two"));
        assert_eq!(event.text.chars().count(), CAPTION_TEXT_MAX_CHARS);
        assert!(event.text_language.is_empty(), "an overlong tag is no tag");
    }

    #[test]
    fn an_overlong_interim_keeps_its_most_recent_words() {
        let mut packet = sample();
        packet.is_final = false;
        let words: Vec<String> = (0..200).map(|i| format!("w{i}")).collect();
        packet.text = words.join(" ");
        let event = text(parse(&packet, None, None));
        assert!(event.text.chars().count() <= CAPTION_TEXT_MAX_CHARS);
        assert!(event.text.ends_with("w198 w199"), "{}", event.text);
        assert!(event.text.starts_with(CAPTION_TRUNCATION_MARK));
        let first_word = event.text[CAPTION_TRUNCATION_MARK.len_utf8()..]
            .split(' ')
            .next()
            .unwrap();
        assert!(words.iter().any(|w| w == first_word), "{first_word}");
        assert!(!event.text.contains("w0 "));
    }

    #[test]
    fn an_overlong_interim_source_text_also_keeps_its_tail() {
        let mut packet = sample();
        packet.is_final = false;
        let words: Vec<String> = (0..200).map(|i| format!("p{i}")).collect();
        packet.source_text = words.join(" ");
        let event = text(parse(&packet, None, None));
        assert!(event.source_text.chars().count() <= CAPTION_TEXT_MAX_CHARS);
        assert!(event.source_text.starts_with(CAPTION_TRUNCATION_MARK));
        assert!(
            event.source_text.ends_with("p198 p199"),
            "{}",
            event.source_text
        );

        packet.is_final = true;
        let event = text(parse(&packet, None, None));
        assert!(event.source_text.starts_with("p0 p1 "));
    }

    #[test]
    fn a_length_cut_leaves_no_trailing_space() {
        assert_eq!(sanitize_caption_text("alpha beta", 6), "alpha");
    }

    #[test]
    fn a_short_interim_is_untouched() {
        assert_eq!(sanitize_caption_tail("  ciao a tutti ", 20), "ciao a tutti");
    }

    #[test]
    fn bidi_and_format_controls_are_stripped_everywhere() {
        let mut packet = sample();
        packet.text = "pay \u{202E}0001\u{202C} now\u{200B}\u{2066}!\u{2069}\u{FEFF}".to_string();
        packet.text_language = "e\u{200F}n".to_string();
        let event = text(parse(&packet, Some("Ma\u{202E}ria\u{E0041}"), None));
        assert_eq!(event.text, "pay 0001 now!");
        assert_eq!(event.text_language, "en");
        assert_eq!(event.speaker_name, "Maria");

        packet.speaker_user_id = "eve\u{202E}@example.com".as_bytes().to_vec();
        assert_eq!(
            text(parse(&packet, None, None)).speaker_name,
            "eve@example.com"
        );

        let s = status_of(parse(
            &status(Kind::STATUS_ACTIVE, b"host@example.com"),
            None,
            Some("\u{2067}Host\u{2069}"),
        ));
        assert_eq!(s.started_by_name, "Host");
    }

    #[test]
    fn status_active_carries_the_resolved_starter() {
        let s = status_of(parse(
            &status(Kind::STATUS_ACTIVE, b"host@example.com"),
            None,
            Some("Hana"),
        ));
        assert_eq!(
            s,
            CaptionStatus {
                active: true,
                started_by_user_id: "host@example.com".to_string(),
                started_by_name: "Hana".to_string(),
                started_by_local: false,
            }
        );

        let s = status_of(parse(
            &status(Kind::STATUS_ACTIVE, b"host@example.com"),
            None,
            None,
        ));
        assert_eq!(s.started_by_name, "host@example.com");

        let s = status_of(parse(
            &status(Kind::STATUS_ACTIVE, b"me@example.com"),
            None,
            None,
        ));
        assert!(s.started_by_local);
    }

    #[test]
    fn status_stopped_is_inactive_with_no_starter() {
        let s = status_of(parse(
            &status(Kind::STATUS_STOPPED, b"host@example.com"),
            None,
            Some("Hana"),
        ));
        assert!(!s.active);
        assert!(s.started_by_name.is_empty());
        assert!(s.started_by_user_id.is_empty());
        assert!(!s.started_by_local);
    }

    #[test]
    fn the_interim_tail_starts_at_a_word_boundary() {
        assert_eq!(
            sanitize_caption_tail("alpha beta gamma", 9),
            "\u{2026}gamma"
        );
        assert_eq!(
            sanitize_caption_tail("alpha beta gamma", 11),
            "\u{2026}beta gamma"
        );
        assert_eq!(sanitize_caption_tail("abcdefghij", 5), "\u{2026}ghij");
    }

    #[test]
    fn joiners_survive_for_persian_and_indic_text() {
        for s in [
            "\u{0645}\u{06CC}\u{200C}\u{062E}\u{0648}\u{0627}\u{0647}\u{0645}",
            "\u{0915}\u{094D}\u{200D}\u{0937}",
        ] {
            assert_eq!(sanitize_caption_text(s, CAPTION_TEXT_MAX_CHARS), s);
        }
    }

    #[test]
    fn non_format_invisibles_are_stripped() {
        for c in [
            '\u{FE00}',
            '\u{FE0F}',
            '\u{E0100}',
            '\u{E01EF}',
            '\u{034F}',
            '\u{180B}',
            '\u{180D}',
            '\u{180F}',
            '\u{115F}',
            '\u{1160}',
            '\u{3164}',
            '\u{FFA0}',
            '\u{2800}',
        ] {
            assert_eq!(
                sanitize_caption_text(&format!("a{c}b"), CAPTION_TEXT_MAX_CHARS),
                "ab",
                "U+{:04X}",
                c as u32
            );
        }
        assert_eq!(
            sanitize_caption_text("one\u{2028}two\u{2029}three", CAPTION_TEXT_MAX_CHARS),
            "one two three"
        );
    }

    #[test]
    fn combining_mark_runs_are_capped_per_base_character() {
        let zalgo = format!("a{}", "\u{0301}".repeat(CAPTION_TEXT_MAX_CHARS));
        let capped = format!("a{}", "\u{0301}".repeat(CAPTION_MAX_COMBINING_MARKS));
        let mut packet = sample();
        packet.text = zalgo.clone();
        assert_eq!(text(parse(&packet, None, None)).text, capped);
        assert_eq!(
            sanitize_caption_text(&format!("{zalgo}b{zalgo}"), CAPTION_TEXT_MAX_CHARS),
            format!("{capped}b{capped}")
        );

        let joined = format!("{capped}\u{200D}\u{0301}");
        assert_eq!(
            sanitize_caption_text(&joined, CAPTION_TEXT_MAX_CHARS),
            format!("{capped}\u{200D}")
        );
    }

    #[test]
    fn legitimate_scripts_with_stacked_marks_survive_byte_for_byte() {
        for s in [
            // Hindi.
            "\u{0915}\u{094D}\u{0937}\u{093F} \u{0938}\u{094D}\u{0924}\u{094D}\u{0930}\u{0940}",
            // Burmese: one consonant carrying 5 marks.
            "\u{101C}\u{103B}\u{103E}\u{1031}\u{102C}\u{1037}",
            // Pointed Hebrew with cantillation: 5 marks on one letter.
            "\u{05D1}\u{05BC}\u{05B0}\u{0591}\u{05BD}\u{05C1}",
        ] {
            assert_eq!(
                sanitize_caption_text(s, CAPTION_TEXT_MAX_CHARS).as_bytes(),
                s.as_bytes()
            );
        }
    }

    #[test]
    fn the_starter_user_id_is_sanitized_and_capped() {
        let raw = format!("\u{202E}host\u{200B}@example.com{}", "x".repeat(200));
        let s = status_of(parse(
            &status(Kind::STATUS_ACTIVE, raw.as_bytes()),
            None,
            None,
        ));
        assert!(s.started_by_user_id.starts_with("host@example.comxxx"));
        assert_eq!(s.started_by_user_id.chars().count(), CAPTION_NAME_MAX_CHARS);
    }

    #[test]
    fn only_an_active_status_from_our_own_user_is_local() {
        let s = status_of(parse(
            &status(Kind::STATUS_STOPPED, b"me@example.com"),
            None,
            None,
        ));
        assert!(!s.started_by_local);

        let s = status_of(caption_update_from_wrapper(
            &wrapper(SYSTEM_USER_ID.as_bytes(), &status(Kind::STATUS_ACTIVE, b"")),
            |_| None,
            |_| None,
            |_| false,
            "",
        ));
        assert!(!s.started_by_local);
        assert!(s.started_by_user_id.is_empty());
    }

    #[test]
    fn markup_is_preserved_as_text_for_the_renderer_to_escape() {
        assert_eq!(
            sanitize_caption_text("  <b>hi</b> & bye  ", CAPTION_TEXT_MAX_CHARS),
            "<b>hi</b> & bye"
        );
    }
}
