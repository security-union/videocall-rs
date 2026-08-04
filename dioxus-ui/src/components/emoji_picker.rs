// SPDX-License-Identifier: MIT OR Apache-2.0

//! The standard-emoji picker panel for the CUSTOM reaction (issue 1884),
//! extracted into its own child component so it forms an isolated reactive
//! scope.
//!
//! PERF (issue 1884 perf review): the category grid can hold ~388 buttons (e.g.
//! People & Body). Reading `emoji_group` HERE — not inline in the ~9,000-line
//! `AttendantsComponent` — means an UNRELATED attendants re-render (speaking
//! indicators, stats ticks, peer churn) while the picker sits open no longer
//! re-pays the ~388-VNode + ~776-String grid rebuild: this child is memoized on
//! its props (the `emoji_group` signal handle + the send handler), so an
//! unrelated attendants re-render does not reach it. Since issue 2141 it
//! re-renders when the selected category OR the search query changes: `query` is
//! a hook on THIS component and is read unconditionally in the render body, so
//! every keystroke re-renders it — by design (that is what a type-ahead is), and
//! still bounded by the cap below. What the isolation buys is unchanged: the
//! picker's cost is paid for picker input, never for peer churn. Mirrors the
//! `ReactionsOverlay` isolation.
//!
//! SEARCH (issue 2141): a type-ahead field filters the WHOLE emoji table — 1914
//! entries, the default-skin-tone set `emojis::iter()` yields, measured not
//! guessed — by CLDR name and GitHub shortcode. The 1884 invariant above —
//! "never mount the full table" — is preserved by a hard
//! [`EMOJI_SEARCH_RESULT_CAP`] on the rendered result set: the query `"a"`
//! matches 1423 emoji but mounts 60 buttons, which is STRICTLY FEWER than the
//! smallest category grid (Activities, 85), so search can never become the
//! heaviest MOUNT in this panel. (Heaviest mount, not heaviest render — see the
//! cap's own docs: a search render is cheaper in DOM nodes and dearer in CPU.)
//! The scan itself allocates nothing per emoji (byte-wise
//! case/separator-insensitive compare over `&'static str`); its only transient
//! cost is one 8-byte reference per MATCH. It runs to COMPLETION regardless of
//! the cap, and the reason is the RANKING, not the count: results are ordered
//! exact > prefix > substring, so a Prefix hit at table index 1900 has to
//! outrank a Substring hit at index 10, and an early exit at 60 raw hits would
//! ship whichever 60 came first in CLDR order rather than the best 60. The exact
//! match TOTAL falls out of that same complete pass for free, and drives the
//! "showing first N of M" affordance and the screen-reader announcement.
//!
//! DOC CORRECTION (issue 2141): the line above used to read "~3800-emoji
//! table", and `attendants.rs` said "~3600". Both were wrong —
//! `emojis::iter()` (and the union of `Group::emojis()`) is 1914 entries, pinned
//! by `search_and_category_sizes_are_what_the_docs_claim` below so the number
//! cannot silently rot again on a crate bump.
//!
//! All the search decisions that are worth testing are pure functions
//! ([`normalize_emoji_query`], [`emoji_match_rank`], [`search_emojis`],
//! [`emoji_search_status`], [`emoji_search_announcement`]) so they are covered
//! by plain `#[test]`s at the bottom of this file rather than by render-level
//! assertions.

use dioxus::prelude::*;
use gloo_timers::callback::Timeout;
use videocall_client::validate_custom_emoji;
use wasm_bindgen::JsCast;

/// Stable DOM/testid slug + human label for an emoji-picker category (issue
/// 1884). The slug is the `emoji-group-{slug}` testid token; the label is the
/// accessible tab name. Exhaustive over `emojis::Group`, so a future crate
/// bump that adds a group fails to compile here rather than shipping an
/// unlabeled tab.
pub fn emoji_group_meta(group: emojis::Group) -> (&'static str, &'static str) {
    use emojis::Group::*;
    match group {
        SmileysAndEmotion => ("smileys-and-emotion", "Smileys & Emotion"),
        PeopleAndBody => ("people-and-body", "People & Body"),
        AnimalsAndNature => ("animals-and-nature", "Animals & Nature"),
        FoodAndDrink => ("food-and-drink", "Food & Drink"),
        TravelAndPlaces => ("travel-and-places", "Travel & Places"),
        Activities => ("activities", "Activities"),
        Objects => ("objects", "Objects"),
        Symbols => ("symbols", "Symbols"),
        Flags => ("flags", "Flags"),
    }
}

// ─────────────────────────── issue 2141: search ───────────────────────────

/// Maximum number of search-result buttons mounted at once (issue 2141).
///
/// This is the guard rail on issue 1884's "never mount the full table"
/// design. A broad query matches most of the 1914-entry table (`"a"` matches
/// 1423, `"face"` 129); without a cap the results grid would mount every one of
/// them and undo the per-category isolation the picker exists to provide.
///
/// 60 is chosen against the panel's real geometry, not picked round: the
/// palette is `max-width: min(360px, 100% - 32px)` and `.emoji-picker__grid` is
/// `repeat(auto-fill, minmax(40px, 1fr))` pinned at 220px tall, which measures
/// 7 columns x ~5 visible rows ≈ 35 cells filling the grid's viewport. 60 is
/// therefore ~1.7 screenfuls — enough that the result list still scrolls and
/// does not feel truncated at a glance — while remaining under the SMALLEST
/// category grid (Activities, 85 emoji) and ~6.5x under the largest (People &
/// Body, 388).
///
/// SCOPE OF THAT CLAIM (issue 2141 perf review): search is the lightest render
/// this panel performs **by DOM node count** — and only by that measure. It is
/// NOT the cheapest in CPU, because a search render also pays a full 1914-entry
/// table scan that a category render does not: measured, search is ~155 us of
/// scan + ~16 us of button VNodes (~171 us), against ~9.5 us of collect + ~103
/// us of buttons (~113 us) for the largest category. The cap bounds the thing
/// that PERSISTS — mounted VNodes and their event listeners, which every
/// subsequent diff re-walks — while the scan is a transient that ends with the
/// render. That is the trade this constant is making; do not "restore" a
/// cheaper-in-every-dimension invariant that never existed.
///
/// [`search_emojis`] enforces this and reports the true total separately so the
/// UI can say "showing first 60 of 137" rather than silently lying about the
/// match count.
pub const EMOJI_SEARCH_RESULT_CAP: usize = 60;

/// Debounce (ms) before the search result count reaches the polite live region
/// (issue 2141). Typing `smile` fires five `input` events; announcing each one
/// would talk over the user mid-word. The timer restarts on every keystroke, so
/// exactly ONE utterance lands, ~this long after typing stops.
///
/// Chosen to sit above a fast typist's inter-key interval (~120-200ms) and
/// below the ~500ms at which a screen-reader user starts to wonder whether
/// anything happened.
pub const EMOJI_SEARCH_ANNOUNCE_DEBOUNCE_MS: u32 = 350;

/// Hard cap on the typed query, mirrored by the input's `maxlength`. The longest
/// CLDR emoji name is well under this; the cap exists so a paste cannot produce
/// an unbounded string to scan against or to echo into the empty state.
pub const EMOJI_SEARCH_QUERY_MAX_CHARS: usize = 64;

