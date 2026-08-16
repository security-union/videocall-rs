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

use leptos::either::Either;
use leptos::prelude::*;

/// Button variants mapped onto the redesign's rectilinear primitives:
/// `Primary` → solid, `Secondary` → hairline outline, `Tertiary` → ghost link.
#[derive(Clone, PartialEq)]
pub enum ButtonVariant {
    Primary,
    Secondary,
    Tertiary,
}

/// Button sizes. These supply padding + text size; the variant supplies the
/// surface, so the two never fight over the same properties.
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
    let base_classes = "disabled:opacity-50 disabled:cursor-not-allowed";

    let variant_classes = match variant {
        ButtonVariant::Primary => "btn-solid",
        ButtonVariant::Secondary => "btn-line",
        ButtonVariant::Tertiary => "btn-ghost",
    };

    let size_classes = match size {
        ButtonSize::Small => "px-4 py-2 text-sm",
        ButtonSize::Medium => "px-5 py-2.5 text-[15px]",
        ButtonSize::Large => "px-6 py-3.5 text-base",
    };

    let combined_class = format!(
        "{} {} {} {}",
        base_classes, variant_classes, size_classes, class
    );

    // `href` is not reactive, so branch once. Leptos 0.7+ replaced the
    // type-erasing `.into_view()` on mismatched branches with `Either`, which
    // also lets us consume `children()` exactly once in the taken branch.
    match href {
        Some(href) => Either::Left(view! {
            <a
                href=href
                class=combined_class
                class:pointer-events-none=disabled
            >
                {children()}
            </a>
        }),
        None => Either::Right(view! {
            <button
                class=combined_class
                disabled=disabled
            >
                {children()}
            </button>
        }),
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
                    inner_html=icon_svg
                ></div>
                <span>{text}</span>
            </div>
        </CTAButton>
    }
}
