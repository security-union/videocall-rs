// SPDX-License-Identifier: MIT OR Apache-2.0

//! File-based theming: schema, loader, validation, and DOM application.
//!
//! Theme files override a small set of public semantic CSS custom properties.
//! Unknown keys are ignored; invalid values are skipped. If parsing fails
//! entirely, zero overrides are applied and the CSS fallback wins.

use std::cell::RefCell;

use serde::Deserialize;

// ── Schema ───────────────────────────────────────────────────────────────────

/// Top-level theme file.
#[derive(Debug, Deserialize)]
pub struct ThemeFile {
    pub version: u32,
    #[allow(dead_code)]
    pub name: Option<String>,
    pub color: Option<ColorTokens>,
}

#[derive(Debug, Deserialize)]
pub struct ColorTokens {
    pub surface: Option<SurfaceTokens>,
    pub border: Option<BorderTokens>,
    pub text: Option<TextTokens>,
    pub brand: Option<BrandTokens>,
    pub status: Option<StatusTokens>,
    pub focus: Option<FocusTokens>,
}

#[derive(Debug, Deserialize)]
pub struct SurfaceTokens {
    pub base: Option<ModeValue>,
    pub raised: Option<ModeValue>,
    pub elevated: Option<ModeValue>,
}

#[derive(Debug, Deserialize)]
pub struct BorderTokens {
    pub default: Option<ModeValue>,
    pub emphasis: Option<ModeValue>,
}

#[derive(Debug, Deserialize)]
pub struct TextTokens {
    pub primary: Option<ModeValue>,
    pub secondary: Option<ModeValue>,
    pub error: Option<ModeValue>,
}

#[derive(Debug, Deserialize)]
pub struct BrandTokens {
    pub accent: Option<ModeValue>,
    #[serde(rename = "accent-hover")]
    pub accent_hover: Option<ModeValue>,
}

#[derive(Debug, Deserialize)]
pub struct StatusTokens {
    pub success: Option<ModeValue>,
    pub warning: Option<ModeValue>,
    pub error: Option<ModeValue>,
}

#[derive(Debug, Deserialize)]
pub struct FocusTokens {
    pub ring: Option<ModeValue>,
}

/// Per-token dark/light pair.
#[derive(Debug, Deserialize)]
pub struct ModeValue {
    pub dark: Option<String>,
    pub light: Option<String>,
}

// ── Resolved variant ─────────────────────────────────────────────────────────

/// Which colour-scheme variant to apply (already resolved from Theme + OS).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResolvedVariant {
    Dark,
    Light,
}

impl ResolvedVariant {
    /// Parse from the string that `apply_theme_to_dom` already computes.
    pub fn from_resolved(s: &str) -> Self {
        if s == "light" {
            Self::Light
        } else {
            Self::Dark
        }
    }
}

// ── Allowlist (security boundary) ────────────────────────────────────────────

/// Each entry maps (extractor-fn on ThemeFile, CSS variable name).
type Extractor = fn(&ThemeFile, ResolvedVariant) -> Option<&String>;