/// How much of the query is echoed back in the visible "no matches" state before
/// it is elided. Keeps a long paste from blowing out the 360px panel.
const EMOJI_SEARCH_ECHO_MAX_CHARS: usize = 24;

/// DOM id of the search field. A constant because it is referenced from THREE
/// places — the `id` attribute, and two imperative `getElementById` focus
/// restores — and a silent drift between them degrades to "focus quietly does
/// not move", which no compiler and no render assertion would catch.
const EMOJI_SEARCH_INPUT_ID: &str = "emoji-search-input";

/// DOM id of the results grid. Referenced from the `id` attribute, the field's
/// `aria-controls` (a DANGLING value there is an axe `aria-valid-attr-value`
/// failure, not a cosmetic one) and the first-option focus selector.
const EMOJI_GRID_ID: &str = "emoji-picker-grid";

/// Viewport width (px) below which the device is treated as a phone. Mirrors
/// `canvas_generator::is_mobile_viewport`'s threshold so "narrow" means one
/// thing across the UI.
const AUTOFOCUS_MIN_VIEWPORT_WIDTH_PX: f64 = 768.0;

/// Viewport height (px) below which `.reactions-palette` hits its
/// `max-height: calc(100dvh - 124px)` cap and becomes a near-full-screen
/// scroller. Kept in lockstep with the `@media (max-height: 520px)` block in
/// `style.css`, which shrinks the results grid at exactly this point.
const AUTOFOCUS_MIN_VIEWPORT_HEIGHT_PX: f64 = 520.0;

/// How well an emoji matched the query. Ordered best-first, so the derived `Ord`
/// IS the result ordering (issue 2141).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EmojiMatchRank {
    /// The whole name or a whole shortcode equals the query (`joy` -> `:joy:`).
    Exact,
    /// A name or shortcode STARTS with the query (`smil` -> `smiling face`).
    Prefix,
    /// A name or shortcode contains the query anywhere (`ok` -> `smoking`).
    Substring,
}

/// The bounded outcome of one search (issue 2141).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmojiSearchResults {
    /// The emoji actually rendered — at most [`EMOJI_SEARCH_RESULT_CAP`],
    /// best-rank first.
    pub shown: Vec<&'static emojis::Emoji>,
    /// How many SENDABLE emoji matched in total, before the cap. Always
    /// `>= shown.len()`.
    pub total: usize,
}

/// Normalize a raw query into the needle actually matched against (issue 2141).
///
/// Trims surrounding whitespace and strips ONE leading and ONE trailing `:` so
/// the way people actually paste shortcodes — `:joy:`, `:joy` — searches the
/// shortcode `joy` rather than failing on the punctuation. Case and
/// `_`/`-`/space differences are NOT handled here; they are folded during the
/// comparison itself (see [`emoji_match_rank`]) so no allocation is needed per
/// candidate.
pub fn normalize_emoji_query(raw: &str) -> &str {
    let trimmed = raw.trim();
    let trimmed = trimmed.strip_prefix(':').unwrap_or(trimmed);
    let trimmed = trimmed.strip_suffix(':').unwrap_or(trimmed);
    trimmed.trim()
}

/// Fold one byte for comparison: ASCII case-insensitive, and `_`, `-` and space
/// are all the same separator. That single rule is what lets `grinning_face`,
/// `grinning-face` and `Grinning Face` all find the CLDR name `grinning face`.
#[inline]
fn fold(b: u8) -> u8 {
    match b {
        b'_' | b'-' | b' ' => b' ',
        _ => b.to_ascii_lowercase(),
    }
}

#[inline]
fn fold_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| fold(*x) == fold(*y))
}

#[inline]
fn fold_starts_with(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len() && fold_eq(&haystack[..needle.len()], needle)
}

/// Case/separator-folded substring test.
///
/// Deliberately byte-wise rather than char-wise: it allocates nothing, and the
/// haystacks (CLDR names, GitHub shortcodes) are overwhelmingly ASCII. A needle
/// could in principle align mid-codepoint inside a non-ASCII name and produce a
/// false positive; the only consequence is one extra suggestion in a search
/// list, never an unsendable emoji (every candidate is still gated on
/// `validate_custom_emoji`).
fn fold_contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if haystack.len() < needle.len() {
        return false;
    }
    (0..=haystack.len() - needle.len()).any(|i| fold_eq(&haystack[i..i + needle.len()], needle))
}

/// Rank how well `emoji` matches an already-[normalized](normalize_emoji_query)
/// `needle`, or `None` if it does not match at all (issue 2141).
///
/// BOTH the CLDR name and EVERY GitHub shortcode are searched. Name-only would
/// miss the way people actually reach for emoji — `:joy` is a shortcode, not a
/// name (that emoji's CLDR name is "face with tears of joy"), and `+1` has no
/// name resemblance to 👍 at all. The best (lowest) rank across all haystacks
/// wins, so an exact shortcode hit outranks an incidental substring hit in some
/// unrelated name.
pub fn emoji_match_rank(emoji: &emojis::Emoji, needle: &str) -> Option<EmojiMatchRank> {
    if needle.is_empty() {
        return None;
    }
    let n = needle.as_bytes();
    let mut best: Option<EmojiMatchRank> = None;

    // `.chain` over name + shortcodes so the ranking rule is written once.
    for hay in std::iter::once(emoji.name()).chain(emoji.shortcodes()) {
        let h = hay.as_bytes();
        let rank = if fold_eq(h, n) {
            EmojiMatchRank::Exact
        } else if fold_starts_with(h, n) {
            EmojiMatchRank::Prefix
        } else if fold_contains(h, n) {
            EmojiMatchRank::Substring
        } else {
            continue;
        };
        if rank == EmojiMatchRank::Exact {
            // Nothing can beat Exact — stop scanning this emoji's shortcodes.
            return Some(EmojiMatchRank::Exact);
        }
        best = Some(match best {
            Some(prev) => prev.min(rank),
            None => rank,
        });
    }
    best
}

