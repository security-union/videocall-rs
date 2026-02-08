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

use yew::prelude::*;

/// Host indicator to show the meeting host/owner
#[function_component(CrownIcon)]
pub fn crown_icon() -> Html {
    html! {
        <span
            class="host-indicator"
            style="color: #888; font-size: 0.85em; margin-left: 4px;"
        >
            {"(Host)"}
        </span>
    }
}
