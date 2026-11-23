# videocall-theme

This crate owns how videocall-rs looks. It holds three stylesheets, typed Dioxus components that emit their classes, and a gallery that shows all of it on one page.

The example I give people is a fuse box. Every room is wired through it, so swapping one breaker changes every outlet on that circuit. `styles/tokens.css` is the fuse box. Colors, type, spacing and radii live there once, and every screen draws from them.

## Browse it

```sh
make theme-gallery        # trunk serve on http://127.0.0.1:3002
make theme-gallery-build  # static build in videocall-theme/gallery/dist
```

The bar at the bottom pins Light or Dark with the real `.theme-light` and `.theme-dark` classes, and simulates Increase Contrast, Reduce Transparency and Reduce Motion. The color values on the page are read from the browser, so they can't drift from tokens.css.

## Use it from an app

Link the three sheets with Trunk, tokens first:

```html
<link data-trunk rel="css" href="../videocall-theme/styles/tokens.css" />
<link data-trunk rel="css" href="../videocall-theme/styles/base.css" />
<link data-trunk rel="css" href="../videocall-theme/styles/components.css" />
```

Then use the components instead of writing class strings:

```rust
use videocall_theme::{Button, ButtonVariant};

rsx! { Button { variant: ButtonVariant::Prominent, "data-testid": "join", "Join" } }
```

Every component takes `class` and passes `id`, `data-testid` and `aria-*` through to its element. A component that brings an ARIA role (sheet, alert, menu, tab list) lets a `role` you pass replace it, and `role: None::<&str>` leaves it off. A browser test that can't use Trunk can inject `TOKENS_CSS`, `BASE_CSS` and `COMPONENTS_CSS` instead.

dioxus-ui renders its buttons, control bar, fields, cards, lists, dialogs, menus and badges through these, so the gallery shows the controls that ship. It still writes the classes by hand in a few places: markup driven from web-sys (video tiles, the speaking glow, the sidebars' containers), lists that re-render per peer or on the stats tick (peer rows, toasts, the diagnostics panel), and elements no component renders yet (a link styled as a button, headings at other levels).

## Layer order

tokens.css declares it, which is why it loads first:

`reset, tokens, base, components, features, overrides`

App screens go in `features`. A later layer wins no matter the specificity, so a feature sheet never needs `!important` to beat a primitive.

## Glass, in three lines

1. Glass is only for what floats above content: bars, sheets, menus, sidebars.
2. Never glass on glass. A control inside a glass surface becomes a fill.
3. Regular is the default. Clear is only over video, with the 35% dim when the picture is bright. A screen with live video puts `.over-media` on its root, and inside it Clear is the only glass: every Regular surface becomes an opaque elevated fill with no blur.

The full contract is the header comment of `styles/components.css`.

## Where the numbers come from

Colors and the type scale are Apple's, measured on iOS 27.2. So are the 44px hit target and the 35% dim. Apple publishes no number for glass blur, tint, radii, spacing, shadows or motion, so those are my approximations, and tokens.css marks each one.

## Tests

`cargo test -p videocall-theme` runs on the host. It covers the class mapping, the tokens.css reader, and server-side renders that check attributes reach the element. It also checks that the two copies of the dark values in tokens.css (the OS query and the `.theme-dark` pin) are still equal, since nothing generates them.