/// Search the whole emoji table for `raw_query`, bounded by
/// [`EMOJI_SEARCH_RESULT_CAP`] (issue 2141).
///
/// Ordering is by [`EmojiMatchRank`] (exact, then prefix, then substring), and
/// WITHIN a rank the crate's canonical CLDR order is preserved — the same order
/// the category grids render — so results feel like a filtered view of the same
/// catalogue rather than a reshuffled one.
///
/// Every candidate is gated on `validate_custom_emoji`, exactly like the
/// category grid, so a search result can never be an emoji the send path would
/// reject. The gate runs AFTER the (cheaper, allocation-free) name/shortcode
/// match so a keystroke pays at most one hash lookup per MATCH rather than one
/// per table entry.
///
/// The scan runs to completion because the RANKING requires it, not because of
/// `total`: a Prefix hit at table index 1900 must outrank a Substring hit at
/// index 10, so an early exit once 60 raw hits were in hand would ship the first
/// 60 in CLDR order rather than the best 60. `total` is an exact count that the
/// same complete pass yields for free.
///
/// The transient cost is one 8-byte `&'static Emoji` per match, spread across
/// the three rank buckets and then `shown`. Measured peak for the worst query in
/// the table (`"a"`, 1423 matches) is ~14.4 KB — the figure includes `Vec`
/// capacity overshoot and `shown` living alongside the buckets, both of which a
/// length-only count misses. 14.4 KB freed at the end of a render is noise
/// against one 640x480 I420 frame (460 KB), and it buys a DOM saving of 1363
/// buttons.
///
/// The per-bucket caps that used to sit here were removed in issue 2141: they
/// made `shown.truncate` unfalsifiable — mutating it stayed green because the
/// buckets had already done its job — and they bought ~33 us and ~14 KB in the
/// worst case, which is not worth a test that cannot fail.
///
/// There is exactly ONE place the cap is enforced (the `truncate` below) and
/// exactly ONE place the empty needle is rejected (`emoji_match_rank`).
/// Both used to be double-guarded; the redundant copies were removed in issue
/// 2141 because they made the tests unfalsifiable — a mutation of either
/// enforcement point stayed green while the other silently covered for it.
pub fn search_emojis(raw_query: &str) -> EmojiSearchResults {
    let needle = normalize_emoji_query(raw_query);
    let mut exact: Vec<&'static emojis::Emoji> = Vec::new();
    let mut prefix: Vec<&'static emojis::Emoji> = Vec::new();
    let mut substring: Vec<&'static emojis::Emoji> = Vec::new();
    let mut total = 0usize;

    for emoji in emojis::iter() {
        let Some(rank) = emoji_match_rank(emoji, needle) else {
            continue;
        };
        if !validate_custom_emoji(emoji.as_str()) {
            continue;
        }
        total += 1;
        match rank {
            EmojiMatchRank::Exact => exact.push(emoji),
            EmojiMatchRank::Prefix => prefix.push(emoji),
            EmojiMatchRank::Substring => substring.push(emoji),
        }
    }

    let mut shown = exact;
    shown.extend(prefix);
    shown.extend(substring);
    shown.truncate(EMOJI_SEARCH_RESULT_CAP);
    EmojiSearchResults { shown, total }
}

/// The VISIBLE result-count line (issue 2141). `None` when there is nothing
/// useful to say: a zero-match query is described by the empty state instead, so
/// returning a count line there would duplicate it.
///
/// When the cap bit, this is the ONLY place the user learns their query matched
/// more than they can see — a silently truncated grid is the bug this prevents.
pub fn emoji_search_status(total: usize, shown: usize) -> Option<String> {
    if total == 0 {
        return None;
    }
    if total > shown {
        Some(format!(
            "Showing first {shown} of {total} \u{2014} refine your search"
        ))
    } else if total == 1 {
        Some("1 emoji".to_string())
    } else {
        Some(format!("{total} emoji"))
    }
}

/// The SCREEN-READER announcement for a completed search (issue 2141).
///
/// Separate from [`emoji_search_status`] on purpose: the visible line sits next
/// to the field and the grid, so it can be terse, while the announcement is
/// heard with no surrounding context and therefore names the query and says
/// explicitly when the list was truncated. Deliberately quote-free — several
/// screen readers read `"` aloud as "quote".
pub fn emoji_search_announcement(query: &str, total: usize, shown: usize) -> String {
    if total == 0 {
        format!("No emoji found for {query}")
    } else if total > shown {
        format!("{total} emoji found for {query}, showing the first {shown}")
    } else if total == 1 {
        format!("1 emoji found for {query}")
    } else {
        format!("{total} emoji found for {query}")
    }
}

/// Elide the echoed query in the visible empty state so a long paste cannot
/// blow out the 360px palette. Character- (not byte-) based so it never splits a
/// codepoint.
fn elide_query(query: &str) -> String {
    if query.chars().count() <= EMOJI_SEARCH_ECHO_MAX_CHARS {
        return query.to_string();
    }
    let head: String = query.chars().take(EMOJI_SEARCH_ECHO_MAX_CHARS).collect();
    format!("{head}\u{2026}")
}

/// Move DOM focus to the first button in the results grid (issue 2141).
///
/// Selector-based rather than id-based on purpose: giving all ~388 category
/// buttons an `id` just so the first one is addressable would add a `String`
/// allocation per button to the picker's heaviest render, which is precisely the
/// cost issue 1884's perf review set out to avoid. Fail-safe: if the grid is
/// empty (no matches) nothing is focused and focus stays in the field.
fn focus_first_emoji_option() {
    if let Some(el) = gloo_utils::document()
        .query_selector(&format!("#{EMOJI_GRID_ID} button"))
        .ok()
        .flatten()
    {
        if let Ok(html) = el.dyn_into::<web_sys::HtmlElement>() {
            let _ = html.focus();
        }
    }
}

/// Return DOM focus to the search field (issue 2141).
///
/// Three callers, one idiom: the clear control (which unmounts itself on click),
/// Escape from inside the results, and ArrowUp from a grid button. Fail-safe by
/// construction — if the field is gone the call is a no-op and focus stays where
/// the browser left it, rather than being thrown to `<body>`.
fn focus_search_input() {
    if let Some(el) = gloo_utils::document().get_element_by_id(EMOJI_SEARCH_INPUT_ID) {
        if let Ok(html) = el.dyn_into::<web_sys::HtmlElement>() {
            let _ = html.focus();
        }
    }
}

/// Should the search field claim focus when the picker opens? (issue 2141)
///
/// Split from the DOM read below so the DECISION is a pure function with plain
/// `#[test]` coverage — the landscape-phone case that this exists to fix is
/// otherwise only reachable on a real device.
///
/// FALSE on a touch-primary device: focusing a text field there raises the soft
/// keyboard, and `.reactions-palette` is `position: fixed; bottom: 104px`, so
/// the keyboard lands on top of the thing the user just opened. Browsing
/// categories by thumb is the common case on touch and must not be ambushed by a
/// keyboard nobody asked for.
///
/// FALSE also on a viewport too small to hold the palette comfortably — checked
/// on BOTH axes, which is the actual bug fixed here. The previous gate was
/// `!is_mobile_viewport()`, and that helper tests WIDTH only (< 768px): a
/// landscape phone is 844x390, so it classified as *desktop* and DID autofocus,
/// into a 390px-tall viewport. That is the worst case, not an exempt one.
///
/// A laptop with a touchscreen reports `(any-pointer: coarse)` but
/// `(pointer: fine)`, so `pointer_coarse` is false there and it keeps the
/// autofocus it should keep.
pub fn should_autofocus_search_field(pointer_coarse: bool, width_px: f64, height_px: f64) -> bool {
    !pointer_coarse
        && width_px >= AUTOFOCUS_MIN_VIEWPORT_WIDTH_PX
        && height_px >= AUTOFOCUS_MIN_VIEWPORT_HEIGHT_PX
}

/// Read the three inputs [`should_autofocus_search_field`] needs out of the DOM.
fn should_autofocus_search() -> bool {
    let Some(win) = web_sys::window() else {
        // No window: there is no field to focus either.
        return false;
    };
    // `(pointer: coarse)` is "the PRIMARY pointing device is coarse", i.e. the
    // device whose focus raises an on-screen keyboard. Orientation-blind, unlike
    // any viewport heuristic. An unreadable `matchMedia` degrades to `false`
    // ("assume a fine pointer") because the size thresholds below still catch
    // every phone-shaped viewport on their own.
    let pointer_coarse = win
        .match_media("(pointer: coarse)")
        .ok()
        .flatten()
        .is_some_and(|mql| mql.matches());
    // Defaults are deliberately generous: an unreadable dimension must not
    // silently suppress the autofocus on a desktop, where it is the whole point.
    let width = win
        .inner_width()
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(1024.0);
    let height = win
        .inner_height()
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(768.0);
    should_autofocus_search_field(pointer_coarse, width, height)
}

