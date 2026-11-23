// SPDX-License-Identifier: MIT OR Apache-2.0

//! The typed primitives. One component per contract class in
//! `styles/components.css`, each a thin wrapper that only picks classes and
//! ARIA attributes.
//!
//! Conventions shared by all of them:
//! - `class` is joined after the system classes. Dioxus does not merge a
//!   spread `class` with the element's own, so it is a real prop.
//! - Any other global attribute (`id`, `data-testid`, `aria_*`, `title`,
//!   `style`, ...) is forwarded to the root element, and controls also take
//!   their element's own attributes (`value`, `placeholder`, `name`, ...).
//! - Event handlers are explicit `Option<EventHandler<_>>` props, because
//!   attribute extension does not carry listeners.
//! - A component that brings an ARIA role (sheet, alert, menu, menu item,
//!   tab list) steps aside when the caller passes `role`: the caller's role
//!   replaces it, together with the attributes that only make sense with
//!   that role (`aria-modal`, `aria-orientation`). `role: None::<&str>`
//!   renders no role at all.

mod buttons;
mod call;
mod content;
mod forms;
mod icon;
mod navigation;
mod overlays;
mod status;
mod switch;

use dioxus::prelude::*;

use crate::classes::with_extra;

/// Whether the caller passed attribute `name`, even with a `None` value.
/// That is how a component knows to leave its own default out.
pub(crate) fn caller_sets(attributes: &[Attribute], name: &str) -> bool {
    attributes.iter().any(|attribute| attribute.name == name)
}

/// Props of the plain parts: [`Card`], [`CardTitle`], [`ListRowValue`],
/// [`ListHeader`], [`ListFooter`], [`ToolbarTitle`], the sheet and alert
/// parts, [`ToastRegion`] and [`MenuHeader`]. Each of them is one element
/// with one system class, the caller's `class` after it, any other global
/// attribute, and the children.
#[derive(Props, Clone, PartialEq)]
pub struct PartProps {
    /// Extra classes, joined after the system one.
    #[props(default)]
    class: Option<String>,
    #[props(extends = GlobalAttributes)]
    attributes: Vec<Attribute>,
    children: Element,
}

/// Declares a plain part.
///
/// A part is a `const` holding a plain `fn` pointer rather than a
/// `#[component]` function, and that is about wasm size. Dioxus builds its
/// per-component machinery (props comparison, cloning, rendering) once per
/// component *type*. Every `#[component]` function is its own type, so each
/// part would pay for its own copy. All parts share the one type
/// `fn(PartProps) -> Element`, so they share one copy. Dioxus still tells
/// them apart by the pointer, and `Card { .. }` in `rsx!` reads the same.
macro_rules! part {
    ($(#[$doc:meta])* $name:ident, $render:ident, $class:literal) => {
        $(#[$doc])*
        #[allow(non_upper_case_globals)]
        pub const $name: fn($crate::components::PartProps) -> Element = {
            fn render(props: $crate::components::PartProps) -> Element {
                $crate::components::$render($class, props)
            }
            render
        };
    };
}
pub(crate) use part;

// One renderer per tag. Not inlined into the parts, so the template for a
// tag exists once however many parts use it.
macro_rules! part_renderer {
    ($name:ident, $tag:ident) => {
        #[inline(never)]
        pub(crate) fn $name(base: &'static str, props: PartProps) -> Element {
            let PartProps {
                class,
                attributes,
                children,
            } = props;
            let class = with_extra(base, class.as_deref());
            rsx! {
                $tag { class, ..attributes, {children} }
            }
        }
    };
}

part_renderer!(part_div, div);
part_renderer!(part_span, span);
part_renderer!(part_h2, h2);
part_renderer!(part_h3, h3);
part_renderer!(part_p, p);

pub use buttons::*;
pub use call::*;
pub use content::*;
pub use forms::*;
pub use icon::*;
pub use navigation::*;
pub use overlays::*;
pub use status::*;
pub use switch::*;
