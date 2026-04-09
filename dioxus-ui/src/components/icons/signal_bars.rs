// SPDX-License-Identifier: MIT OR Apache-2.0

use dioxus::prelude::*;

/// Cellular-style signal bars icon (5 bars of increasing height).
///
/// - `level`: 0..=5, number of filled bars.
/// - `lost`: when true, all bars are gray and a red diagonal slash is drawn.
#[component]
pub fn SignalBarsIcon(
    #[props(default = 5)] level: u8,
    #[props(default = false)] lost: bool,
) -> Element {
    let level = level.min(5);

    // All filled bars share one color determined by the current level.
    let fill_color = if lost {
        "#555"
    } else {
        match level {
            5 => "#00ff41",
            4 => "#4CAF50",
            3 => "#FFC107",
            2 => "#FF8C00",
            1 => "#FF4444",
            _ => "#555", // level 0: all unfilled
        }
    };
    let unfilled = "#555";

    // Bar geometry: 5 bars, each 3px wide with 1.5px gap.
    // x positions: 2, 6.5, 11, 15.5, 20
    // Heights increase from 6 to 18. Bottom y is always 22.
    let bars: [(f64, f64); 5] = [
        (1.5, 6.0),   // bar 1: shortest
        (6.0, 9.0),   // bar 2
        (10.5, 12.0), // bar 3
        (15.0, 15.0), // bar 4
        (19.5, 18.0), // bar 5: tallest
    ];

    let effective_level = if lost { 0 } else { level };

    rsx! {
        svg {
            xmlns: "http://www.w3.org/2000/svg",
            view_box: "0 0 24 24",
            fill: "none",
            for (i, (x, h)) in bars.iter().enumerate() {
                {
                    let bar_num = (i + 1) as u8;
                    let color = if bar_num <= effective_level { fill_color } else { unfilled };
                    let y = 22.0 - h;
                    rsx! {
                        rect {
                            x: "{x}",
                            y: "{y}",
                            width: "3",
                            height: "{h}",
                            rx: "1",
                            fill: "{color}",
                        }
                    }
                }
            }
            // Red diagonal slash when signal is lost
            if lost || level == 0 {
                line {
                    x1: "0.5",
                    y1: "3",
                    x2: "23.5",
                    y2: "21",
                    stroke: "#FF0000",
                    stroke_width: "2",
                    stroke_linecap: "round",
                }
            }
        }
    }
}
