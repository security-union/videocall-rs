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
pub fn SecondaryButton(
    #[prop(into)] title: String,
    #[prop(default = String::new(), into)] class: String,
    #[prop(default = None)] href: Option<String>,
    #[prop(default = None)] style: Option<String>,
) -> impl IntoView {
    let base_class = "secondary-button";
    let combined_class = format!("{} {}", base_class, class);

    view! {
        {move || match &href {
            Some(href) => view! {
                <a href=href class=&combined_class style=style.clone()>
                    <span>{title.clone()}</span>
                </a>
            }.into_view(),
            None => view! {
                <button class=&combined_class style=style.clone()>
                    <span>{title.clone()}</span>
                </button>
            }.into_view()
        }}
    }
}
