// SPDX-License-Identifier: MIT OR Apache-2.0

//! The sparkline inside the signal disc (issue 2661).
//!
//! Markup rather than RSX because peer discs refresh at 1 Hz by writing the DOM
//! directly; one builder keeps mount and refresh from drifting.

use crate::components::signal_quality::{
    SparkPaint, SPARK_X_FIRST, SPARK_X_LAST, SPARK_Y_BOTTOM, SPARK_Y_TOP,
};
use dioxus::prelude::*;
use std::fmt::Write as _;

const SPARK_SLASH: &str = "#FF4444"; // @token-exempt: SVG markup from Rust, not CSS

/// 2.5 CSS px at every disc size. At 3.0 a steep swing self-occludes.
const SPARK_TREND_WIDTH: f64 = 2.5;

const SPARK_SLASH_WIDTH: f64 = 2.0;

const SPARK_HALO: &str = "#000000"; // @token-exempt: SVG markup from Rust, not CSS

/// Black each side of a haloed mark; its cap radius sets the plot box margin.
const SPARK_HALO_MARGIN: f64 = 0.75;
const SPARK_HALO_WIDTH: f64 = SPARK_TREND_WIDTH + 2.0 * SPARK_HALO_MARGIN;
const SPARK_SLASH_HALO_WIDTH: f64 = SPARK_SLASH_WIDTH + 2.0 * SPARK_HALO_MARGIN;

const SPARK_VIEW_W: f64 = 25.5;
/// Taller than the 14 it shipped at: a full halo cap of margin at each end.
const SPARK_VIEW_H: f64 = 16.0;

/// Build the disc's SVG.
///
/// The colour is derived from `paint.level`, never passed in: no caller-supplied
/// text can reach `stroke=` or `fill=`, so the two `innerHTML` sinks are safe by
/// type rather than by caller discipline.
pub fn spark_svg_markup(paint: &SparkPaint) -> String {
    let stroke = paint.level.level_color();

    let mut svg = String::with_capacity(1600);
    let _ = write!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {SPARK_VIEW_W} {SPARK_VIEW_H}" fill="none" aria-hidden="true" focusable="false">"#
    );
    for points in &paint.segments {
        let _ = write!(
            svg,
            r#"<polyline class="spark-halo" points="{points}" stroke="{SPARK_HALO}" stroke-width="{SPARK_HALO_WIDTH}" stroke-linecap="round" stroke-linejoin="round" vector-effect="non-scaling-stroke"/>"#
        );
    }
    for points in &paint.segments {
        let _ = write!(
            svg,
            r#"<polyline points="{points}" stroke="{stroke}" stroke-width="{SPARK_TREND_WIDTH}" stroke-linecap="round" stroke-linejoin="round" vector-effect="non-scaling-stroke"/>"#
        );
    }
    if paint.level.is_lost() {
        for (class, colour, width) in [
            (r#" class="spark-halo""#, SPARK_HALO, SPARK_SLASH_HALO_WIDTH),
            ("", SPARK_SLASH, SPARK_SLASH_WIDTH),
        ] {
            let _ = write!(
                svg,
                r#"<line{class} x1="{SPARK_X_FIRST}" y1="{SPARK_Y_TOP}" x2="{SPARK_X_LAST}" y2="{SPARK_Y_BOTTOM}" stroke="{colour}" stroke-width="{width}" stroke-linecap="round" vector-effect="non-scaling-stroke"/>"#
            );
        }
    }
    svg.push_str("</svg>");
    svg
}

