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
use crate::components::sections::SupportedPlatforms::SupportedPlatformsSection;

// Removed unused import
use crate::components::HeroHeader::*;
use crate::components::Page::*;
use leptos::*;
use leptos_meta::*;

#[server(PerformMarkdownCodeToHtml, "/api", "GetJSON")]
pub async fn perform_markdown_code_to_html(markdown: String) -> Result<String, ServerFnError> {
    use femark::{process_markdown_to_html, HTMLOutput};

    match process_markdown_to_html(markdown) {
        Ok(HTMLOutput { content, toc: _ }) => Ok(content),
        Err(e) => Err(ServerFnError::ServerError(e.to_string())),
    }
}

#[component]
pub fn Home() -> impl IntoView {
    view! {
        <Title text="Home"/>
        <Page>
            <HeroHeader/>

            // Apple-style content sections with generous spacing
            <div class="max-w-7xl mx-auto relative space-y-32 py-24 px-4 sm:px-6 lg:px-8">
                <SupportedPlatformsSection/>
                <DevelopersSection/>
                <CompanySection/>
                <CustomersSection/>
                <PricingSection/>
            </div>
        </Page>
    }
}
