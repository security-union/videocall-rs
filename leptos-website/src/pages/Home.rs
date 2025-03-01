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
                    class="relative group font-bold text-white text-xl px-14 py-6 rounded-xl overflow-hidden border-2 border-white/20 transition-all duration-300 hover:scale-[1.03] active:scale-[0.97] shadow-[0_0_20px_rgba(var(--color-primary-rgb),0.6)] hover:shadow-[0_0_30px_rgba(var(--color-primary-rgb),0.9)]"
                >
                    {/* Background with animated gradient */}
                    <div class="absolute inset-0 bg-gradient-to-r from-primary via-secondary to-primary bg-[length:200%_100%] animate-gradient-x"></div>
                    
                    {/* Pulsing glow effect */}
                    <div class="absolute inset-0 opacity-40 group-hover:opacity-70 blur-xl bg-gradient-to-r from-primary via-white to-secondary transition-opacity duration-300 animate-pulse"></div>
                    
                    {/* Sparkle effects */}
                    <div class="absolute -top-2 -right-2 w-4 h-4 animate-sparkle delay-100">
                        <svg class="w-full h-full text-white" viewBox="0 0 24 24" fill="currentColor">
                            <path d="M12 0L14 9L23 12L14 15L12 24L10 15L1 12L10 9L12 0Z"/>
                        </svg>
                    </div>
                    <div class="absolute -bottom-2 -left-2 w-4 h-4 animate-sparkle delay-200">
                        <svg class="w-full h-full text-white" viewBox="0 0 24 24" fill="currentColor">
                            <path d="M12 0L14 9L23 12L14 15L12 24L10 15L1 12L10 9L12 0Z"/>
                        </svg>
                    </div>
                    
                    {/* Shine effect on hover */}
                    <div class="absolute top-0 -inset-full h-full w-1/2 z-5 block transform -skew-x-12 bg-gradient-to-r from-transparent to-white opacity-30 group-hover:animate-shine"></div>
                    
                    {/* Button text with shadow for better visibility */}
                    <span class="relative z-10 flex items-center drop-shadow-lg">
                        <svg class="w-7 h-7 mr-3 transform group-hover:scale-125 transition-transform" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M15 10l4.553-2.276A1 1 0 0121 8.618v6.764a1 1 0 01-1.447.894L15 14M5 18h8a2 2 0 002-2V8a2 2 0 00-2-2H5a2 2 0 00-2 2v8a2 2 0 002 2z"/>
                        </svg>
                        {"Create a meeting"}
                        <svg class="w-6 h-6 ml-3 transform group-hover:translate-x-2 transition-transform" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M17 8l4 4m0 0l-4 4m4-4H3"/>
                        </svg>
                    </span>
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