fn extract_surface_base(f: &ThemeFile, v: ResolvedVariant) -> Option<&String> {
    mode_pick(f.color.as_ref()?.surface.as_ref()?.base.as_ref()?, v)
}
fn extract_surface_raised(f: &ThemeFile, v: ResolvedVariant) -> Option<&String> {
    mode_pick(f.color.as_ref()?.surface.as_ref()?.raised.as_ref()?, v)
}
fn extract_surface_elevated(f: &ThemeFile, v: ResolvedVariant) -> Option<&String> {
    mode_pick(f.color.as_ref()?.surface.as_ref()?.elevated.as_ref()?, v)
}
fn extract_border_default(f: &ThemeFile, v: ResolvedVariant) -> Option<&String> {
    mode_pick(f.color.as_ref()?.border.as_ref()?.default.as_ref()?, v)
}
fn extract_border_emphasis(f: &ThemeFile, v: ResolvedVariant) -> Option<&String> {
    mode_pick(f.color.as_ref()?.border.as_ref()?.emphasis.as_ref()?, v)
}
fn extract_text_primary(f: &ThemeFile, v: ResolvedVariant) -> Option<&String> {
    mode_pick(f.color.as_ref()?.text.as_ref()?.primary.as_ref()?, v)
}
fn extract_text_secondary(f: &ThemeFile, v: ResolvedVariant) -> Option<&String> {
    mode_pick(f.color.as_ref()?.text.as_ref()?.secondary.as_ref()?, v)
}
fn extract_text_error(f: &ThemeFile, v: ResolvedVariant) -> Option<&String> {
    mode_pick(f.color.as_ref()?.text.as_ref()?.error.as_ref()?, v)
}
fn extract_brand_accent(f: &ThemeFile, v: ResolvedVariant) -> Option<&String> {
    mode_pick(f.color.as_ref()?.brand.as_ref()?.accent.as_ref()?, v)
}
fn extract_brand_accent_hover(f: &ThemeFile, v: ResolvedVariant) -> Option<&String> {
    mode_pick(f.color.as_ref()?.brand.as_ref()?.accent_hover.as_ref()?, v)
}
fn extract_status_success(f: &ThemeFile, v: ResolvedVariant) -> Option<&String> {
    mode_pick(f.color.as_ref()?.status.as_ref()?.success.as_ref()?, v)
}
fn extract_status_warning(f: &ThemeFile, v: ResolvedVariant) -> Option<&String> {
    mode_pick(f.color.as_ref()?.status.as_ref()?.warning.as_ref()?, v)
}
fn extract_status_error(f: &ThemeFile, v: ResolvedVariant) -> Option<&String> {
    mode_pick(f.color.as_ref()?.status.as_ref()?.error.as_ref()?, v)
}
fn extract_focus_ring(f: &ThemeFile, v: ResolvedVariant) -> Option<&String> {
    mode_pick(f.color.as_ref()?.focus.as_ref()?.ring.as_ref()?, v)
}

fn mode_pick(mv: &ModeValue, v: ResolvedVariant) -> Option<&String> {
    match v {
        ResolvedVariant::Dark => mv.dark.as_ref(),
        ResolvedVariant::Light => mv.light.as_ref(),
    }
}

/// The complete allowlist. Only these CSS vars can ever be set by a theme file.
const ALLOWLIST: &[(&str, Extractor)] = &[
    ("--bg", extract_surface_base as Extractor),
    ("--surface", extract_surface_raised),
    ("--surface-elevated", extract_surface_elevated),
    ("--border", extract_border_default),
    ("--border-emphasis", extract_border_emphasis),
    ("--text-primary", extract_text_primary),
    ("--text-secondary", extract_text_secondary),
    ("--accent", extract_brand_accent),
    ("--accent-hover", extract_brand_accent_hover),
    ("--success", extract_status_success),
    ("--warning", extract_status_warning),
    ("--error", extract_status_error),
    ("--error-text", extract_text_error),
    ("--focus-ring", extract_focus_ring),
];

// ── Validation ───────────────────────────────────────────────────────────────

/// Lightweight format check: hex (#rgb/#rrggbb/#rrggbbaa), rgb()/rgba(), hsl()/hsla().
///
/// Security: this is the guard ahead of user-imported theme files. It uses a
/// *positive* grammar rather than a blocklist — a value must be either a hex
/// literal or a single rgb/rgba/hsl/hsla function call with no nested function.
/// Rejecting any second `(` defeats url(), var(), expression(), image-set(),
/// image(), -webkit-image-set(), attr(), etc. in one rule, case-insensitively,
/// without enumerating names.
fn is_valid_color_value(s: &str) -> bool {
    let trimmed = s.trim();

    // Hex literals.
    if let Some(hex) = trimmed.strip_prefix('#') {
        let len = hex.len();
        return (len == 3 || len == 4 || len == 6 || len == 8)
            && hex.chars().all(|c| c.is_ascii_hexdigit());
    }

    // Functional notation — lowercase for case-insensitive structural checks.
    let lower = trimmed.to_ascii_lowercase();
    let is_color_fn = lower.starts_with("rgb(")
        || lower.starts_with("rgba(")
        || lower.starts_with("hsl(")
        || lower.starts_with("hsla(");
    if !is_color_fn {
        return false;
    }

    // Locate the function's opening paren; there must be no further `(` after
    // it (no nested function), and the value must close with `)`.
    let open = match lower.find('(') {
        Some(i) => i,
        None => return false,
    };
    if lower[open + 1..].contains('(') {
        return false;
    }

    trimmed.ends_with(')')
        && !trimmed.contains('{')
        && !trimmed.contains('}')
        && !trimmed.contains(';')
        && !trimmed.contains("/*")
}

