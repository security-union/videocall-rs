// SPDX-License-Identifier: MIT OR Apache-2.0

use dioxus::prelude::*;

#[component]
pub fn DigitalOceanIcon() -> Element {
    rsx! {
        svg {
            width: "100%",
            height: "100%",
            view_box: "0 0 200 65",
            fill: "none",
            xmlns: "http://www.w3.org/2000/svg",
            rect { width: "200", height: "65", rx: "5", fill: "#031B4E" }
            rect {
                width: "200",
                height: "65",
                rx: "5",
                fill: "url(#paint0_linear)",
                style: "mix-blend-mode:overlay",
            }
            // DigitalOcean logo paths
            path {
                d: "M73.8134 33.544C72.3797 32.5873 70.6 32.085 68.5236 32.085H64V45.9816H68.5236C70.6 45.9816 72.3797 45.4794 73.8134 44.4509C74.6044 43.9247 75.2224 43.1593 75.6426 42.2265C76.0628 41.2936 76.2853 40.1934 76.2853 38.9496C76.2853 37.7298 76.0628 36.6295 75.6426 35.6967C75.2224 34.7878 74.6044 34.0703 73.8134 33.544Z",
                fill: "white",
            }
            // Droplet icon
            path {
                fill_rule: "evenodd",
                clip_rule: "evenodd",
                d: "M39.0979 48V41.8036C45.7129 41.8036 50.7985 35.3032 48.2748 28.3845C47.3571 25.8376 45.2923 23.7848 42.7304 22.8724C35.7713 20.3635 29.2327 25.4574 29.2327 31.9959V31.9959H23C23 21.5419 33.1711 13.3688 44.1835 16.7901C49.0014 18.2727 52.8251 22.1121 54.3546 26.902C57.7959 37.8881 49.6131 48 39.0979 48V48Z",
                fill: "white",
            }
            path {
                fill_rule: "evenodd",
                clip_rule: "evenodd",
                d: "M39.0976 41.8417H32.9031V35.6833V35.6833H39.0976V35.6833V41.8417Z",
                fill: "white",
            }
            path {
                fill_rule: "evenodd",
                clip_rule: "evenodd",
                d: "M32.9024 46.5935H28.1227V46.5935V41.8416H32.9024V46.5935V46.5935Z",
                fill: "white",
            }
            path {
                fill_rule: "evenodd",
                clip_rule: "evenodd",
                d: "M28.1251 41.8417H24.1102V41.8417V37.8882V37.8882H28.0868V37.8882V41.8417H28.1251Z",
                fill: "white",
            }
            defs {
                linearGradient {
                    id: "paint0_linear",
                    x1: "106.667",
                    y1: "-23.1356",
                    x2: "58.8573",
                    y2: "72.8536",
                    stop { stop_color: "white" }
                    stop { offset: "1", stop_color: "white", stop_opacity: "0" }
                }
            }
        }
    }
}
