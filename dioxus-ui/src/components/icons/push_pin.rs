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
pub fn PushPinIcon() -> Element {
    rsx! {
        svg {
            class: "w-8",
            xmlns: "http://www.w3.org/2000/svg",
            enable_background: "new 0 0 24 24",
            view_box: "0 0 24 24",
            fill: "#FFFFFF",
            g {
                rect { fill: "none", height: "24", width: "24" }
            }
            g {
                path {
                    d: "M16,9V4l1,0c0.55,0,1-0.45,1-1v0c0-0.55-0.45-1-1-1H7C6.45,2,6,2.45,6,3v0 c0,0.55,0.45,1,1,1l1,0v5c0,1.66-1.34,3-3,3h0v2h5.97v7l1,1l1-1v-7H19v-2h0C17.34,12,16,10.66,16,9z",
                    fill_rule: "evenodd"
                }
            }
        }
    }
}
