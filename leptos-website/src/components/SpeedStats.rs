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

struct SpeedStat {
    name: String,
    color: String,
    color_transparent: String,
    background: String,
    percentage: u8,
}

#[component]
pub fn PercentageBar(
    color: String,
    percentage: u8,
    background: String,
    color_transparent: String,
    tech_name: String,
) -> impl IntoView {
    view! {
        <div class="text-eggshell flex flex-col lg:items-center lg:flex-row">
            <div class=" mb-2 font-bold lg:hidden">{tech_name}</div>
            <div
                style=format!("border-color: {color_transparent}")
                class="w-full h-10 relative rounded-md border-2"
            >
                <div
                    style=format!("width: {percentage}%; outline-color: {color}")
                    class=format!(
                        "h-full absolute top-0 left-0 outline-2 outline flex items-center justify-end px-2 {background}"
                    )
                >
                    <span class="relative ">{percentage} "%"</span>
                </div>
            </div>
        </div>
    }
}

#[component]
pub fn Label(tech_name: String) -> impl IntoView {
    view! {
        <div class="text-purple dark:text-eggshell">
            <div class="h-10 text-xl flex items-center justify-end">{tech_name}</div>
        </div>
    }
}

#[component]
pub fn SpeedStats(shadow: bool, border: bool) -> impl IntoView {
    let shadow_class = if shadow {
        "shadow-[10px_10px_0px_#190E3825]"
    } else {
        ""
    };

    let border_class = if border { "border" } else { "" };

    let labels = vec![
        SpeedStat {
            name: String::from("VanillaJS"),
            color: String::from("#A5D6A7"),
            color_transparent: String::from("#A5D6A740"),
            background: String::from(""),
            percentage: 100,
        },
        SpeedStat {
            name: String::from("Leptos"),
            color: String::from("#ED3135"),
            color_transparent: String::from("#ED313540"),
            background: String::from("bg-gradient-to-r from-purple to-red"),
            percentage: 92,
        },
        SpeedStat {
            name: String::from("Vue"),
            color: String::from("#F0ADA8"),
            color_transparent: String::from("#F0ADA840"),
            background: String::from(""),
            percentage: 80,
        },
        SpeedStat {
            name: String::from("Svelte"),
            color: String::from("#D2D7B4"),
            color_transparent: String::from("#D2D7B440"),
            background: String::from(""),
            percentage: 73,
        },
        SpeedStat {
            name: String::from("React"),
            color: String::from("#A8DADC"),
            color_transparent: String::from("#A8DADC40"),
            background: String::from(""),
            percentage: 33,
        },
    ];

    view! {
        <div class="2xl:ml-[-100px]">
            <div class="flex max-w-4xl mx-auto">
                <div class="hidden lg:flex flex-col gap-4 py-8 pr-4 pl-0 mt-0.5 font-bold ">
                    {labels
                        .iter()
                        .map(|row| {
                            view! { <Label tech_name=row.name.clone()/> }
                        })
                        .collect::<Vec<_>>()}
                </div>
                <div class=format!(
                    "w-full h-full p-8 gap-4 bg-gradient-to-tr from-purple to-dark_blue rounded-md mx-auto flex flex-col {} {}",
                    shadow_class, border_class
                )>
                    {labels
                        .iter()
                        .map(|row| {
                            view! {
                                <PercentageBar
                                    tech_name=row.name.clone()
                                    color=row.color.clone()
                                    color_transparent=row.color_transparent.clone()
                                    background=row.background.clone()
                                    percentage=row.percentage
                                />
                            }
                        })
                        .collect::<Vec<_>>()}
                    <p class="text-white opacity-50 text-sm">
                        "Source: "
                        <a href="https://krausest.github.io/js-framework-benchmark/2023/table_chrome_113.0.5672.63.html">
                            <code>"js-framework-benchmark"</code>
                            " official results for Chrome 113."
                        </a>
                    </p>
                </div>
            </div>
        </div>
    }
}
