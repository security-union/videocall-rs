use yew::prelude::*;

#[derive(Properties, PartialEq)]
pub struct NetEqChartProps {
    pub data: Vec<u64>,
    pub chart_type: ChartType,
    pub width: u32,
    pub height: u32,
}

#[derive(PartialEq, Clone)]
pub enum ChartType {
    Buffer,
    Jitter,
}

impl ChartType {
    fn stroke_color(&self) -> &'static str {
        match self {
            ChartType::Buffer => "#8ef",
            ChartType::Jitter => "#ff8",
        }
    }

    fn label(&self) -> &'static str {
        match self {
            ChartType::Buffer => "Buffer (ms)",
            ChartType::Jitter => "Jitter (ms)",
        }
    }
}

#[function_component(NetEqChart)]
pub fn neteq_chart(props: &NetEqChartProps) -> Html {
    let NetEqChartProps {
        data,
        chart_type,
        width,
        height,
    } = props;

    let chart_width = *width as f64;
    let chart_height = *height as f64;
    let margin_left = 25.0;
    let margin_bottom = 15.0;
    let plot_width = chart_width - margin_left - 10.0;
    let plot_height = chart_height - margin_bottom - 5.0;

    let max_val = *data.iter().max().unwrap_or(&1);
    let max_val_f64 = max_val as f64;
    let data_len = data.len();

    // Generate polyline points
    let points: String = if data.is_empty() {
        String::new()
    } else {
        data.iter()
            .enumerate()
            .map(|(i, v)| {
                let x = margin_left
                    + (i as f64 / (data_len.saturating_sub(1).max(1) as f64) * plot_width);
                let y = plot_height
                    - (*v as f64 / if max_val_f64 == 0.0 { 1.0 } else { max_val_f64 }
                        * plot_height)
                    + 5.0;
                format!("{:.1},{:.1}", x, y)
            })
            .collect::<Vec<_>>()
            .join(" ")
    };

    let time_span = data_len.saturating_sub(1);

    html! {
        <div class="neteq-chart">
            <div class="chart-title">{ chart_type.label() }</div>
            <svg width={width.to_string()} height={height.to_string()} viewBox={format!("0 0 {} {}", width, height)} preserveAspectRatio="none">
                // Y-axis
                <line x1={margin_left.to_string()} y1="5" x2={margin_left.to_string()} y2={(plot_height + 5.0).to_string()} stroke="#666" stroke-width="1" />
                // X-axis
                <line x1={margin_left.to_string()} y1={(plot_height + 5.0).to_string()} x2={(chart_width - 5.0).to_string()} y2={(plot_height + 5.0).to_string()} stroke="#666" stroke-width="1" />

                // Data line
                if !points.is_empty() {
                    <polyline points={points} fill="none" stroke={chart_type.stroke_color()} stroke-width="2" />
                }

                // Y-axis labels
                <text x="0" y="10" fill="#888" font-size="8">{ max_val }</text>
                <text x="0" y={(plot_height + 5.0).to_string()} fill="#888" font-size="8">{"0"}</text>

                // X-axis labels
                <text x={margin_left.to_string()} y={(chart_height - 1.0).to_string()} fill="#888" font-size="8">{"0s"}</text>
                <text x={(chart_width - 20.0).to_string()} y={(chart_height - 1.0).to_string()} fill="#888" font-size="8">{ format!("{}s", time_span) }</text>
            </svg>
        </div>
    }
}
