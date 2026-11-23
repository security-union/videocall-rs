// SPDX-License-Identifier: MIT OR Apache-2.0

//! Small questions about the device the page runs on.

/// Whether a text field may take focus on its own when it appears.
///
/// On a phone or tablet the primary pointer is a finger (`pointer: coarse`),
/// and focusing a field raises the on-screen keyboard, which covers the
/// button the person came to press. There they tap the field themselves.
/// With a mouse or trackpad autofocus saves a click, so it stays.
pub fn autofocus_allowed() -> bool {
    let coarse = web_sys::window()
        .and_then(|window| window.match_media("(pointer: coarse)").ok().flatten())
        .is_some_and(|query| query.matches());
    !coarse
}
