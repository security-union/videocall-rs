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

#[island]
pub fn InteractiveCodeExample(children: Children) -> impl IntoView {
    provide_context(RwSignal::new(OnStep::Idle));

    view! {
        <div class="w-full min-h-[500px]">
            {children()}
        </div>
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum OnStep {
    Idle,
    Callback,
    Setter,
    Getter,
}

impl OnStep {
    fn next(&mut self) {
        match self {
            Self::Idle => *self = Self::Callback,
            Self::Callback => *self = Self::Setter,
            Self::Setter => *self = Self::Getter,
            Self::Getter => *self = Self::Idle,
        }
    }

    fn prev(&mut self) {
        match self {
            Self::Idle => *self = Self::Getter,
            Self::Callback => *self = Self::Idle,
            Self::Setter => *self = Self::Callback,
            Self::Getter => *self = Self::Setter,
        }
    }
}

#[island]
pub fn CodeView() -> impl IntoView {
    let phase = expect_context::<RwSignal<OnStep>>().read_only();
    let callback_class = move || {
        if phase() == OnStep::Callback {
            "border-2 border-red rounded-sm"
        } else {
            "border-2 border-transparent"
        }
    };
    let setter_class = move || {
        if phase() == OnStep::Setter {
            "border-2 border-red rounded-sm"
        } else {
            "border-2 border-transparent"
        }
    };
    let getter_class = move || {
        if phase() == OnStep::Getter {
            "border-2 border-red rounded-sm"
        } else {
            "border-2 border-transparent"
        }
    };

    view! {
        <pre class="code-block-inner" data-lang="tsx">
            "#"
            <i class="hh8">"["</i>
            <i class="hh15">"component"</i>
            <i class="hh8">"]"</i>
            "\n"
            <i class="hh15">"pub"</i>
            " "
            <i class="hh15">"fn"</i>
            " "
            <i class="hh13">"Button"</i>
            <i class="hh8">"()"</i>
            " "
            <i class="hh5">"-"</i>
            <i class="hh5">">"</i>
            " "
            <i class="hh15">"impl"</i>
            " "
            <i class="hh13">"IntoView"</i>
            " "
            <i class="hh8">"{"</i>
            "\n  "
            <i class="hh6">"let"</i>
            " "
            <i class="hh8">"("</i>
            <span class=getter_class>
                <i class="hh17">"count"</i>
            </span>
            <i class="hh9">","</i>
            " "
            <span class=setter_class>
                <i class="hh17">"set_count"</i>
            </span>
            <i class="hh8">")"</i>
            " "
            <i class="hh5">"="</i>
            " "
            <i class="hh6">"create_signal"</i>
            <i class="hh8">"("</i>
            "0"
            <i class="hh8">")"</i>
            <i class="hh9">";"</i>
            "\n  "
            <i class="hh15">"view"</i>
            <i class="hh5">"!"</i>
            " "
            <i class="hh8">"{"</i>
            " \n    "
            <i class="hh5">"<"</i>
            <i class="hh12">"button"</i>
            " "
            <i class="hh15">"on"</i>
            ":"
            <span class=callback_class>
                <i class="hh15">"click"</i>
            </span>
            <i class="hh5">"="</i>
            <i class="hh15">"move"</i>
            " "
            <i class="hh5">"|"</i>
            <i class="hh15">"_"</i>
            <i class="hh5">"|"</i>
            " "
            <i class="hh8">"{"</i>
            " \n        "
            <i class="hh17">"set_count"</i>
            <i class="hh9">"."</i>
            <span class=setter_class>
                <i class="hh3">"update"</i>
            </span>
            <i class="hh8">"("</i>
            <i class="hh5">"|"</i>
            <i class="hh15">"n"</i>
            <i class="hh5">"|"</i>
            " "
            <i class="hh5">"*"</i>
            <i class="hh15">"n"</i>
            " "
            <i class="hh5">"+="</i>
            " "
            "1"
            <i class="hh8">")"</i>
            <i class="hh9">";"</i>
            "\n      "
            <i class="hh8">"}"</i>
            "\n    "
            <i class="hh5">">"</i>
            "\n      "
            "\"Click me: \""
            "\n      "
            <i class="hh8">"{"</i>
            <span class=getter_class>
                <i class="hh17">"count"</i>
            </span>
            <i class="hh8">"}"</i>
            "\n    "
            <i class="hh5">"<"</i>
            <i class="hh5">"/"</i>
            <i class="hh12">"button"</i>
            <i class="hh5">">"</i>
            "\n  "
            <i class="hh8">"}"</i>
            "\n"
            <i class="hh8">"}"</i>
        </pre>
    }
}

#[island]
pub fn ExampleComponent() -> impl IntoView {
    let (phase, set_phase) = expect_context::<RwSignal<OnStep>>().split();
    let (count, set_count) = create_signal(0);
    let (interactive, set_interactive) = create_signal(false);

    view! {
        <div class="px-2 py-6 h-full w-full flex flex-col justify-center items-center ">
            <div class="flex justify-around w-full mb-8">
                <button
                    class=move || {
                        if !interactive() {
                            "border-b-2 dark:text-white dark:border-white"
                        } else {
                            "dark:text-white dark:border-white"
                        }
                    }
                    on:click=move |_| set_interactive(false)
                >
                    "Counter Button"
                </button>
                <button
                    class=move || {
                        if interactive() {
                            "border-b-2 dark:text-white dark:border-white"
                        } else {
                            "dark:text-white dark:border-white"
                        }
                    }
                    on:click=move |_| set_interactive(true)
                >
                    "How does it work?"
                </button>
            </div>
            <button
                class="text-lg py-2 px-4 text-purple dark:text-eggshell rounded-md \
                border border-purple dark:border-eggshell disabled:opacity-50"
                disabled=move || interactive() && phase() != OnStep::Idle
                on:click=move |_| {
                    set_count.update(|n| *n += 1);
                    if interactive() {
                        set_phase.update(OnStep::next);
                    }
                }
            >
                "Click me: "
                {count}
            </button>
            <Show
                when=move || interactive() && phase() != OnStep::Idle
                fallback=|| {
                    view! { <div class="h-8"></div> }
                }
            >
                <div class="h-8 flex justify-around w-full dark:text-white">
                    <button on:click=move |_| set_phase.update(OnStep::prev)>
                        "〈 Previous Step"
                    </button>
                    <button on:click=move |_| set_phase.update(OnStep::next)>
                        "Next Step 〉"
                    </button>
                </div>
            </Show>
            {move || {
                interactive()
                    .then(|| {
                        view! {
                            <ol class="text-sm w-3/4 mt-8 dark:text-white list-decimal">
                                <li>
                                    "The component function runs once, creating the DOM elements and setting up the reactive system."
                                </li>
                                <li>
                                    "Clicking the button fires the "
                                    <code class=move || { if phase() == OnStep::Callback { "border-2 border-red rounded-sm" } else { "border-2 border-transparent" } }>
                                        "on:click"
                                    </code> " handler. "
                                </li>
                                <li>
                                    "The click handler calls "
                                    <code class=move || { if phase() == OnStep::Setter { "border-2 border-red rounded-sm" } else { "border-2 border-transparent" } }>
                                        "set_count.update"
                                    </code> " to increment the count."
                                </li>
                                <li>
                                    "The change to the "
                                    <code class=move || { if phase() == OnStep::Getter { "border-2 border-red rounded-sm" } else { "border-2 border-transparent" } }>
                                        "count"
                                    </code>
                                    " signal makes a targeted update to the DOM, changing the value of a single text node without re-rendering anything else."
                                </li>
                            </ol>
                        }
                    })
            }}
        </div>
    }
}
