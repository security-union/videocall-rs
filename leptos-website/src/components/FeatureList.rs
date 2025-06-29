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

#[component]
pub fn FeatureListItem(text: String) -> impl IntoView {
    view! {
        <div class="w-14 h-14 table-row">
            <svg
                xmlns="http://www.w3.org/2000/svg"
                fill="none"
                stroke="currentColor"
                stroke-width="1"
                class="w-10 h-10 stroke-purple dark:stroke-eggshell table-cell"
                viewBox="0 0 24 24"
            >
                <path
                    stroke-linecap="round"
                    stroke-linejoin="round"
                    d="M9 12.75 11.25 15 15 9.75M21 12a9 9 0 1 1-18 0 9 9 0 0 1 18 0z"
                ></path>
            </svg>
            <p class="pl-2 pb-2 text-purple dark:text-eggshell table-cell align-top">
                {text}
            </p>
        </div>
    }
}

#[component]
pub fn FeatureList(items: Vec<String>) -> impl IntoView {
    let feature_list_items: Vec<_> = items
        .iter()
        .map(|item_text| {
            FeatureListItem(FeatureListItemProps {
                text: item_text.clone(),
            })
        })
        .collect();

    view! { <div class="table">{feature_list_items}</div> }
}
