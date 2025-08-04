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

use crate::components::CTAButton::{CTAButton, ButtonSize, ButtonVariant};
use leptos::*;

#[component]
pub fn PricingSection() -> impl IntoView {
    view! {
        <section id="pricing" class="relative">
            <div class="text-center mb-20">
                <h2 class="text-headline text-foreground mb-6">
                    "Pricing"
                </h2>
                <p class="text-body-large text-foreground-secondary max-w-3xl mx-auto">
                    "Choose the deployment option that works best for your needs"
                </p>
            </div>

            <div class="grid md:grid-cols-2 gap-8 lg:gap-12 max-w-4xl mx-auto">
                <PricingCard
                    title="Self-Hosted"
                    price="Free"
                    description="Deploy and manage your own instance with full control"
                    features=vec![
                        "Complete source code".to_string(),
                        "Docker & Helm charts".to_string(),
                        "Community support".to_string(),
                        "You manage updates and security".to_string(),
                    ]
                    button_text="Get the Helm Chart"
                    button_href="https://github.com/security-union/videocall-rs/tree/main/helm"
                    variant=ButtonVariant::Secondary
                    highlighted=false
                />

                // <PricingCard
                //     title="Managed Cloud"
                //     price="Get started for free"
                //     description="Fully managed service with guaranteed uptime and support"
                //     features=vec![
                //         "99.9% uptime SLA".to_string(),
                //         "Auto-scaling infrastructure".to_string(),
                //         "24/7 support".to_string(),
                //         "Regular updates & security patches".to_string(),
                //     ]
                //     button_text="Get Started"
                //     button_href="https://app.videocall.rs"
                //     variant=ButtonVariant::Secondary
                //     highlighted=false
                // />

                <PricingCard
                    title="Enterprise"
                    price="Custom"
                    description="Tailored solutions for large organizations with specific requirements"
                    features=vec![
                        "Custom SLA terms".to_string(),
                        "Dedicated support team".to_string(),
                        "Custom feature development".to_string(),
                        "On-premise deployment options".to_string(),
                    ]
                    button_text="Contact Sales"
                    button_href="mailto:support@securityunion.dev"
                    variant=ButtonVariant::Primary
                    highlighted=true
                />
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
    #[prop(default = ButtonVariant::Secondary)] variant: ButtonVariant,
    #[prop(default = false)] highlighted: bool,
) -> impl IntoView {
    let card_class = if highlighted {
        "card-apple relative transform scale-105 ring-2 ring-primary/20"
    } else {
        "card-apple"
    };

    view! {
        <div class=format!("{} group hover:shadow-lg transition-all duration-300", card_class)>
            {if highlighted {
                view! {
                    <div class="absolute -top-4 left-1/2 transform -translate-x-1/2">
                        <span class="bg-primary text-white px-4 py-1 rounded-full text-sm font-medium">
                            "Most Popular"
                        </span>
                    </div>
                }.into_view()
            } else {
                view! {}.into_view()
            }}
            
            <div class="text-center mb-8">
                <h3 class="text-subheadline text-foreground mb-2">
                    {title}
                </h3>
                <div class="text-4xl font-bold text-foreground mb-2">
                    {price}
                </div>
                <p class="text-body text-foreground-secondary">
                    {description}
                </p>
            </div>

            <ul class="space-y-4 mb-8">
                {features.into_iter().map(|feature| view! {
                    <li class="flex items-center text-foreground-secondary">
                        <div class="w-5 h-5 rounded-full bg-primary/10 flex items-center justify-center mr-3 flex-shrink-0">
                            <svg class="w-3 h-3 text-primary" fill="currentColor" viewBox="0 0 20 20">
                                <path fill-rule="evenodd" d="M16.707 5.293a1 1 0 010 1.414l-8 8a1 1 0 01-1.414 0l-4-4a1 1 0 011.414-1.414L8 12.586l7.293-7.293a1 1 0 011.414 0z" clip-rule="evenodd" />
                            </svg>
                        </div>
                        <span class="text-sm">{feature}</span>
                    </li>
                }).collect_view()}
            </ul>

            <CTAButton
                variant=variant
                size=ButtonSize::Medium
                href=Some(button_href)
                class="w-full justify-center".to_string()
            >
                {button_text}
            </CTAButton>
        </div>
    }
}