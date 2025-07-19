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

use crate::components::sections::Company::CompanySection;
use crate::components::sections::Customers::CustomersSection;
use crate::components::sections::Developers::DevelopersSection;
use crate::components::sections::Pricing::PricingSection;
use crate::components::sections::Solutions::SolutionsSection;
use crate::components::CTAButton::*;
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
            <div class="w-full flex justify-center pb-16 relative z-10" style="margin-top: -2rem;">
                <CTAButton
                    title="Create a meeting".to_string()
                    icon=IconProps {
                        path: "M15 10l4.553-2.276A1 1 0 0121 8.618v6.764a1 1 0 01-1.447.894L15 14M5 18h8a2 2 0 002-2V8a2 2 0 00-2-2H5a2 2 0 00-2 2v8a2 2 0 002 2z".into(),
                        size: "w-16 h-16".into(),
                    }
                    animated=true
                    href=Some("https://app.videocall.rs".to_string())
                    class="text-4xl px-16 py-8".to_string()
                />
            </div>
            <div class="max-w-[1720px] mx-auto relative">
                <SolutionsSection/>
                <DevelopersSection/>
                <CompanySection/>
                <CustomersSection/>
                <PricingSection/>
            </div>
        </Page>
    }
}
