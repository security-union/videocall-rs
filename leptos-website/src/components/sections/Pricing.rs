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

use crate::components::Reveal::RevealOnView;
use leptos::prelude::*;

#[component]
pub fn PricingSection() -> impl IntoView {
    view! {
        <section id="pricing" aria-labelledby="pricing-title" class="px-6 md:px-10 py-16 md:py-24">
            <div class="max-w-content mx-auto">
                <RevealOnView class="">
                    <p class="section-index" aria-hidden="true">"08 — Deployment"</p>
                    <h2 id="pricing-title" class="text-h2 text-fg mt-4">"Run it yourself, or have us run it"</h2>
                    <p class="text-body-lg text-fg-2 mt-4 max-w-2xl">"Self-host the entire stack, or let us operate it for you."</p>
                </RevealOnView>

                // Two editorial columns split by a single hairline — no cards,
                // no lift. The recommended tier is marked with a monochrome tag
                // and a weight cue, not an accent border (accent discipline).
                <div class="grid md:grid-cols-2 max-w-4xl mt-10 md:mt-12 divide-y md:divide-y-0 md:divide-x divide-line">
                    <PricingColumn
                        title="Self-Hosted"
                        price="Free"
                        description="Deploy and manage your own instance with full control."
                        features=vec![
                            "Complete source code".to_string(),
                            "Kubernetes Helm charts".to_string(),
                            "Community support".to_string(),
                            "You manage updates and security".to_string(),
                        ]
                        button_text="Get the Helm chart"
                        button_href="https://github.com/security-union/videocall-rs/tree/main/helm"
                        highlighted=false
                    />

                    <PricingColumn
                        title="Enterprise"
                        price="Custom"
                        description="Tailored deployments for organizations with specific requirements."
                        features=vec![
                            "Custom SLA terms".to_string(),
                            "Dedicated support team".to_string(),
                            "Custom feature development".to_string(),
                            "On-premise deployment options".to_string(),
                        ]
                        button_text="Contact sales"
                        button_href="mailto:support@securityunion.dev"
                        highlighted=true
                    />
                </div>
            </div>
        </section>
    }
}

#[component]
fn PricingColumn(
    #[prop(into)] title: String,
    #[prop(into)] price: String,
    #[prop(into)] description: String,
    features: Vec<String>,
    #[prop(into)] button_text: String,
    #[prop(into)] button_href: String,
    #[prop(default = false)] highlighted: bool,
) -> impl IntoView {
    // Symmetric interior gutters so the dividing hairline has air on both sides.
    let button_class = if highlighted { "btn-solid" } else { "btn-line" };

    view! {
        <div class="flex flex-col py-8 md:py-2 md:px-10 md:first:pl-0 md:last:pr-0">
            {highlighted
                .then(|| {
                    view! { <span class="eyebrow text-fg">"Recommended"</span> }
                })}

            <h3 class="text-h3 text-fg mt-2">{title}</h3>
            <div class="text-4xl font-semibold text-fg mt-3">{price}</div>
            <p class="text-fg-2 mt-3">{description}</p>

            <ul class="space-y-3 mt-8 mb-8">
                {features
                    .into_iter()
                    .map(|feature| {
                        view! {
                            <li class="flex gap-3 text-fg-2">
                                <span class="font-mono text-fg-3" aria-hidden="true">"+"</span>
                                <span class="text-sm">{feature}</span>
                            </li>
                        }
                    })
                    .collect_view()}
            </ul>

            <a href=button_href class=format!("{} px-5 py-2.5 w-full mt-auto", button_class)>
                {button_text}
            </a>
        </div>
    }
}