/// The polite live region carrying the debounced search-result count (issue
/// 2141).
///
/// Its own component so the picker does NOT read `announcement` in its render
/// body: a debounced write would otherwise re-render the picker (and re-run the
/// table scan) ~350ms after every typing pause, for a string the picker never
/// displays. Same isolation trick as `ReactionsOverlay` / the picker itself.
///
/// `role="status"` already implies `aria-live="polite"`; both are written out to
/// match the existing `reaction-live-region` markup verbatim, and they AGREE —
/// a `role="status"` with `aria-live="off"` would be a self-contradiction that
/// silences the region.
#[component]
fn EmojiSearchLiveRegion(announcement: ReadSignal<String>) -> Element {
    rsx! {
        div {
            class: "visually-hidden",
            id: "emoji-search-status",
            "data-testid": "emoji-search-live",
            role: "status",
            "aria-live": "polite",
            "aria-atomic": "true",
            "{announcement}"
        }
    }
}

/// The standard-emoji picker panel (CUSTOM reaction, issue 1884): a search
/// field (issue 2141), category toggles, and a scrollable grid showing EITHER
/// the bounded search results OR the SELECTED category only, so the full
/// 1914-emoji table is never mounted at once. `emoji_group` is read + written
/// here (tab click), and each grid button calls `send_custom_reaction` with its
/// glyph. Rendered inside `.reactions-palette` by `AttendantsComponent` only
/// while the picker is open; Arrow/Home/End keydowns are stopped here so the
/// palette's roving handler does not yank focus back to the quick row (Escape
/// and Tab still bubble/work). The recents quick-picks stay in the palette (the
/// parent), not here.
///
/// SEARCH STATE LIFETIME (issue 2141): `query` is a hook on THIS component, and
/// the component is mounted behind `if emoji_picker_open()`. Closing the picker
/// therefore drops the query with the scope — a reopened picker always starts
/// on a clean, unfiltered category view, never mid-search. That mirrors the
/// existing rule that a reopened PALETTE never starts mid-picker.
#[component]
pub fn EmojiPicker(
    mut emoji_group: Signal<emojis::Group>,
    send_custom_reaction: EventHandler<String>,
) -> Element {
    let mut query = use_signal(String::new);
    // Written ONLY by the debounce timer below and read ONLY by
    // `EmojiSearchLiveRegion`; the picker passes the handle without reading it.
    let mut announcement = use_signal(String::new);
    // The debounce timer is HELD, never `.forget()`ed: replacing it cancels the
    // previous one (gloo's `Timeout` cancels on drop) and dropping this signal
    // when the picker unmounts cancels the last one, so a pending announcement
    // can never fire against a dead scope (Dioxus 0.7 `set()` is
    // `try_write().unwrap()` -> panic).
    let mut announce_timer: Signal<Option<Timeout>> = use_signal(|| None);

    // Recompute the (bounded) result set for the current query. Runs on the
    // picker's own re-render only — the parent never re-renders it for this.
    let raw_query = query();
    let needle = normalize_emoji_query(&raw_query);
    let searching = !needle.is_empty();
    let results = searching.then(|| search_emojis(&raw_query));

    // Announce the result count on a debounce. Subscribes to `query` only; the
    // scan inside the closure runs once per typing PAUSE, not once per
    // keystroke, and its cost is off the render critical path.
    use_effect(move || {
        let raw = query();
        if normalize_emoji_query(&raw).is_empty() {
            // Clearing is immediate: cancel any pending announcement and empty
            // the region (an empty live region utters nothing, and resetting it
            // means the NEXT search announces even if it repeats this one).
            // Both writes are change-guarded through `peek()` (non-subscribing):
            // an unconditional `set()` dirties every subscriber even when the
            // value is unchanged, which is what defeated `PeerTile` memoization
            // in #2103.
            if announce_timer.peek().is_some() {
                announce_timer.set(None);
            }
            if !announcement.peek().is_empty() {
                announcement.set(String::new());
            }
            return;
        }
        let timer = Timeout::new(EMOJI_SEARCH_ANNOUNCE_DEBOUNCE_MS, move || {
            let found = search_emojis(&raw);
            let text = emoji_search_announcement(
                normalize_emoji_query(&raw),
                found.total,
                found.shown.len(),
            );
            // try_write, not set: belt-and-braces against a fire racing an
            // unmount even though holding the Timeout should have cancelled it.
            if let Ok(mut a) = announcement.try_write() {
                *a = text;
            }
        });
        announce_timer.set(Some(timer));
    });

    // Read ONCE, up here: the tabs loop below shadows the name `group` with its
    // own loop variable, so comparing against a re-read `emoji_group()` inside
    // the loop would be both a redundant signal read per tab and an easy place
    // for the two to drift.
    let selected_group = emoji_group();
    let (_, group_label) = emoji_group_meta(selected_group);
    // ONE grid loop feeds from either source, so the option markup (and its
    // `React with {name}` label) cannot drift between the two modes.
    let options: Vec<&'static emojis::Emoji> = match &results {
        Some(found) => found.shown.clone(),
        None => selected_group
            .emojis()
            .filter(|e| validate_custom_emoji(e.as_str()))
            .collect(),
    };
    let status_line = results
        .as_ref()
        .and_then(|found| emoji_search_status(found.total, found.shown.len()));
    let grid_label = if searching {
        "Emoji search results".to_string()
    } else {
        format!("{group_label} emoji")
    };

    rsx! {
        div {
            class: "emoji-picker",
            "data-testid": "emoji-picker",
            role: "group",
            "aria-label": "Choose an emoji",
            onkeydown: move |evt: Event<KeyboardData>| {
                let key = evt.key();
                if key == Key::Escape {
                    // ESCAPE TIERING, tier 2 (issue 2141). Reaching the picker
                    // ROOT means the field did not already handle this: either
                    // focus is IN the field with an empty query (its own handler
                    // deliberately lets that bubble), or focus is on a result
                    // button / the clear control. In the second case there is
                    // still a layer to peel — "you are down in the results" — so
                    // Escape climbs back to the field instead of tearing the
                    // whole palette down, which is what #main-container's
                    // one-surface-per-Escape rule asks for. With no query there
                    // IS no results layer, so nothing is peeled here and the
                    // event bubbles on to close the palette exactly as it did
                    // before 2141.
                    if !query.peek().is_empty() {
                        evt.stop_propagation();
                        evt.prevent_default();
                        focus_search_input();
                    }
                } else if key == Key::ArrowUp {
                    // Mirror of the field's ArrowDown-into-results: ArrowUp from
                    // a grid button (or a category tab) climbs back to the
                    // field, so the search loop is escapable from the keyboard
                    // without Shift+Tab past every option. The field's own
                    // handler stops ArrowUp before it reaches here, so a caret
                    // in the text keeps its native start-of-line behaviour.
                    evt.stop_propagation();
                    evt.prevent_default();
                    focus_search_input();
                } else if key == Key::ArrowRight
                    || key == Key::ArrowLeft
                    || key == Key::ArrowDown
                    || key == Key::Home
                    || key == Key::End
                {
                    evt.stop_propagation();
                }
            },
            // Issue 2141: the search field. `<input type="search">` already has
            // the implicit ARIA role `searchbox`, so an explicit
            // `role="searchbox"` would be redundant — and `role="search"` is a
            // LANDMARK for a search *region*, which is the wrong thing entirely
            // inside a transient toolbar popover. `aria-label` gives it a real
            // accessible name (no visible <label> fits a 360px palette) and
            // `aria-controls` names the grid it filters.
            //
            // NO `aria-describedby` (issue 2141 review). It used to point at the
            // live region below, which is a documented anti-pattern: a
            // `role="status"` node dual-purposed as an accessible DESCRIPTION is
            // re-read in full by NVDA/JAWS on every refocus — by then stale —
            // and double-announces on the tick the region fires. It was also
            // empty at mount, which is the one moment focus actually lands in
            // the field, so the description said nothing precisely when it was
            // read. Repointing it at the visible `.emoji-picker__status`
            // instead would not do: that node is conditionally rendered (no
            // query, or zero matches -> absent), so the IDREF would dangle,
            // which is the exact defect the always-mounted grid container below
            // exists to avoid. The count reaches screen readers through the
            // debounced live region, which is what a live region is for.
            div { class: "emoji-picker__search",
                input {
                    id: EMOJI_SEARCH_INPUT_ID,
                    class: "emoji-picker__search-input",
                    "data-testid": "emoji-search-input",
                    r#type: "search",
                    "aria-label": "Search emoji by name or shortcode",
                    "aria-controls": EMOJI_GRID_ID,
                    autocomplete: "off",
                    autocapitalize: "none",
                    // Relabels the soft keyboard's action key from "Go"/"Return"
                    // to a search affordance. Cosmetic on a hardware keyboard,
                    // load-bearing on the touch devices this panel now guards
                    // its autofocus against.
                    enterkeyhint: "search",
                    spellcheck: false,
                    maxlength: EMOJI_SEARCH_QUERY_MAX_CHARS as i64,
                    placeholder: "Search emoji",
                    value: "{raw_query}",
                    oninput: move |evt: Event<FormData>| query.set(evt.value()),
                    onclick: move |e: MouseEvent| e.stop_propagation(),
                    onkeydown: move |evt: Event<KeyboardData>| {
                        let key = evt.key();
                        if key == Key::Escape {
                            // ESCAPE TIERING, matching #main-container's rule
                            // that each Escape peels EXACTLY ONE surface: with a
                            // query typed, Escape clears the query and stays
                            // put; with the field already empty it bubbles and
                            // closes the whole palette, exactly as Escape does
                            // from anywhere else in the picker today. The
                            // `prevent_default` also suppresses Chrome's native
                            // `type=search` Escape-to-revert, which would clear
                            // the DOM value out from under the signal.
                            if !query.peek().is_empty() {
                                evt.stop_propagation();
                                evt.prevent_default();
                                query.set(String::new());
                            }
                        } else if key == Key::ArrowUp {
                            // Swallowed HERE so the picker root's "ArrowUp
                            // climbs back to the field" rule cannot fire on the
                            // field itself: inside the text, ArrowUp must keep
                            // its native move-caret-to-start behaviour.
                            evt.stop_propagation();
                        } else if key == Key::ArrowDown || key == Key::Enter {
                            // Into the results. Enter deliberately FOCUSES the
                            // top result instead of sending it: activating a
                            // reaction broadcasts to every attendee, and a
                            // reflexive Enter after typing must not become an
                            // accidental all-hands emoji. One more Enter, on the
                            // now-focused button, sends it.
                            evt.stop_propagation();
                            evt.prevent_default();
                            focus_first_emoji_option();
                        }
                    },
                    // `autofocus` is only honoured on initial page load, not on
                    // insertion into a live DOM, so focus is set explicitly on
                    // mount (same idiom as `search_modal.rs`). See
                    // `should_autofocus_search` for when it is suppressed.
                    //
                    // FIRES ONCE PER ELEMENT CREATION, never on re-render, so it
                    // cannot steal the caret mid-word: dioxus-core 0.7.3
                    // hard-codes `(Listener, Listener) => false` in
                    // `diff/node.rs::attribute_changed`, and listeners are never
                    // `volatile`, so the `if volatile || attribute_changed`
                    // guard that is the only route into `write_attribute` — and
                    // thus the only caller of `create_event_listener`, where
                    // dioxus-web intercepts `name == "mounted"` — is false on
                    // every re-render. This input also sits outside every
                    // conditional branch with an identical attribute set each
                    // render, so the element itself is never recreated.
                    onmounted: move |evt| async move {
                        if should_autofocus_search() {
                            let _ = evt.data.set_focus(true).await;
                        }
                    },
                }
                // Rendered only with a query present, so there is never a
                // control with nothing to clear. Focus returns to the field
                // because this button is about to unmount — the same
                // focus-before-unmount idiom as the recents reset (issue 2086).
                if !raw_query.is_empty() {
                    button {
                        class: "emoji-picker__search-clear",
                        r#type: "button",
                        "data-testid": "emoji-search-clear",
                        "aria-label": "Clear emoji search",
                        title: "Clear emoji search",
                        onclick: move |e: MouseEvent| {
                            e.stop_propagation();
                            query.set(String::new());
                            focus_search_input();
                        },
                        span { "aria-hidden": "true", "\u{00d7}" }
                    }
                }
            }
            EmojiSearchLiveRegion { announcement }
            // Category switcher: a group of toggle buttons (one representative
            // glyph per group). Deliberately NOT role=tab/tablist — a full ARIA
            // tabs widget also needs a tabpanel + arrow-key roving, which we do
            // not implement; a group of `aria-pressed` toggles is the honest,
            // non-misleading contract.
            //
            // Issue 2141: while a query is active the strip stays fully ENABLED
            // (clicking a category is how you leave search) but NO tab reports
            // itself pressed — the grid below is showing search results, not
            // that category, and an `aria-pressed="true"` on a category that is
            // not on screen is exactly the inverted-state defect of #2123/#2135.
            // The `active` class is dropped in lockstep, so the visual and the
            // ARIA state never disagree.
            div {
                class: "emoji-picker__tabs",
                role: "group",
                "aria-label": "Emoji categories",
                for group in emojis::Group::iter() {
                    {
                        let (slug, label) = emoji_group_meta(group);
                        let selected = !searching && selected_group == group;
                        let tab_glyph = group.emojis().next().map(|e| e.as_str()).unwrap_or("");
                        rsx! {
                            button {
                                key: "{slug}",
                                class: if selected { "emoji-tab active" } else { "emoji-tab" },
                                r#type: "button",
                                "aria-pressed": if selected { "true" } else { "false" },
                                "data-testid": "emoji-group-{slug}",
                                "aria-label": "{label}",
                                title: "{label}",
                                onclick: move |e: MouseEvent| {
                                    e.stop_propagation();
                                    // Picking a category LEAVES search: the grid
                                    // must show the category the user just
                                    // pressed. Peek-guarded so an already-empty
                                    // query is not needlessly dirtied.
                                    if !query.peek().is_empty() {
                                        query.set(String::new());
                                    }
                                    emoji_group.set(group);
                                },
                                span { class: "reaction-option__emoji", "aria-hidden": "true", "{tab_glyph}" }
                            }
                        }
                    }
                }
            }
            // Visible result summary. `aria-hidden` because the live region
            // above carries the same information on a debounce — without this
            // the count would be both announced AND read again while browsing.
            if let Some(status) = status_line {
                div {
                    class: "emoji-picker__status",
                    "data-testid": "emoji-search-status-text",
                    "aria-hidden": "true",
                    "{status}"
                }
            }
            // Scrollable grid: bounded search results when searching, otherwise
            // the selected category. Emoji over the byte cap are filtered out
            // (via validate_custom_emoji) in BOTH modes so every shown option is
            // sendable. testid index is 0-based within whichever grid is shown.
            //
            // The container is ALWAYS rendered, even with zero matches, because
            // the field's `aria-controls` names its id: unmounting it on an empty
            // result would leave a dangling IDREF, which is an invalid
            // `aria-controls` value (axe `aria-valid-attr-value`) — the field
            // would claim to control an element that is not in the document. The
            // no-match state therefore renders INSIDE it, and the `--empty`
            // modifier replaces `display: grid` with a centring flex box so the
            // message is not laid out inside a single 40px column. That modifier
            // is written as a COMPOUND selector in style.css; as a lone class it
            // tied on specificity with `.emoji-picker__grid` and lost on source
            // order, which made it silently inert.
            div {
                class: if searching && options.is_empty() {
                    "emoji-picker__grid emoji-picker__grid--empty"
                } else {
                    "emoji-picker__grid"
                },
                id: EMOJI_GRID_ID,
                "data-testid": "emoji-picker-grid",
                role: "group",
                "aria-label": "{grid_label}",
                if searching && options.is_empty() {
                    div {
                        class: "emoji-picker__empty",
                        "data-testid": "emoji-search-empty",
                        p { class: "emoji-picker__empty-title",
                            "No emoji match \u{201c}{elide_query(needle)}\u{201d}"
                        }
                        p { class: "emoji-picker__empty-hint",
                            "Try a name like heart, or a shortcode like :joy:"
                        }
                    }
                } else {
                    for (i, emoji) in options.iter().enumerate() {
                        {
                            let glyph = emoji.as_str().to_string();
                            let name = emoji.name().to_string();
                            rsx! {
                                button {
                                    key: "{glyph}",
                                    class: "reaction-option emoji-option",
                                    r#type: "button",
                                    "data-testid": "emoji-option-{i}",
                                    "aria-label": "React with {name}",
                                    title: "{name}",
                                    onclick: move |e: MouseEvent| {
                                        e.stop_propagation();
                                        send_custom_reaction.call(glyph.clone());
                                    },
                                    span { class: "reaction-option__emoji", "aria-hidden": "true", "{glyph}" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Issue 2141: the search predicates, the cap, the ordering and both text
    //! builders are pure, so they are exercised here as plain `#[test]`s
    //! (`cargo test -p videocall-ui --lib`) rather than through the DOM.
    use super::*;

    /// A stable handle on a well-known emoji so the tests read as intent.
    fn e(glyph: &str) -> &'static emojis::Emoji {
        emojis::get(glyph).expect("glyph is in the emoji table")
    }

    /// The `@bvt1` Playwright spec types `rocket` and asserts the top result's
    /// label is exactly `React with rocket`. That only proves the RESULTS grid
    /// (rather than the category grid still on screen) if `rocket` is an
    /// unambiguous single hit — so pin the premise here, where a crate bump
    /// fails a local unit test instead of flaking an e2e run in CI.
    #[test]
    fn rocket_is_the_unique_exact_hit_the_e2e_spec_relies_on() {
        let found = search_emojis("rocket");
        assert_eq!(
            found.total, 1,
            "e2e premise: `rocket` must match exactly one"
        );
        let top = found.shown.first().expect("one result");
        assert_eq!(top.name(), "rocket");
        assert_eq!(emoji_match_rank(top, "rocket"), Some(EmojiMatchRank::Exact));
    }

    #[test]
    fn normalize_strips_shortcode_colons_and_whitespace() {
        assert_eq!(normalize_emoji_query("  joy  "), "joy");
        assert_eq!(normalize_emoji_query(":joy:"), "joy");
        assert_eq!(normalize_emoji_query(":joy"), "joy");
        assert_eq!(normalize_emoji_query("joy:"), "joy");
        // Only ONE colon per side is stripped, so `::` collapses to empty
        // rather than leaving a stray colon that matches nothing.
        assert_eq!(normalize_emoji_query("::"), "");
        assert_eq!(normalize_emoji_query("   "), "");
    }

    #[test]
    fn shortcode_search_finds_what_name_search_cannot() {
        // 😂's CLDR name is "face with tears of joy" — a NAME-ONLY search for
        // "joy" would still find it, so the load-bearing case is 👍, whose name
        // ("thumbs up") shares nothing with its `+1` shortcode.
        let thumbs = e("\u{1f44d}");
        assert_eq!(
            emoji_match_rank(thumbs, "+1"),
            Some(EmojiMatchRank::Exact),
            "shortcode +1 must be searchable"
        );
        assert_eq!(
            emoji_match_rank(thumbs, "thumbs up"),
            Some(EmojiMatchRank::Exact),
            "CLDR name must be searchable"
        );
        // Separator folding: `_`, `-` and space are interchangeable.
        assert_eq!(
            emoji_match_rank(thumbs, "thumbs_up"),
            Some(EmojiMatchRank::Exact)
        );
        assert_eq!(
            emoji_match_rank(thumbs, "THUMBS-UP"),
            Some(EmojiMatchRank::Exact)
        );
        assert_eq!(
            emoji_match_rank(thumbs, "thumb"),
            Some(EmojiMatchRank::Prefix)
        );
        assert_eq!(
            emoji_match_rank(thumbs, "umbs"),
            Some(EmojiMatchRank::Substring)
        );
        assert_eq!(emoji_match_rank(thumbs, "zzzz"), None);
        assert_eq!(emoji_match_rank(thumbs, ""), None);
    }

    #[test]
    fn results_are_ranked_exact_then_prefix_then_substring() {
        let found = search_emojis("joy");
        assert!(!found.shown.is_empty());
        // Whatever the table order, every Exact precedes every Prefix which
        // precedes every Substring — the ordering contract, checked against the
        // production ranker rather than a re-implementation of it.
        let ranks: Vec<EmojiMatchRank> = found
            .shown
            .iter()
            .map(|em| emoji_match_rank(em, "joy").expect("shown results matched"))
            .collect();
        assert!(
            ranks.windows(2).all(|w| w[0] <= w[1]),
            "results must be ordered best-rank-first, got {ranks:?}"
        );
        // `:joy:` is an exact shortcode hit, so it must be FIRST, not merely
        // present.
        assert_eq!(
            found.shown.first().map(|em| em.as_str()),
            Some("\u{1f602}"),
            "an exact shortcode hit must outrank substring hits"
        );
    }

    #[test]
    fn within_a_rank_the_catalogue_order_is_preserved() {
        // The buckets are filled by a single forward pass over `emojis::iter()`
        // and concatenated, so equal-rank results keep CLDR order — search
        // reads as a filtered view of the same catalogue the tabs show.
        let found = search_emojis("face");
        let catalogue: Vec<&str> = emojis::iter().map(|em| em.as_str()).collect();
        let same_rank: Vec<usize> = found
            .shown
            .iter()
            .filter(|em| emoji_match_rank(em, "face") == Some(EmojiMatchRank::Prefix))
            .map(|em| {
                catalogue
                    .iter()
                    .position(|c| *c == em.as_str())
                    .expect("result came from the catalogue")
            })
            .collect();
        assert!(same_rank.len() > 1, "need >1 prefix hit to test ordering");
        assert!(
            same_rank.windows(2).all(|w| w[0] < w[1]),
            "equal-rank results must keep catalogue order, got {same_rank:?}"
        );
    }

    #[test]
    fn broad_query_is_capped_but_reports_the_true_total() {
        // "a" matches most of the table. This is THE issue-2141 invariant: the
        // rendered set stays bounded while the reported total does not.
        let found = search_emojis("a");
        assert_eq!(
            found.shown.len(),
            EMOJI_SEARCH_RESULT_CAP,
            "a broad query must fill the cap exactly, never exceed it"
        );
        assert!(
            found.total > EMOJI_SEARCH_RESULT_CAP,
            "expected the broad query to overflow the cap, got {}",
            found.total
        );
        // And the cap really is below the SMALLEST category grid, which is what
        // makes search the lightest render this panel performs (issue 1884).
        let smallest_category = emojis::Group::iter()
            .map(|g| {
                g.emojis()
                    .filter(|em| validate_custom_emoji(em.as_str()))
                    .count()
            })
            .min()
            .expect("emojis::Group is non-empty");
        assert!(
            EMOJI_SEARCH_RESULT_CAP < smallest_category,
            "cap {EMOJI_SEARCH_RESULT_CAP} must stay under the smallest category grid \
             ({smallest_category} buttons) or search becomes the heaviest render"
        );
    }

    /// Pins every table/category size quoted in this module's docs and in
    /// `attendants.rs`, so an `emojis` crate bump that moves them turns the
    /// numbers into a test failure instead of a stale comment. The old "~3800" /
    /// "~3600" claims went unchallenged precisely because nothing measured them.
    #[test]
    fn search_and_category_sizes_are_what_the_docs_claim() {
        assert_eq!(emojis::iter().count(), 1914, "documented table size");
        let per_group: Vec<usize> = emojis::Group::iter()
            .map(|g| {
                g.emojis()
                    .filter(|em| validate_custom_emoji(em.as_str()))
                    .count()
            })
            .collect();
        assert_eq!(per_group.iter().sum::<usize>(), 1914);
        assert_eq!(per_group.iter().min().copied(), Some(85), "Activities");
        assert_eq!(per_group.iter().max().copied(), Some(388), "People & Body");
        // The broad-query figure quoted in the module doc.
        assert_eq!(search_emojis("a").total, 1423);
    }

    #[test]
    fn every_result_is_sendable_and_unique() {
        // The category grid filters on `validate_custom_emoji`; search must keep
        // that filter or it offers emoji the send path rejects.
        //
        // HONEST SCOPE: `videocall-client`'s own
        // `custom_emoji_cap_admits_every_standard_emoji` proves nothing in
        // `emojis::iter()` currently exceeds the 32-byte cap, so today this
        // filter rejects zero emoji and this loop cannot fail by removing it.
        // It is a CONTRACT pin, not a discriminating assertion: it fails the day
        // a crate bump introduces a longer sequence, which is exactly when
        // dropping the filter would start offering unsendable options. The
        // discriminating tests in this module are the cap, ordering and text
        // ones below/above.
        for q in ["a", "flag", "family", "keycap", "joy"] {
            let found = search_emojis(q);
            for em in &found.shown {
                assert!(
                    validate_custom_emoji(em.as_str()),
                    "search offered an unsendable emoji {:?} for query {q}",
                    em.as_str()
                );
            }
            let mut seen: Vec<&str> = found.shown.iter().map(|em| em.as_str()).collect();
            let before = seen.len();
            seen.sort_unstable();
            seen.dedup();
            assert_eq!(before, seen.len(), "duplicate results for {q}");
            assert!(found.total >= found.shown.len());
        }
    }

    #[test]
    fn empty_and_punctuation_only_queries_search_nothing() {
        for q in ["", "   ", "::", ":"] {
            let found = search_emojis(q);
            assert!(found.shown.is_empty(), "{q:?} must not search");
            assert_eq!(found.total, 0, "{q:?} must not search");
        }
    }

    #[test]
    fn no_match_query_yields_the_empty_state_not_a_blank_grid() {
        let found = search_emojis("zzqqxx");
        assert_eq!(found.total, 0);
        assert!(found.shown.is_empty());
        // Zero matches has no count line — the empty state speaks instead.
        assert_eq!(emoji_search_status(found.total, found.shown.len()), None);
        assert_eq!(
            emoji_search_announcement("zzqqxx", found.total, found.shown.len()),
            "No emoji found for zzqqxx"
        );
    }

    #[test]
    fn status_line_flags_truncation_and_pluralises() {
        assert_eq!(emoji_search_status(0, 0), None);
        assert_eq!(emoji_search_status(1, 1).as_deref(), Some("1 emoji"));
        assert_eq!(emoji_search_status(12, 12).as_deref(), Some("12 emoji"));
        assert_eq!(
            emoji_search_status(137, EMOJI_SEARCH_RESULT_CAP).as_deref(),
            Some("Showing first 60 of 137 \u{2014} refine your search")
        );
    }

    #[test]
    fn announcement_names_the_query_and_says_when_truncated() {
        assert_eq!(
            emoji_search_announcement("smile", 0, 0),
            "No emoji found for smile"
        );
        assert_eq!(
            emoji_search_announcement("joy", 1, 1),
            "1 emoji found for joy"
        );
        assert_eq!(
            emoji_search_announcement("smile", 12, 12),
            "12 emoji found for smile"
        );
        assert_eq!(
            emoji_search_announcement("a", 2451, 60),
            "2451 emoji found for a, showing the first 60"
        );
        // No ASCII quotes — several screen readers read them aloud.
        assert!(!emoji_search_announcement("a", 2451, 60).contains('"'));
    }

    #[test]
    fn long_query_echo_is_elided() {
        let short = "smile";
        assert_eq!(elide_query(short), short);
        let long = "x".repeat(EMOJI_SEARCH_ECHO_MAX_CHARS + 10);
        let elided = elide_query(&long);
        assert_eq!(elided.chars().count(), EMOJI_SEARCH_ECHO_MAX_CHARS + 1);
        assert!(elided.ends_with('\u{2026}'));
        // Multi-byte input must not be split mid-codepoint.
        let wide = "\u{4e16}".repeat(EMOJI_SEARCH_ECHO_MAX_CHARS + 5);
        assert_eq!(
            elide_query(&wide).chars().count(),
            EMOJI_SEARCH_ECHO_MAX_CHARS + 1
        );
    }

    // ── issue 2141 review: the mobile-occlusion fix ──────────────────────────
    //
    // The defect these guard: the search field was unreachable on a phone. Three
    // independent causes, so three independent guards — the pure predicate here,
    // and the CSS/HTML halves pinned against the SHIPPED files below, because
    // neither has a compiler and both were shipped broken once already.

    /// The SHIPPED stylesheet, so the pins below assert against the real file
    /// rather than a copy of what it is supposed to say.
    const SHIPPED_CSS: &str = include_str!("../../static/style.css");
    /// The SHIPPED page shell, for the viewport-meta pin.
    const SHIPPED_INDEX_HTML: &str = include_str!("../../index.html");

    /// Drop `/* ... */` blocks so a selector merely DISCUSSED in a comment does
    /// not count as a selector that is declared.
    fn strip_css_comments(css: &str) -> String {
        let mut out = String::with_capacity(css.len());
        let mut rest = css;
        while let Some(start) = rest.find("/*") {
            out.push_str(&rest[..start]);
            match rest[start + 2..].find("*/") {
                Some(end) => rest = &rest[start + 2 + end + 2..],
                None => {
                    rest = "";
                    break;
                }
            }
        }
        out.push_str(rest);
        out
    }

    #[test]
    fn autofocus_is_suppressed_where_the_field_would_be_swallowed() {
        // DESKTOP: the case the autofocus exists for. Type-ahead is the whole
        // point of the field, so a fine pointer on a roomy viewport must keep
        // it — including Playwright's default 1280x720, which every @bvt1 emoji
        // spec depends on.
        assert!(should_autofocus_search_field(false, 1440.0, 900.0));
        assert!(should_autofocus_search_field(false, 1280.0, 720.0));

        // LANDSCAPE PHONE — the regression this predicate was written for.
        // 844x390: width alone (the pre-review gate, `!is_mobile_viewport()`,
        // was `width >= 768`) calls this a DESKTOP and autofocuses into a 390px
        // viewport, which is the worst case rather than an exempt one. Both the
        // coarse pointer and the short viewport must independently veto it, so
        // deleting either clause fails here.
        assert!(!should_autofocus_search_field(true, 844.0, 390.0));
        assert!(
            !should_autofocus_search_field(false, 844.0, 390.0),
            "a 390px-tall viewport must veto autofocus even if the pointer reads fine"
        );

        // PORTRAIT PHONE: caught on width, as it always was.
        assert!(!should_autofocus_search_field(true, 390.0, 844.0));
        assert!(!should_autofocus_search_field(false, 390.0, 844.0));

        // TABLET: roomy on both axes, so only the pointer can veto it — and it
        // must, because a tablet raises a soft keyboard just like a phone.
        assert!(!should_autofocus_search_field(true, 820.0, 1180.0));
        assert!(should_autofocus_search_field(false, 820.0, 1180.0));

        // Boundaries are inclusive on the "allowed" side, matching `>=`.
        assert!(should_autofocus_search_field(
            false,
            AUTOFOCUS_MIN_VIEWPORT_WIDTH_PX,
            AUTOFOCUS_MIN_VIEWPORT_HEIGHT_PX
        ));
        assert!(!should_autofocus_search_field(
            false,
            AUTOFOCUS_MIN_VIEWPORT_WIDTH_PX - 1.0,
            AUTOFOCUS_MIN_VIEWPORT_HEIGHT_PX
        ));
        assert!(!should_autofocus_search_field(
            false,
            AUTOFOCUS_MIN_VIEWPORT_WIDTH_PX,
            AUTOFOCUS_MIN_VIEWPORT_HEIGHT_PX - 1.0
        ));
    }

    #[test]
    fn short_viewport_threshold_is_in_lockstep_with_the_css_media_query() {
        // NOT `X == X`: this reads the shipped stylesheet and requires the media
        // query that shrinks the results grid to break at the SAME height the
        // autofocus gate does. Move either number alone and this fails.
        let css = strip_css_comments(SHIPPED_CSS);
        let query = format!(
            "@media (max-height: {}px)",
            AUTOFOCUS_MIN_VIEWPORT_HEIGHT_PX as u32
        );
        let at = css.get_or_panic(&query);
        // Char-bounded rather than byte-sliced: the stylesheet is not ASCII.
        let following: String = css[at..].chars().take(240).collect();
        assert!(
            following.contains(".emoji-picker__grid"),
            "the {query} block must be the one that resizes the emoji grid"
        );
    }

    #[test]
    fn empty_grid_modifier_outranks_the_grid_rule_it_overrides() {
        // The lone-class form `.emoji-picker__grid--empty { ... }` ties with
        // `.emoji-picker__grid` on specificity (0,1,0) and loses on source
        // order, so it ships INERT — measured on the version that did: the
        // zero-match message rendered 44px wide and 320px tall with its title
        // wrapped over eight lines inside one 40px grid column. The compound
        // form is (0,2,0) and wins wherever either rule is later moved to.
        let css = strip_css_comments(SHIPPED_CSS);
        let compound = ".emoji-picker__grid.emoji-picker__grid--empty";
        assert!(
            css.contains(compound),
            "the --empty modifier must be declared as a compound selector"
        );
        // Every mention of the modifier must be part of the compound form. A
        // re-added lone-class rule would reintroduce the tie silently, and it
        // would still LOOK like a fix in review.
        assert_eq!(
            css.matches(".emoji-picker__grid--empty").count(),
            css.matches(compound).count(),
            "a lone `.emoji-picker__grid--empty` selector is back and is inert"
        );
    }

    #[test]
    fn palette_is_height_bounded_and_the_keyboard_resizes_the_layout_viewport() {
        // The palette is `position: fixed; bottom: 104px` and grows UPWARD, so
        // without a cap the ~400px picker column runs off the top of a short
        // viewport and clips the search field, which is first in the column.
        let css = strip_css_comments(SHIPPED_CSS);
        let at = css.get_or_panic(".reactions-palette {");
        let rest = &css[at..];
        let end = rest
            .find('}')
            .expect("`.reactions-palette` block must close");
        let block = &rest[..end];
        assert!(
            block.contains("max-height: calc(100dvh - 124px)"),
            "`.reactions-palette` must be height-bounded against the viewport"
        );
        assert!(
            block.contains("max-height: calc(100vh - 124px)"),
            "and must keep the `vh` fallback for engines without `dvh`"
        );
        assert!(
            block.contains("overflow-y: auto"),
            "a bounded palette must scroll rather than clip"
        );

        // Without this the layout viewport stays full-height when the soft
        // keyboard opens, so every bottom-anchored fixed surface — this palette
        // included — sits behind the keyboard.
        //
        // Asserted against the meta tag's `content` VALUE, not against the file:
        // a plain `contains` over index.html was satisfied by the HTML comment
        // that explains the directive, so deleting the directive itself left the
        // test green. (Found by mutating it — the failure mode this whole test
        // block exists to catch.)
        assert!(
            viewport_meta_content(SHIPPED_INDEX_HTML)
                .contains("interactive-widget=resizes-content"),
            "the viewport meta must let the soft keyboard resize the layout viewport"
        );
    }

    /// The `content` attribute of `<meta name="viewport">`, and nothing else.
    fn viewport_meta_content(html: &str) -> &str {
        let tag_at = html
            .find("name=\"viewport\"")
            .expect("index.html must declare a viewport meta");
        let rest = &html[tag_at..];
        let value_at = rest
            .find("content=\"")
            .expect("the viewport meta must carry a content attribute")
            + "content=\"".len();
        let rest = &rest[value_at..];
        let end = rest.find('"').expect("unterminated content attribute");
        &rest[..end]
    }

    /// `str::find` that reports WHAT was missing instead of unwrapping `None`.
    trait FindOrPanic {
        fn get_or_panic(&self, needle: &str) -> usize;
    }
    impl FindOrPanic for String {
        fn get_or_panic(&self, needle: &str) -> usize {
            self.find(needle)
                .unwrap_or_else(|| panic!("shipped asset no longer contains {needle:?}"))
        }
    }
}