/// The disc's sparkline mount. `spark_id` is an RSX attribute, so it reaches
/// the DOM through `setAttribute`, which never parses it as markup.
#[component]
pub fn SignalSparkIcon(paint: SparkPaint, spark_id: String) -> Element {
    let markup = spark_svg_markup(&paint);
    rsx! {
        span {
            class: "signal-spark",
            "data-signal-spark": "{spark_id}",
            dangerous_inner_html: "{markup}",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::signal_quality::{flat_spark_segment, spark_y, SignalLevel};

    /// The narrowest disc less its border, since the svg fills the content box.
    /// The worst case, so the widest cap. `the_cap_budget_*` pins it to the CSS.
    const SPARK_MIN_RENDER_PX: f64 = 18.0;

    const SHIPPED_CSS: &str = include_str!("../../../static/style.css");

    fn trend_half_units() -> f64 {
        half_units(SPARK_TREND_WIDTH)
    }

    fn half_units(css_px: f64) -> f64 {
        (css_px / 2.0) * (SPARK_VIEW_W / SPARK_MIN_RENDER_PX)
    }

    /// Whatever reaches furthest owns the clipping bounds, slash keyline too.
    fn widest_stroke_half_units() -> f64 {
        half_units(SPARK_HALO_WIDTH.max(SPARK_SLASH_HALO_WIDTH))
    }

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

    fn css_rule<'a>(css: &'a str, needle: &str) -> (&'a str, &'a str) {
        let at = css
            .find(needle)
            .unwrap_or_else(|| panic!("style.css no longer declares `{needle}`"));
        let rule = &css[at..];
        let open = rule.find('{').expect("a rule body");
        let close = rule.find('}').expect("a closed rule body");
        (&rule[..open], &rule[open + 1..close])
    }

    fn px_value(body: &str, prop: &str) -> f64 {
        let at = body
            .find(prop)
            .unwrap_or_else(|| panic!("no `{prop}` in {body}"));
        body[at + prop.len()..]
            .trim_start()
            .split("px")
            .next()
            .and_then(|n| n.parse::<f64>().ok())
            .unwrap_or_else(|| panic!("no px value for `{prop}` in {body}"))
    }

    fn first_y(points: &str) -> f64 {
        points
            .split(' ')
            .next()
            .and_then(|p| p.split(',').nth(1))
            .and_then(|y| y.parse::<f64>().ok())
            .expect("a parsable point")
    }

    fn paint_with(level: SignalLevel, segments: &[&str]) -> SparkPaint {
        SparkPaint {
            segments: segments.iter().map(|s| (*s).to_string()).collect(),
            level,
            sample_count: segments.len(),
            latency_ms: 40.0,
        }
    }

    #[test]
    fn the_trend_carries_the_level_colour() {
        for level in [
            SignalLevel::Excellent,
            SignalLevel::Good,
            SignalLevel::Fair,
            SignalLevel::Poor,
            SignalLevel::Bad,
            SignalLevel::Lost,
        ] {
            let svg = spark_svg_markup(&paint_with(level, &["2.9,3 22.6,13"]));
            assert!(
                svg.contains(&format!(
                    r#"<polyline points="2.9,3 22.6,13" stroke="{}""#,
                    level.level_color()
                )),
                "the {level:?} trend must be painted in the level colour: {svg}"
            );
        }
    }

    #[test]
    fn the_trend_is_never_a_hairline() {
        let svg = spark_svg_markup(&paint_with(SignalLevel::Good, &["2.9,3 22.6,13"]));
        const {
            assert!(
                SPARK_TREND_WIDTH > 1.5,
                "the trend must stay thicker than the 1.5 it shipped at"
            );
        }
        assert_eq!(
            svg.matches(r#"stroke-width="1""#).count(),
            0,
            "the disc draws no hairline at all now: {svg}"
        );
        assert!(svg.contains(&format!(
            r#"stroke-width="{SPARK_TREND_WIDTH}" stroke-linecap="round""#
        )));
    }

    #[test]
    fn spark_cap_fits_inside_the_view_box() {
        let cap = widest_stroke_half_units();
        assert!(
            SPARK_X_FIRST - cap > 0.0,
            "the OLDEST point's cap is clipped on the left: {} <= 0",
            SPARK_X_FIRST - cap
        );
        assert!(
            SPARK_X_LAST + cap < SPARK_VIEW_W,
            "the NEWEST point's cap is clipped on the right: {} >= {SPARK_VIEW_W}",
            SPARK_X_LAST + cap
        );
        // Y too: a tighter margin, so a width bump flat-tops a peak silently.
        let top = SPARK_Y_TOP - cap;
        let bottom = spark_y(0.0) + cap;
        assert!(
            top > 0.0,
            "a peak at the plot ceiling is flat-topped: {top}"
        );
        assert!(
            bottom < SPARK_VIEW_H,
            "a trough at the floor is flat-bottomed: {bottom} >= {SPARK_VIEW_H}"
        );
        let svg = spark_svg_markup(&paint_with(SignalLevel::Good, &[]));
        assert!(
            svg.contains(&format!(r#"viewBox="0 0 {SPARK_VIEW_W} {SPARK_VIEW_H}""#)),
            "{svg}"
        );
    }

    /// `SPARK_MIN_RENDER_PX` is a DERIVATION from the stylesheet, so read it.
    #[test]
    fn the_cap_budget_tracks_the_smallest_shipped_disc() {
        let css = strip_css_comments(SHIPPED_CSS);
        let (selectors, smallest) = css_rule(&css, "\n.split-peer-tile .audio-indicator,");
        assert!(
            selectors.contains(".split-peer-tile .signal-indicator,"),
            "the 22px thumbnail rule no longer covers the disc: {selectors}"
        );
        let (_, base) = css_rule(&css, "\n.signal-indicator {");
        assert!(
            base.contains("box-sizing: border-box"),
            "a content-box disc does not deduct its border: {base}"
        );
        assert_eq!(
            SPARK_MIN_RENDER_PX,
            px_value(smallest, "width:") - 2.0 * px_value(base, "border:"),
            "the smallest disc moved; SPARK_MIN_RENDER_PX must follow it"
        );
    }

    #[test]
    fn every_disc_svg_rule_is_full_bleed() {
        let css = strip_css_comments(SHIPPED_CSS);
        let rules: Vec<&str> = css
            .match_indices(".signal-indicator svg")
            .map(|(i, _)| css_rule(&css[i..], ".signal-indicator svg").1)
            .collect();
        assert!(!rules.is_empty(), "the disc's svg rule vanished");
        for body in rules {
            assert!(
                body.contains("width: 100%"),
                "a `.signal-indicator svg` rule sizes the graph in px: {body}"
            );
        }
    }

    #[test]
    fn the_plot_is_flush_with_the_view_box() {
        // The TREND halo: it is the trend that has to reach the edges.
        let cap = half_units(SPARK_HALO_WIDTH);
        // Half a percent of the width — under a tenth of a CSS px at 18px.
        let slack = SPARK_VIEW_W * 0.005;
        let left = SPARK_X_FIRST - cap;
        let right = SPARK_VIEW_W - (SPARK_X_LAST + cap);
        assert!(
            left < slack,
            "the oldest cap stops {left} short of the left edge (max {slack})"
        );
        assert!(
            right < slack,
            "the newest cap stops {right} short of the right edge (max {slack})"
        );
    }

    #[test]
    fn position_alone_cannot_separate_the_measured_levels() {
        let clearance = trend_half_units();
        let ys: Vec<f64> = [
            SignalLevel::Excellent,
            SignalLevel::Good,
            SignalLevel::Fair,
            SignalLevel::Poor,
            SignalLevel::Bad,
        ]
        .iter()
        .map(|l| first_y(&flat_spark_segment(*l)))
        .collect();
        let tightest = ys
            .windows(2)
            .map(|w| w[1] - w[0])
            .fold(f64::INFINITY, f64::min);
        assert!(
            tightest < 2.0 * clearance,
            "levels now DO clear each other ({tightest} > {}), so height alone may \
             now separate them — the colour-independent channel is the level word \
             every disc puts in its own `aria-label` and `title`",
            2.0 * clearance
        );
    }

    #[test]
    fn no_area_fill_is_drawn_under_the_trend() {
        let flat = "2.90,10.50 22.60,10.50";
        for level in [
            SignalLevel::Excellent,
            SignalLevel::Good,
            SignalLevel::Fair,
            SignalLevel::Poor,
            SignalLevel::Bad,
            SignalLevel::Unmeasured,
            SignalLevel::Lost,
        ] {
            let svg = spark_svg_markup(&paint_with(level, &[flat]));
            assert!(
                svg.contains(r#"<polyline points="#),
                "{level:?} plotted nothing, so the count below passes vacuously: {svg}"
            );
            assert!(
                !svg.contains("<polygon"),
                "the disc interior is one flat colour, so {level:?} may fill no area: {svg}"
            );
        }
    }

    #[test]
    fn an_orphaned_reading_still_gets_its_own_halo() {
        let svg = spark_svg_markup(&paint_with(
            SignalLevel::Good,
            &["2.90,4.00 12.00,4.00", "22.60,6.00 22.60,6.00"],
        ));
        assert_eq!(svg.matches(r#"<polyline points="#).count(), 2, "{svg}");
        assert_eq!(
            svg.matches(r#"<polyline class="spark-halo""#).count(),
            2,
            "the orphan is where the halo matters most: {svg}"
        );
    }

    /// The halo is the bounded backdrop. Painted OVER, it would hide the trend.
    #[test]
    fn the_trend_rides_on_a_halo_painted_under_it() {
        let svg = spark_svg_markup(&paint_with(SignalLevel::Good, &["2.9,3 22.6,13"]));
        let at = |needle: &str| svg.find(needle).expect(needle);
        let halo = at(r#"<polyline class="spark-halo""#);
        assert!(
            halo < at(&format!(r#"stroke="{}""#, SignalLevel::Good.level_color())),
            "the halo must be painted BEFORE the trend it backs: {svg}"
        );
        assert_eq!(
            svg.matches(r#"<polyline class="spark-halo""#).count(),
            svg.matches(r#"<polyline points="#).count(),
            "one halo per run, or a segment loses its backdrop: {svg}"
        );
        const {
            assert!(
                SPARK_HALO_WIDTH > SPARK_TREND_WIDTH && SPARK_SLASH_HALO_WIDTH > SPARK_SLASH_WIDTH,
                "a halo no wider than the mark it backs shows nothing"
            );
        }
        assert!(
            svg.contains(&format!(
                r#"stroke="{SPARK_HALO}" stroke-width="{SPARK_HALO_WIDTH}""#
            )),
            "{svg}"
        );
    }

    #[test]
    fn no_circle_is_drawn_inside_the_plot() {
        let lit = spark_svg_markup(&paint_with(SignalLevel::Good, &["2.9,3 22.6,13"]));
        let empty = spark_svg_markup(&paint_with(SignalLevel::Unmeasured, &[]));
        assert!(!lit.contains("<circle"), "the head dot is gone: {lit}");
        assert!(!empty.contains("<circle"), "{empty}");
    }

    #[test]
    fn no_grid_is_drawn_behind_the_trend() {
        let svg = spark_svg_markup(&paint_with(SignalLevel::Good, &["2.9,3 22.6,13"]));
        assert!(!svg.contains("spark-grid"), "the grid is gone: {svg}");
        assert_eq!(
            svg.matches("<line").count(),
            0,
            "a measured disc draws no rules at all now: {svg}"
        );
        let lost = spark_svg_markup(&paint_with(SignalLevel::Lost, &["2.9,3 22.6,13"]));
        assert_eq!(
            lost.matches("<line").count(),
            2,
            "only the slash and its keyline are lines: {lost}"
        );
    }

    #[test]
    fn only_a_lost_disc_draws_the_slash() {
        // Bad's colour IS the slash hex; its width (1, 2.5, 4) is unambiguous.
        let slash = r#"stroke-width="2" stroke-linecap="round""#;
        let lost = spark_svg_markup(&paint_with(SignalLevel::Lost, &["2.9,3 22.6,13"]));
        let bad = spark_svg_markup(&paint_with(SignalLevel::Bad, &["2.9,3 22.6,13"]));
        assert!(lost.contains(slash), "{lost}");
        assert!(!bad.contains(slash), "{bad}");
        let geometry = format!(
            r#"x1="{SPARK_X_FIRST}" y1="{SPARK_Y_TOP}" x2="{SPARK_X_LAST}" y2="{SPARK_Y_BOTTOM}""#
        );
        assert!(lost.contains(&geometry), "{lost}");
        let keyline = format!(
            r#"<line class="spark-halo" {geometry} stroke="{SPARK_HALO}" stroke-width="{SPARK_SLASH_HALO_WIDTH}""#
        );
        assert!(lost.contains(&keyline), "{lost}");
        assert!(
            lost.find(&keyline).unwrap() < lost.rfind(SPARK_SLASH).unwrap(),
            "the slash's keyline must be painted UNDER it: {lost}"
        );
        // Every disc has a threshold keyline, so match the slash keyline's width.
        assert!(
            !bad.contains(&format!(r#"stroke-width="{SPARK_SLASH_HALO_WIDTH}""#)),
            "{bad}"
        );
    }
}
