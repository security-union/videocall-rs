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
                    class="relative group font-semibold text-foreground text-xl px-12 py-5 bg-gradient-to-r from-primary to-secondary rounded-lg shadow-lg shadow-primary/20 hover:shadow-xl hover:shadow-primary/30 transition-all duration-300 overflow-hidden"
                >
                    {/* Shine effect on hover */}
                    <span class="absolute inset-0 w-0 bg-white/20 skew-x-[-20deg] group-hover:w-full group-hover:transition-all group-hover:duration-700 transform -translate-x-full group-hover:translate-x-0"></span>
                    
                    {/* Button text */}
                    <span class="relative flex items-center">
                        {"Create a meeting"}
                        <svg class="w-5 h-5 ml-2 transform group-hover:translate-x-1 transition-transform" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M14 5l7 7m0 0l-7 7m7-7H3" />
                        </svg>
                    </span>
                </a>
            </div>

            <SolutionsSection/>
            <DevelopersSection/>
            <CompanySection/>
            <CustomersSection/>
            <PricingSection/>
        </Page>
    }
}