// ── Parse + resolve ──────────────────────────────────────────────────────────

/// Errors from theme file parsing.
#[derive(Debug)]
pub enum ThemeFileError {
    Json(serde_json::Error),
    UnsupportedVersion(u32),
}

impl std::fmt::Display for ThemeFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(e) => write!(f, "theme JSON parse error: {e}"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported theme version: {v}"),
        }
    }
}

/// Parse and validate a theme file from JSON.
pub fn parse_theme_file(json: &str) -> Result<ThemeFile, ThemeFileError> {
    let file: ThemeFile = serde_json::from_str(json).map_err(ThemeFileError::Json)?;
    if file.version != 1 {
        return Err(ThemeFileError::UnsupportedVersion(file.version));
    }
    Ok(file)
}

/// Resolve a parsed theme file into a list of (CSS-var-name, validated-value) pairs.
pub fn validate_and_resolve(
    file: &ThemeFile,
    variant: ResolvedVariant,
) -> Vec<(&'static str, String)> {
    let mut pairs = Vec::new();
    for &(css_var, extractor) in ALLOWLIST {
        if let Some(value) = extractor(file, variant) {
            if is_valid_color_value(value) {
                pairs.push((css_var, value.clone()));
            } else {
                log::warn!("theme_file: skipping invalid color value for {css_var}: {value:?}");
            }
        }
    }
    pairs
}

// ── Active theme source (v1: bundled default) ────────────────────────────────

/// Returns the JSON of the currently-active theme file.
/// v1: always the bundled default. Future phases will swap in user-imported files.
fn active_theme_file_json() -> &'static str {
    include_str!("../static/themes/default.json")
}

// ── Resolved-token cache (issue #1440) ───────────────────────────────────────

