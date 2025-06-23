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

#[derive(Clone)]
pub struct IconProps {
    pub path: String,
    pub size: String,
}

#[component]
pub fn CTAButton(
    title: String,
    icon: IconProps,
    #[prop(default = false)] animated: bool,
    #[prop(default = String::new())] class: String,
    #[prop(default = None)] href: Option<String>,
    #[prop(default = None)] style: Option<String>,
) -> impl IntoView {
    let base_class = "cta-button".to_string();
    let combined_class = format!("{} {}", base_class, class);

    let icon_size = icon.size.to_string();
    let icon_path = icon.path.to_string();

    let button_content = move || {
        view! {
            {/* Glow effect div for hover */}
            <div class="cta-glow" style={style.clone()}></div>

            {/* Button content */}
            <div class="flex items-center justify-center">
                <svg
                    class=move || {
                        let mut classes = vec![&icon_size, "mr-6"];
                        if animated {
                            classes.push("transition-transform duration-300");
                        }
                        classes.join(" ")
                    }
                    xmlns="http://www.w3.org/2000/svg"
                    fill="none"
                    viewBox="0 0 24 24"
                    stroke-width="1.5"
                    stroke="currentColor"
                >
                    <path stroke-linecap="round" stroke-linejoin="round" d=&icon_path/>
                </svg>
                <span>{title.clone()}</span>
            </div>
        }
    };

    let combined_class = combined_class.clone();
    let content = button_content();

    view! {
        {move || match &href {
            Some(href) => view! {
                <a href=href class=&combined_class>
                    {content.clone()}
                </a>
            }.into_view(),
            None => view! {
                <button class=&combined_class>
                    {content.clone()}
                </button>
            }.into_view()
        }}
    }
}
