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
        <section id="pricing" aria-labelledby="pricing-title" class="px-6 md:px-10 py-24 md:py-32">
            <div class="max-w-content mx-auto">
                <RevealOnView class="">
                    <p class="section-index" aria-hidden="true">"07 — Deployment"</p>
                    <h2 id="pricing-title" class="text-h2 text-fg mt-4">"Run it yourself, or have us run it"</h2>
                    <p class="text-body-lg text-fg-2 mt-4 max-w-2xl">"Self-host the entire stack, or let us operate it for you."</p>
                </RevealOnView>

                <RevealOnView class="mt-12">
                <div class="grid md:grid-cols-2 gap-6 max-w-4xl">
                    <PricingCard
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
                        delay=0
                    />

                    <PricingCard
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
                        delay=90
                    />
                </div>
                </RevealOnView>
            </div>
        </section>
    }
}

#[component]
fn PricingCard(
    #[prop(into)] title: String,
    #[prop(into)] price: String,
    #[prop(into)] description: String,
    features: Vec<String>,
    #[prop(into)] button_text: String,
    #[prop(into)] button_href: String,
    #[prop(default = false)] highlighted: bool,
    /// Stagger for the reveal-in cascade + the recommended rule draw-in (ms).
    delay: i32,
) -> impl IntoView {
    // The one licensed accent here: a 2px signal top border on the recommended
    // tier (drawn in on reveal), plus a mono RECOMMENDED tag. Everything else
    // stays monochrome. `card-lift` adds the transform-only hover lift; the
    // panel is a `reveal-item` so it joins the staggered reveal cascade.
    let card_class = if highlighted {
        "panel card-lift reveal-item relative flex flex-col order-first md:order-none"
    } else {
        "panel card-lift reveal-item relative flex flex-col"
    };
    let button_class = if highlighted { "btn-solid" } else { "btn-line" };

    view! {
        <div class=card_class style=format!("transition-delay:{delay}ms")>
            {highlighted.then(|| view! {
                <span
                    class="draw-x absolute top-0 left-0 right-0 h-[2px] bg-signal"
                    style=format!("transition-delay:{}ms", delay + 200)
                    aria-hidden="true"
                ></span>
                <span class="eyebrow text-signal">"Recommended"</span>
            })}

            <h3 class="text-h3 text-fg mt-2">{title}</h3>
            <div class="text-4xl font-semibold text-fg mt-3">{price}</div>
            <p class="text-fg-2 mt-3">{description}</p>

            <ul class="space-y-3 mt-8 mb-8">
                {features.into_iter().map(|feature| view! {
                    <li class="flex gap-3 text-fg-2">
                        <span class="font-mono text-fg-3" aria-hidden="true">"+"</span>
                        <span class="text-sm">{feature}</span>
                    </li>
                }).collect_view()}
            </ul>

            <a href=button_href class=format!("{} px-5 py-2.5 w-full mt-auto", button_class)>
                {button_text}
            </a>
        </div>
    }
}
