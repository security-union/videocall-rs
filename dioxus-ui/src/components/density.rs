// SPDX-License-Identifier: MIT OR Apache-2.0

/// Density modes control how many peer tiles the grid displays by setting a
/// minimum tile width.  The layout algorithm fits as many tiles as possible
/// while keeping each tile at least `min_tile_width` pixels wide.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DensityMode {
    Auto,
    Standard,
    Dense,
    Maximum,
}

impl DensityMode {
    /// Minimum tile width (px) for this mode, adjusted for viewport.
    /// This is the ONLY knob that differentiates modes — no participant caps.
    /// The system fits as many tiles as possible while keeping each tile at
    /// least this wide.  Values are tuned so that **all four modes produce
    /// visibly different tile counts** for a typical 20-participant call on
    /// both desktop (~1366 px) and mobile (~375 px).
    pub fn min_tile_width(self, viewport_w: f64) -> f64 {
        if viewport_w < 568.0 {
            // Mobile: 1-col vs 2-col is the main differentiator.
            match self {
                DensityMode::Standard => 250.0, // 1 col, ~4 tiles
                DensityMode::Auto => 170.0,     // 1 col, ~6 tiles
                DensityMode::Dense => 140.0,    // 2 cols, ~16 tiles
                DensityMode::Maximum => 90.0,   // 3 cols, ~20 tiles
            }
        } else {
            // Desktop: 3-col / 4-col / 5-col+ transitions.
            match self {
                DensityMode::Standard => 340.0, // 3 cols, ~9 tiles
                DensityMode::Auto => 280.0,     // 4 cols, ~12 tiles
                DensityMode::Dense => 260.0,    // 4 cols, ~16 tiles
                DensityMode::Maximum => 120.0,  // 5+ cols, all tiles
            }
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            DensityMode::Auto => "Auto",
            DensityMode::Standard => "Standard",
            DensityMode::Dense => "Dense",
            DensityMode::Maximum => "Maximum",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            DensityMode::Auto => "Optimal",
            DensityMode::Standard => "Fewer, larger",
            DensityMode::Dense => "More, smaller",
            DensityMode::Maximum => "As many as fit",
        }
    }
}

pub const DENSITY_MODES: [DensityMode; 4] = [
    DensityMode::Auto,
    DensityMode::Standard,
    DensityMode::Dense,
    DensityMode::Maximum,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_returns_expected_strings() {
        assert_eq!(DensityMode::Auto.label(), "Auto");
        assert_eq!(DensityMode::Standard.label(), "Standard");
        assert_eq!(DensityMode::Dense.label(), "Dense");
        assert_eq!(DensityMode::Maximum.label(), "Maximum");
    }

    #[test]
    fn description_returns_expected_strings() {
        assert_eq!(DensityMode::Auto.description(), "Optimal");
        assert_eq!(DensityMode::Standard.description(), "Fewer, larger");
        assert_eq!(DensityMode::Dense.description(), "More, smaller");
        assert_eq!(DensityMode::Maximum.description(), "As many as fit");
    }

    #[test]
    fn density_modes_array_has_all_variants() {
        assert_eq!(DENSITY_MODES.len(), 4);
        assert!(DENSITY_MODES.contains(&DensityMode::Auto));
        assert!(DENSITY_MODES.contains(&DensityMode::Standard));
        assert!(DENSITY_MODES.contains(&DensityMode::Dense));
        assert!(DENSITY_MODES.contains(&DensityMode::Maximum));
    }

    #[test]
    fn min_tile_width_desktop_ordering() {
        let vw = 1366.0;
        let standard = DensityMode::Standard.min_tile_width(vw);
        let auto = DensityMode::Auto.min_tile_width(vw);
        let dense = DensityMode::Dense.min_tile_width(vw);
        let maximum = DensityMode::Maximum.min_tile_width(vw);
        assert!(
            standard > auto && auto > dense && dense > maximum,
            "Expected Standard ({standard}) > Auto ({auto}) > Dense ({dense}) > Maximum ({maximum})"
        );
    }

    #[test]
    fn min_tile_width_mobile_ordering() {
        let vw = 375.0;
        let standard = DensityMode::Standard.min_tile_width(vw);
        let auto = DensityMode::Auto.min_tile_width(vw);
        let dense = DensityMode::Dense.min_tile_width(vw);
        let maximum = DensityMode::Maximum.min_tile_width(vw);
        assert!(
            standard > auto && auto > dense && dense > maximum,
            "Expected Standard ({standard}) > Auto ({auto}) > Dense ({dense}) > Maximum ({maximum})"
        );
    }

    #[test]
    fn min_tile_width_positive() {
        for &mode in &DENSITY_MODES {
            let desktop = mode.min_tile_width(1366.0);
            let mobile = mode.min_tile_width(375.0);
            assert!(
                desktop > 0.0,
                "{:?} desktop min_tile_width should be positive, got {desktop}",
                mode
            );
            assert!(
                mobile > 0.0,
                "{:?} mobile min_tile_width should be positive, got {mobile}",
                mode
            );
        }
    }
}
