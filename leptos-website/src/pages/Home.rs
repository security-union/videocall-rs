use crate::components::HeroHeader::*;
use crate::components::Page::*;
use crate::components::sections::Solutions::SolutionsSection;
use crate::components::sections::Developers::DevelopersSection;
use crate::components::sections::Company::CompanySection;
use crate::components::sections::Customers::CustomersSection;
use crate::components::sections::Pricing::PricingSection;
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
                    class="group relative px-16 py-8 rounded-2xl bg-gradient-to-r from-primary to-primary hover:from-primary hover:to-secondary text-white font-bold text-4xl shadow-xl hover:shadow-2xl transition-all duration-300 hover:scale-[1.03] active:scale-[0.97] border-4 border-white/20"
                >
                    {/* Top accent line */}
                    <div class="absolute top-0 left-0 w-full h-2 bg-white/30 rounded-t-2xl"></div>
                    
                    {/* Button content */}
                    <div class="flex items-center justify-center">
                        <svg class="w-16 h-16 mr-6 transition-transform duration-300 group-hover:scale-125" fill="none" viewBox="0 0 24 24" stroke="currentColor" stroke-width="2">
                            <path stroke-linecap="round" stroke-linejoin="round" d="M15 10l4.553-2.276A1 1 0 0121 8.618v6.764a1 1 0 01-1.447.894L15 14M5 18h8a2 2 0 002-2V8a2 2 0 00-2-2H5a2 2 0 00-2 2v8a2 2 0 002 2z"/>
                        </svg>
                        <span>{"Create a meeting"}</span>
                    </div>
                </a>
            </div>
            <div class="px-4">
                <SolutionsSection/>
                <DevelopersSection/>
                <CompanySection/>
                <CustomersSection/>
                <PricingSection/>
            </div>
        </Page>
    }
}
