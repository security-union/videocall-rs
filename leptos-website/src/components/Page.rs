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
use leptos::*;
use leptos_meta::Style;

#[component]
pub fn Page(children: Children) -> impl IntoView {
    view! {
        <Style>{include_str!("../global.css")}</Style>
        <div class="min-h-screen text-foreground bg-background overflow-x-hidden">
            <div class="w-full min-h-[70vh]">
                {children()}
            </div>
            <Footer/>
        </div>
    }
}
