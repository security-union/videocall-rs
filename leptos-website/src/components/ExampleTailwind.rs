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

use leptos::*;

pub enum AlertType {
    Info,
    Success,
    Warning,
    Error,
}

pub const EXAMPLE_TAILWIND_CODE: &str = r#"```tsx
#[component]
pub fn Collections() -> impl IntoView {
    view! { 
        <h2 class="text-2xl font-bold text-black">
            "Collections"
        </h2>
        <div class="my-4">
            <div class="cursor-pointer">
                <div class="max-w-sm h-40 overflow-hidden rounded-lg">
                    <img loading="lazy" src="desk.jpg"
                         class="h-full w-full object-cover object-center"/>
                </div>
                <h3 class="mt-6 text-sm text-[#3f3f3f] dark:text-[#e0e0e0]">
                    "Desk and Office"
                </h3>
                <p class="font-semibold text-black">
                    "Work from home accessories"
                </p>
            </div>
        </div>
    }
}
```"#;

#[component]
pub fn ExampleTailwind() -> impl IntoView {
    view! {
        <div class="p-4 sm:p-8">
            <h2 class="text-2xl font-bold text-black dark:text-eggshell">"Collections"</h2>
            <div class="my-4">
                <div class="cursor-not-allowed">
                    <div class="max-w-sm h-40 overflow-hidden rounded-lg">
                        <img
                            loading="lazy"
                            src="https://tailwindui.com/img/ecommerce-images/home-page-02-edition-01.jpg"
                            class="h-full w-full object-cover object-center"
                            alt=""
                        />
                    </div>
                    <h3 class="mt-6 text-sm text-[#3f3f3f] dark:text-[#e0e0e0]">
                        "Desk and Office"
                    </h3>
                    <p class="font-semibold text-black dark:text-eggshell">
                        "Work from home accessories"
                    </p>
                </div>
            </div>
        </div>
    }
}
