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

//! NetEQ chart components for visualizing audio buffer statistics
//!
//! This is a simplified version - full charting can be added later using a WASM-compatible
//! charting library.

use dioxus::prelude::*;

#[derive(Clone, PartialEq)]
pub enum ChartType {
    Buffer,
    Jitter,
}

#[derive(Props, Clone, PartialEq)]
pub struct NetEqChartProps {
    pub data: Vec<u64>,
    pub chart_type: ChartType,
    pub width: u32,
    pub height: u32,
}

#[component]
pub fn NetEqChart(props: NetEqChartProps) -> Element {
    let label = match props.chart_type {
        ChartType::Buffer => "Buffer",
        ChartType::Jitter => "Jitter",
    };

    let (min_val, max_val, avg_val) = if props.data.is_empty() {
        (0, 0, 0)
    } else {
        let min = *props.data.iter().min().unwrap_or(&0);
        let max = *props.data.iter().max().unwrap_or(&0);
        let avg = props.data.iter().sum::<u64>() / props.data.len() as u64;
        (min, max, avg)
    };

    let current_val = props.data.last().copied().unwrap_or(0);

    rsx! {
        div {
            class: "neteq-chart",
            style: "width: {props.width}px; height: {props.height}px; background: #1C1C1E; border-radius: 8px; padding: 8px;",
            div { style: "font-size: 11px; color: #AEAEB2; margin-bottom: 4px;", "{label}" }
            div { style: "font-size: 16px; color: #FFFFFF; font-weight: 600;", "{current_val}ms" }
            div { style: "font-size: 10px; color: #8E8E93; margin-top: 4px;",
                "min: {min_val} | avg: {avg_val} | max: {max_val}"
            }
            // Simple bar visualization
            div {
                style: "margin-top: 8px; height: 4px; background: #2C2C2E; border-radius: 2px; overflow: hidden;",
                div {
                    style: "height: 100%; background: linear-gradient(90deg, #30D158, #FF9F0A); width: {(current_val.min(100) as f32)}%;",
                }
            }
        }
    }
}
