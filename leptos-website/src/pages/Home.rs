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
use crate::components::sections::System::SystemSection;

// Removed unused import
use crate::components::HeroHeader::*;
use crate::components::MediaBand::{GlobeBand, RoverBand};
use crate::components::Page::*;
use leptos::prelude::*;
use leptos_meta::Title;

#[server(PerformMarkdownCodeToHtml)]
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
        <Title text="videocall.rs — Full-stack real-time audio and video, in Rust"/>
        <Page>
            <SiteNav/>
            <main id="content">
                // Alternating rhythm: media band -> editorial -> media -> editorial.
                // Hairline `.rule` seams sit ONLY between two contained editorial
                // bands; a full-bleed media band's black edge does its own
                // separating, so no rule is placed adjacent to one.
                <Hero/>
                <GlobeBand/>
                <SystemSection/>
                <div class="rule"></div>
                <SupportedPlatformsSection/>
                <div class="rule"></div>
                <DevelopersSection/>
                <RoverBand/>
                <CompanySection/>
                <div class="rule"></div>
                <CustomersSection/>
                <div class="rule"></div>
                <PricingSection/>
            </main>
        </Page>
    }
}