/// Resolved CSS-var pairs for both variants of the active theme, cached so the
/// bundled `default.json` is parsed (`serde_json::from_str`) at most once per page
/// load instead of on every `apply_theme_file_tokens` call (init + every
/// signal-driven theme change + every OS `prefers-color-scheme` flip).
struct ResolvedBundledTheme {
    dark: Vec<(&'static str, String)>,
    light: Vec<(&'static str, String)>,
}

thread_local! {
    /// Memoized resolve of the bundled theme (issue #1440). The wasm target is
    /// single-threaded, so `thread_local! + RefCell` needs no `Sync` bound.
    ///
    /// V1 KEY: nothing — the source is a compile-time `include_str!` of a trusted
    /// file, so the resolved pairs never change for the page's lifetime. Only a
    /// SUCCESSFUL resolve is cached; a parse failure falls through uncached (the
    /// bundled source is deterministic, so this only matters defensively). When
    /// phase-2 makes the theme source dynamic (user-imported files), the cache key
    /// MUST incorporate the active source so a swapped theme re-parses — see #1411.
    static RESOLVED_THEME_CACHE: RefCell<Option<ResolvedBundledTheme>> =
        const { RefCell::new(None) };
}

/// Resolve the active theme's CSS-var pairs for `variant`, parsing the bundled
/// JSON at most once (issue #1440). Returns `None` on parse failure so the caller
/// applies zero overrides and the CSS fallback stays authoritative.
fn resolved_theme_pairs(variant: ResolvedVariant) -> Option<Vec<(&'static str, String)>> {
    RESOLVED_THEME_CACHE.with(|cache| {
        if cache.borrow().is_none() {
            // Not yet cached — parse + resolve BOTH variants once, then cache.
            let file = match parse_theme_file(active_theme_file_json()) {
                Ok(f) => f,
                Err(e) => {
                    log::warn!("theme_file: failed to parse active theme, using CSS fallback: {e}");
                    return None;
                }
            };
            *cache.borrow_mut() = Some(ResolvedBundledTheme {
                dark: validate_and_resolve(&file, ResolvedVariant::Dark),
                light: validate_and_resolve(&file, ResolvedVariant::Light),
            });
        }
        cache.borrow().as_ref().map(|resolved| match variant {
            ResolvedVariant::Dark => resolved.dark.clone(),
            ResolvedVariant::Light => resolved.light.clone(),
        })
    })
}

// ── DOM application ──────────────────────────────────────────────────────────

/// Remove all managed CSS custom properties from documentElement inline style.
fn clear_theme_overrides() {
    let style = match document_element_style() {
        Some(s) => s,
        None => return,
    };
    for &(var_name, _) in ALLOWLIST {
        let _ = style.remove_property(var_name);
    }
}

/// Apply the active theme file's tokens for the given resolved variant.
/// Called from `apply_theme_to_dom` after setting `data-theme`.
///
/// On any parse/load failure, clears all inline overrides so the CSS fallback
/// remains authoritative.
pub fn apply_theme_file_tokens(resolved_variant_str: &str) {
    // Always clear first — prevents stale dark values shadowing light (or vice-versa).
    clear_theme_overrides();

    let variant = ResolvedVariant::from_resolved(resolved_variant_str);
    // Resolve via the #1440 cache: the bundled JSON is parsed once per page load,
    // not on every apply. `None` = parse failure → leave the cleared state so the
    // CSS fallback wins (same behavior as the prior inline parse-fail return).
    let pairs = match resolved_theme_pairs(variant) {
        Some(p) => p,
        None => return,
    };

    let style = match document_element_style() {
        Some(s) => s,
        None => return,
    };
    for (var_name, value) in pairs {
        let _ = style.set_property(var_name, &value);
    }
}

/// Helper: get the CSSStyleDeclaration of documentElement.
fn document_element_style() -> Option<web_sys::CssStyleDeclaration> {
    use wasm_bindgen::JsCast;
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.document_element())
        .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok())
        .map(|el| el.style())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bundled_default() {
        let file = parse_theme_file(active_theme_file_json()).expect("bundled default must parse");
        assert_eq!(file.version, 1);

        let dark_pairs = validate_and_resolve(&file, ResolvedVariant::Dark);
        assert!(!dark_pairs.is_empty());
        // All 14 tokens should resolve for the bundled default.
        assert_eq!(dark_pairs.len(), 14);

        let light_pairs = validate_and_resolve(&file, ResolvedVariant::Light);
        assert_eq!(light_pairs.len(), 14);
    }

    #[test]
    fn rejects_invalid_version() {
        let json = r#"{"version": 99, "color": {}}"#;
        assert!(matches!(
            parse_theme_file(json),
            Err(ThemeFileError::UnsupportedVersion(99))
        ));
    }

    /// Issue #1440: the cached resolve must be IDENTICAL to a fresh parse+resolve
    /// of the bundled source, for BOTH variants. This pins that the cache returns
    /// the real resolved pairs (not an empty/wrong vec). If the cache stored the
    /// wrong variant's pairs, or cached an empty vec, the equality below fails.
    #[test]
    fn cached_resolve_matches_fresh_resolve_both_variants() {
        let file = parse_theme_file(active_theme_file_json()).expect("bundled must parse");
        let fresh_dark = validate_and_resolve(&file, ResolvedVariant::Dark);
        let fresh_light = validate_and_resolve(&file, ResolvedVariant::Light);

        // First call populates the cache; assert it equals the uncached resolve.
        assert_eq!(
            resolved_theme_pairs(ResolvedVariant::Dark).expect("bundled resolves"),
            fresh_dark,
            "cached dark pairs must equal a fresh resolve"
        );
        assert_eq!(
            resolved_theme_pairs(ResolvedVariant::Light).expect("bundled resolves"),
            fresh_light,
            "cached light pairs must equal a fresh resolve"
        );
        // Both variants are non-empty (14 tokens each) — guards against a cache
        // that silently serves an empty vec.
        assert_eq!(fresh_dark.len(), 14);
        assert_eq!(fresh_light.len(), 14);
    }

