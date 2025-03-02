use leptos::*;

use crate::pages::Home::perform_markdown_code_to_html;

#[component]
pub fn CodeExample(
    children: Children,
    code: String,
    shadow: bool,
    border: bool,
    background: String,
) -> impl IntoView {
    let code_resource = create_resource(
        || false,
        move |_| perform_markdown_code_to_html(code.clone()),
    );

    view! {
        <CodeExampleLayout
            shadow
            border
            background
            code=CodeExampleMode::Html(code_resource)
            children=children
        />
    }
}

#[derive(Clone)]
pub enum CodeExampleMode {
    Html(Resource<bool, Result<String, ServerFnError>>),
    View(View),
}

#[component]
pub fn CodeExampleLayout(
    code: CodeExampleMode,
    shadow: bool,
    border: bool,
    background: String,
    children: Children,
) -> impl IntoView {
    let shadow_class = if shadow {
        "shadow-[10px_10px_0px_#190E3825]"
    } else {
        ""
    };

    let border_class = if border { "border" } else { "" };
    let code_children_class = "w-full lg:max-w-md max-w-full p-3 md:p-6 bg-[#0b081a] text-[14px] lg:text-[16px] text-white overflow-x-scroll min-h-[300px]";

    view! {
        <div class=format!(
            "flex flex-col lg:flex-row w-full max-w-4xl border-black border-opacity-30 bg-white rounded-md overflow-hidden mx-auto {} {}",
            shadow_class, border_class
        )>
            {match code {
                CodeExampleMode::Html(code_resource) => {
                    view! {
                        <Suspense fallback=move || {
                            view! { <div class=code_children_class>"fallback"</div> }
                        }>
                            {move || {
                                code_resource
                                    .and_then(|code| {
                                        view! { <div class=code_children_class inner_html=code></div> }
                                    })
                            }}
                        </Suspense>
                    }
                        .into_view()
                }
                CodeExampleMode::View(child) => {
                    view! { <div class=code_children_class>{child}</div> }
                        .into_view()
                }
            }}
            <div class="w-full flex flex-col lg:max-w-md max-w-full border-black dark:border-eggshell border-opacity-30 items-center min-h-[300px]">
                <div class="w-full bg-white dark:bg-black flex h-10 lg:rounded-tr-lg border-b border-black dark:border-eggshell border-opacity-30 gap-4 justify-between items-center px-4 pointer-events-none border-t lg:border-t-0">
                    <div class="w-full rounded-md bg-[#dbdbdb] items-center text-sm text-black text-opacity-80 h-5 px-2 pointer-events-none">
                        "example.com"
                    </div>
                    <div class="flex gap-3">
                        <span class="w-3 h-3 rounded-full bg-beige"></span>
                        <span class="w-3 h-3 rounded-full bg-pink"></span>
                        <span class="w-3 h-3 rounded-full bg-light_blue"></span>
                    </div>
                </div>
                <div class=format!("w-full h-full min-h-[250px] {}", background)>{children()}</div>
            </div>
        </div>
    }
}
