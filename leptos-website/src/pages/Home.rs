use crate::components::HeroHeader::*;
use crate::components::Page::*;
use leptos::*;
use leptos_meta::*;

#[server(PerformMarkdownCodeToHtml, "/api", "GetJSON")]
pub async fn perform_markdown_code_to_html(markdown: String) -> Result<String, ServerFnError> {
    use cached::proc_macro::cached;

    #[cached]
    fn process_markdown(markdown: String) -> Result<String, ServerFnError> {
        use femark::{process_markdown_to_html, HTMLOutput};

        match process_markdown_to_html(markdown) {
            Ok(HTMLOutput { content, toc: _ }) => Ok(content),
            Err(e) => Err(ServerFnError::ServerError(e.to_string())),
        }
    }

    process_markdown(markdown)
}

#[component]
pub fn Home() -> impl IntoView {
    view! {
        <Title text="Home"/>
        <Page>
            <HeroHeader/>
            <div class="w-full flex justify-center pb-16">
                <a
                    href="https://app.videocall.rs"
                    class="font-semibold text-eggshell text-2xl px-8 py-5  bg-gradient-to-r from-dark_blue to-purple rounded-md shadow-[3px_3px_0px_#0d0b29] hover:saturate-200 transition-all"
                >
                    "Create a meeting"
                </a>
            </div>
        </Page>
    }
}
