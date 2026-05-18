// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure color-space conversions used by the custom HSV color picker.
//!
//! All functions are framework-agnostic and unit-testable on the host.
//! - `hue` is degrees in `[0.0, 360.0)`.
//! - `saturation` and `value` are in `[0.0, 1.0]`.
//! - RGB channels are 8-bit `u8`.

/// Convert HSV to 8-bit RGB.
///
/// `hue` is wrapped into `[0, 360)`. `saturation` and `value` are clamped to
/// `[0, 1]`. Output channels are rounded to the nearest integer.
pub fn hsv_to_rgb(hue: f32, saturation: f32, value: f32) -> (u8, u8, u8) {
    let h = ((hue % 360.0) + 360.0) % 360.0;
    let s = saturation.clamp(0.0, 1.0);
    let v = value.clamp(0.0, 1.0);

    let c = v * s;
    let h_prime = h / 60.0;
    let x = c * (1.0 - ((h_prime % 2.0) - 1.0).abs());
    let (r1, g1, b1) = match h_prime as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    let to_byte = |f: f32| (((f + m) * 255.0).round().clamp(0.0, 255.0)) as u8;
    (to_byte(r1), to_byte(g1), to_byte(b1))
}

/// Convert 8-bit RGB to HSV.
///
/// Returns `(hue, saturation, value)` with hue in `[0, 360)` and the other two
/// in `[0, 1]`. When the color is achromatic (R == G == B) the returned hue is
/// `0.0`; callers that care about marker continuity should track the previous
/// hue separately.
pub fn rgb_to_hsv(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let r = r as f32 / 255.0;
    let g = g as f32 / 255.0;
    let b = b as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;

    let hue = if delta == 0.0 {
        0.0
    } else if max == r {
        60.0 * (((g - b) / delta) % 6.0)
    } else if max == g {
        60.0 * (((b - r) / delta) + 2.0)
    } else {
        60.0 * (((r - g) / delta) + 4.0)
    };
    let hue = if hue < 0.0 { hue + 360.0 } else { hue };

    let saturation = if max == 0.0 { 0.0 } else { delta / max };
    (hue, saturation, max)
}

/// Format an RGB triple as an upper-case `#RRGGBB` string.
pub fn rgb_to_hex(r: u8, g: u8, b: u8) -> String {
    format!("#{r:02X}{g:02X}{b:02X}")
}

/// Parse a 6-digit hex color, with or without a leading `#`. Returns `None`
/// for any other input.
pub fn parse_hex(input: &str) -> Option<(u8, u8, u8)> {
    let trimmed = input.trim();
    let body = trimmed.strip_prefix('#').unwrap_or(trimmed);
    if body.len() != 6 || !body.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&body[0..2], 16).ok()?;
    let g = u8::from_str_radix(&body[2..4], 16).ok()?;
    let b = u8::from_str_radix(&body[4..6], 16).ok()?;
    Some((r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() <= eps
    }

    #[test]
    fn hsv_primary_colors_round_trip() {
        for (h, expected) in [
            (0.0, (255u8, 0u8, 0u8)),
            (60.0, (255, 255, 0)),
            (120.0, (0, 255, 0)),
            (180.0, (0, 255, 255)),
            (240.0, (0, 0, 255)),
            (300.0, (255, 0, 255)),
        ] {
            assert_eq!(hsv_to_rgb(h, 1.0, 1.0), expected, "hue={h}");
        }
    }

    #[test]
    fn hsv_black_and_white() {
        assert_eq!(hsv_to_rgb(0.0, 0.0, 0.0), (0, 0, 0));
        assert_eq!(hsv_to_rgb(0.0, 0.0, 1.0), (255, 255, 255));
        assert_eq!(hsv_to_rgb(123.0, 0.0, 0.5), (128, 128, 128));
    }

    #[test]
    fn rgb_to_hsv_primaries() {
        let (h, s, v) = rgb_to_hsv(255, 0, 0);
        assert!(close(h, 0.0, 0.001));
        assert!(close(s, 1.0, 0.001));
        assert!(close(v, 1.0, 0.001));

        let (h, s, v) = rgb_to_hsv(0, 255, 0);
        assert!(close(h, 120.0, 0.001));
        assert!(close(s, 1.0, 0.001));
        assert!(close(v, 1.0, 0.001));

        let (h, s, v) = rgb_to_hsv(0, 0, 255);
        assert!(close(h, 240.0, 0.001));
        assert!(close(s, 1.0, 0.001));
        assert!(close(v, 1.0, 0.001));
    }

    #[test]
    fn rgb_to_hsv_grayscale_has_zero_saturation() {
        let (_, s, v) = rgb_to_hsv(128, 128, 128);
        assert!(close(s, 0.0, 0.001));
        assert!(close(v, 128.0 / 255.0, 0.001));
    }

    #[test]
    fn round_trip_random_samples() {
        // Round-trip RGB -> HSV -> RGB should be lossless within 1 LSB.
        for &(r, g, b) in &[
            (12u8, 175u8, 255u8),
            (255, 0, 191),
            (221, 160, 221),
            (91, 207, 159),
            (17, 34, 51),
            (200, 200, 50),
        ] {
            let (h, s, v) = rgb_to_hsv(r, g, b);
            let (r2, g2, b2) = hsv_to_rgb(h, s, v);
            assert!(
                (r as i32 - r2 as i32).abs() <= 1
                    && (g as i32 - g2 as i32).abs() <= 1
                    && (b as i32 - b2 as i32).abs() <= 1,
                "{:?} -> ({h},{s},{v}) -> {:?}",
                (r, g, b),
                (r2, g2, b2)
            );
        }
    }

    #[test]
    fn hex_formats_uppercase() {
        assert_eq!(rgb_to_hex(0, 0, 0), "#000000"); // @token-exempt: test fixture
        assert_eq!(rgb_to_hex(255, 255, 255), "#FFFFFF"); // @token-exempt: test fixture
        assert_eq!(rgb_to_hex(12, 175, 255), "#0CAFFF"); // @token-exempt: test fixture
    }

    #[test]
    fn parse_hex_accepts_with_and_without_hash() {
        assert_eq!(parse_hex("#0CAFFF"), Some((12, 175, 255))); // @token-exempt: test fixture
        assert_eq!(parse_hex("0caFFF"), Some((12, 175, 255)));
        assert_eq!(parse_hex("  #FF00BF  "), Some((255, 0, 191))); // @token-exempt: test fixture
    }

    #[test]
    fn parse_hex_rejects_invalid() {
        assert_eq!(parse_hex(""), None);
        assert_eq!(parse_hex("#12345"), None); // @token-exempt: test fixture
        assert_eq!(parse_hex("#1234567"), None); // @token-exempt: test fixture
        assert_eq!(parse_hex("#GGGGGG"), None); // @token-exempt: test fixture
        assert_eq!(parse_hex("red"), None);
    }
}
