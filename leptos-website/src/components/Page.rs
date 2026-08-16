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

use crate::components::Footer::*;
// use crate::components::Header::*;
use leptos::prelude::*;
use leptos_meta::Style;

#[component]
pub fn Page(children: Children) -> impl IntoView {
    view! {
        <Style>{include_str!("../global.css")}</Style>
        // overflow-x: clip, NOT hidden: `hidden` makes this div a scroll
        // container, which silently breaks every position:sticky descendant
        // (nav, the system stack). `clip` clips without creating one.
        <div class="min-h-screen text-fg bg-bg [overflow-x:clip]">
            {children()}
            <Footer/>
        </div>
    }
}
