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

dioxus-ui renders its buttons, control bar, fields, cards, lists, dialogs, menus, badges and icons through these, so the gallery shows the controls that ship. It still writes the classes by hand in a few places: markup driven from web-sys (video tiles, the speaking glow, the sidebars' containers), lists that re-render per peer or on the stats tick (peer rows, toasts, the diagnostics panel), and elements no component renders yet (a link styled as a button, headings at other levels).

## Icons

`src/icons.rs` is the one icon set. Each glyph is `&'static` data (a view box, stroke or fill, and its shapes), and one `Icon` component draws any of them, so the app pays for the render glue once. Both dioxus-ui and the gallery draw from it, and the gallery's Icons section lists every glyph by name.

```rust
use videocall_theme::{icons, Icon};

rsx! { Icon { glyph: &icons::CLOSE, size: "16", "aria-hidden": "true" } }
```

A glyph keeps the SVG elements it was drawn with, so a `line` stays a `line`. Brand marks and charts aren't icons and stay in the app.

## The call bar

The control bar is described once, by `CallControl`: each control's name, tooltip, icon, look and test ids, on and off. `CallButton` draws a control as a bar button, `CallMenuItem` as a menu row, and `CallMoreButton` is the phone bar's More. The gallery renders its bars from the same enum, so it can't drift from the app.

On a wide screen every control sits in the bar. Under 37.5em (the phone layout's breakpoint) the bar keeps mic, camera, More and hang up, and the rest are rows of the menu More opens: the same names and icons, `aria-checked` where a control has an on state. The app decides which placement to render with a media query listener, so only one exists in the DOM.

`Menu` brings the menu keys when it has its own `menu` role: Down, Up, Home and End move focus between rows, and Escape or Tab call `onclose`. Opening it, focusing the first row (`MenuItem { autofocus }`) and handing focus back to the button is the caller's job.

## Layer order

tokens.css declares it, which is why it loads first:

`reset, tokens, base, components, features, overrides`

App screens go in `features`. A later layer wins no matter the specificity, so a feature sheet never needs `!important` to beat a primitive.

## Glass, in three lines

1. Glass is only for what floats above content: bars, sheets, menus, sidebars.
2. Never glass on glass. A control inside a glass surface becomes a fill.
3. Regular is the default. Clear is only over video, with the 35% dim when the picture is bright. A screen with live video puts `.over-media` on its root, and inside it Clear is the only glass: every Regular surface becomes an opaque elevated fill with no blur.

The full contract is the header comment of `styles/components.css`.

Text that lives on video for good, like a tile's name label, sits on `--material-dim-label` instead. That's 54% black, the lowest alpha that keeps white footnote text at 4.5:1 when the picture behind it is pure white.

## Text size and contrast

On an iPhone or iPad, base.css gives the root `font: -apple-system-body`, so `1rem` follows the Dynamic Type setting. It's 17px at the default size, so `--type-scale` shrinks the text styles by 16/17 to keep them on the HIG sizes there. The rule only applies with `(hover: none) and (pointer: coarse)`, because a Mac supports the same keyword but reports a fixed 13px. An iPad with a trackpad attached reports hover, so it gets the plain 16px root and ignores Dynamic Type.

Under Increase Contrast a tinted or destructive button drops its tint for the opaque page background plus an outline. A tinted fill under a blue or red label can't reach 4.5:1 in light mode, even with the darker Increase Contrast colors.

## Where the numbers come from

Colors and the type scale are Apple's, measured on iOS 27.2. So are the 44px hit target and the 35% dim. Apple publishes no number for glass blur, tint, radii, spacing, shadows or motion, so those are my approximations, and tokens.css marks each one.

## Wasm size

Every component type costs dioxus-ui a few kilobytes of wasm, because Dioxus compiles its props comparison and render glue once per type. The single-class wrappers (`Card`, `ListRowValue`, `ListHeader`, the sheet and alert parts) are `part!` consts that share one props type, `PartProps`, so they share that glue. A new wrapper with no props of its own should be a `part!` too (see `src/components/mod.rs`).

## Tests

`cargo test -p videocall-theme` runs on the host. It covers the class mapping, the tokens.css reader, and server-side renders that check attributes reach the element. It also checks that the two copies of the dark values in tokens.css (the OS query and the `.theme-dark` pin) are still equal, since nothing generates them.