    /// Issue #1440: a second call returns the SAME pairs as the first — the cache
    /// is populated once and reused. Variant order must not bleed: a Light call
    /// after a Dark call must still return Light pairs (not the cached Dark).
    #[test]
    fn cache_is_stable_across_calls_and_variants() {
        let dark_a = resolved_theme_pairs(ResolvedVariant::Dark).expect("resolves");
        let light = resolved_theme_pairs(ResolvedVariant::Light).expect("resolves");
        let dark_b = resolved_theme_pairs(ResolvedVariant::Dark).expect("resolves");

        assert_eq!(dark_a, dark_b, "repeated dark calls must be identical");
        assert_ne!(
            dark_a, light,
            "dark and light pairs differ — the cache must not serve one variant for the other"
        );
    }

    #[test]
    fn rejects_css_injection() {
        let json = r##"{
            "version": 1,
            "color": {
                "surface": {
                    "base": {"dark": "red; } html { display:none", "light": "#fff"}
                }
            }
        }"##;
        let file = parse_theme_file(json).unwrap();
        let pairs = validate_and_resolve(&file, ResolvedVariant::Dark);
        // The dark value is rejected, only light would resolve (but we asked for dark).
        assert!(pairs.is_empty());
    }

    #[test]
    fn valid_color_formats() {
        assert!(is_valid_color_value("#fff"));
        assert!(is_valid_color_value("#ffffff"));
        assert!(is_valid_color_value("#ffffffaa"));
        assert!(is_valid_color_value("#abcdef"));
        assert!(is_valid_color_value("rgb(1, 2, 3)"));
        assert!(is_valid_color_value("rgba(1, 2, 3, 0.5)"));
        assert!(is_valid_color_value("hsl(120, 50%, 50%)"));
        assert!(is_valid_color_value("hsla(120, 50%, 50%, 0.8)"));
    }

    #[test]
    fn invalid_color_formats() {
        assert!(!is_valid_color_value("red"));
        assert!(!is_valid_color_value("not-a-color"));
        assert!(!is_valid_color_value("#gg"));
        assert!(!is_valid_color_value("rgb(1,2,3};html{display:none"));
    }

    #[test]
    fn rejects_functional_notation_smuggling() {
        assert!(!is_valid_color_value("rgba(0, url(https://evil/x), 0, 1)"));
        assert!(!is_valid_color_value("rgb(var(--x), 0, 0)"));
        assert!(!is_valid_color_value("rgba(0,0,0,1) /* x */"));
        assert!(!is_valid_color_value("rgb(expression(alert(1)), 0, 0)"));
        // Case-insensitive: uppercase / mixed-case function names must not bypass.
        assert!(!is_valid_color_value("rgb(0,0,0) URL(https://evil/x)"));
        assert!(!is_valid_color_value("rgb(0,0,0) Url(https://evil/x)"));
        assert!(!is_valid_color_value("rgb(0,0,0) uRl(https://evil/x)"));
        // Image-fetching function family must not bypass (was not enumerated).
        assert!(!is_valid_color_value(
            "rgb(0,0,0) image-set('https://evil/x')"
        ));
        assert!(!is_valid_color_value(
            "rgb(0,0,0) IMAGE-SET('https://evil/x')"
        ));
        assert!(!is_valid_color_value("rgb(0,0,0) image('https://evil/x')"));
        assert!(!is_valid_color_value(
            "rgb(0,0,0) -webkit-image-set('https://evil/x')"
        ));
        // attr() / any nested function.
        assert!(!is_valid_color_value("rgb(0,0,0) attr(data-x)"));
    }

    #[test]
    fn garbage_json_yields_error() {
        assert!(parse_theme_file("not json at all").is_err());
        assert!(parse_theme_file("").is_err());
        assert!(parse_theme_file("{}").is_err()); // missing version
    }
}
