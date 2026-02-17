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

use dioxus::prelude::*;

#[component]
pub fn YoutubeIcon() -> Element {
    rsx! {
        svg {
            class: "external-icon",
            view_box: "0 0 30 20",
            "focusable": "false",
            style: "pointer-events: none; display: block; width: 100%; height: 100%;",
            svg {
                view_box: "0 0 28.6 19.5",
                preserve_aspect_ratio: "xMidYMid meet",
                xmlns: "http://www.w3.org/2000/svg",
                g {
                    path {
                        d: "M27.9727 3.12324C27.6435 1.89323 26.6768 0.926623 25.4468 0.597366C23.2197 2.24288e-07 14.285 0 14.285 0C14.285 0 5.35042 2.24288e-07 3.12323 0.597366C1.89323 0.926623 0.926623 1.89323 0.597366 3.12324C2.24288e-07 5.35042 0 10 0 10C0 10 2.24288e-07 14.6496 0.597366 16.8768C0.926623 18.1068 1.89323 19.0734 3.12323 19.4026C5.35042 20 14.285 20 14.285 20C14.285 20 23.2197 20 25.4468 19.4026C26.6768 19.0734 27.6435 18.1068 27.9727 16.8768C28.5701 14.6496 28.5701 10 28.5701 10C28.5701 10 28.5677 5.35042 27.9727 3.12324Z",
                        fill: "#FF0000"
                    }
                    path {
                        d: "M11.4253 14.2854L18.8477 10.0004L11.4253 5.71533V14.2854Z",
                        fill: "white"
                    }
                }
            }
        }
    }
}
