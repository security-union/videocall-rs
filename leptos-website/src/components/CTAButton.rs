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

use leptos::*;

/// Apple-style button variants
#[derive(Clone, PartialEq)]
pub enum ButtonVariant {
    Primary,
    Secondary,
    Tertiary,
}

/// Apple-style button sizes
#[derive(Clone, PartialEq)]
pub enum ButtonSize {
    Small,
    Medium,
    Large,
}

#[component]
pub fn CTAButton(
    children: Children,
    #[prop(default = ButtonVariant::Primary)] variant: ButtonVariant,
    #[prop(default = ButtonSize::Medium)] size: ButtonSize,
    #[prop(default = String::new())] class: String,
    #[prop(default = None)] href: Option<String>,
    #[prop(default = false)] disabled: bool,
) -> impl IntoView {
    let base_classes = "inline-flex items-center justify-center font-medium transition-all duration-200 ease-out focus:outline-none focus:ring-2 focus:ring-offset-2 focus:ring-offset-background disabled:opacity-50 disabled:cursor-not-allowed";
    
    let variant_classes = match variant {
        ButtonVariant::Primary => "bg-primary text-white hover:bg-primary-dark focus:ring-primary/20 shadow-sm hover:shadow-md",
        ButtonVariant::Secondary => "bg-background-secondary text-foreground border border-border hover:bg-background-tertiary hover:border-border-secondary focus:ring-primary/20",
        ButtonVariant::Tertiary => "text-primary hover:text-primary-dark hover:bg-primary/5 focus:ring-primary/20",
    };

    let size_classes = match size {
        ButtonSize::Small => "px-4 py-2 text-sm rounded-md",
        ButtonSize::Medium => "px-6 py-3 text-base rounded-lg",
        ButtonSize::Large => "px-8 py-4 text-lg rounded-xl",
    };

    let combined_class = format!("{} {} {} {}", base_classes, variant_classes, size_classes, class);

    let content = children();

    view! {
        {move || match &href {
            Some(href) => view! {
                <a 
                    href=href 
                    class=&combined_class
                    class:pointer-events-none=disabled
                >
                    {content.clone()}
                </a>
            }.into_view(),
            None => view! {
                <button 
                    class=&combined_class
                    disabled=disabled
                >
                    {content.clone()}
                </button>
            }.into_view()
        }}
    }
}

/// Simplified button with icon for backward compatibility
#[component] 
pub fn ButtonWithIcon(
    #[prop(into)] text: String,
    #[prop(into)] icon_svg: String,
    #[prop(default = ButtonVariant::Primary)] variant: ButtonVariant,
    #[prop(default = ButtonSize::Medium)] size: ButtonSize,
    #[prop(default = String::new())] class: String,
    #[prop(default = None)] href: Option<String>,
) -> impl IntoView {
    view! {
        <CTAButton 
            variant=variant
            size=size
            class=class
            href=href
        >
            <div class="flex items-center space-x-2">
                <div 
                    class="w-5 h-5 flex-shrink-0"
                    inner_html=&icon_svg
                ></div>
                <span>{text}</span>
            </div>
        </CTAButton>
    }
}