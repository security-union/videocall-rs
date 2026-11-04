// SPDX-License-Identifier: MIT OR Apache-2.0

//! Help & FAQ page (issue 3351).

use crate::components::action_bar_layout::ALL_SLOTS;
use crate::components::density::DENSITY_MODES;
use crate::components::device_settings_modal::{
    TRANSPORT_WEBSOCKET_DESC, TRANSPORT_WEBTRANSPORT_DESC,
};
use crate::context::{DISPLAY_NAME_MAX_LEN, MEETING_ID_ALLOWED_CHARS, MEETING_ID_MAX_LEN};
use crate::routing::{is_from_call, Route};
use dioxus::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Section {
    id: &'static str,
    title: &'static str,
}

const START: Section = Section {
    id: "start",
    title: "Start or join a meeting",
};
const JOINING: Section = Section {
    id: "joining",
    title: "Before you join",
};
const CONTROLS: Section = Section {
    id: "controls",
    title: "Controls in a call",
};
const TILES: Section = Section {
    id: "tiles",
    title: "Video tiles and screen sharing",
};
const MEETING_INFO: Section = Section {
    id: "meeting-info",
    title: "Meeting info and inviting people",
};
const SETTINGS: Section = Section {
    id: "settings",
    title: "Settings",
};
const HOSTS: Section = Section {
    id: "hosts",
    title: "For hosts",
};
const SHORTCUTS: Section = Section {
    id: "shortcuts",
    title: "Keyboard shortcuts",
};
const FAQ: Section = Section {
    id: "faq",
    title: "Frequently asked questions",
};

const SECTIONS: [Section; 9] = [
    START,
    JOINING,
    CONTROLS,
    TILES,
    MEETING_INFO,
    SETTINGS,
    HOSTS,
    SHORTCUTS,
    FAQ,
];

const TOP_ID: &str = "help-top";
const TITLE_ID: &str = "help-title";
const CALL_NOTE_ID: &str = "help-call-note";
const DOCUMENT_TITLE: &str = "Help & FAQ \u{2013} videocall.rs";
const SMOOTH_SCROLL_CLASS: &str = "help-smooth-scroll";

#[derive(Clone, Debug, Default, PartialEq)]
struct Arrival {
    search: String,
    hash: String,
}

thread_local! {
    static ARRIVAL: std::cell::RefCell<Option<Arrival>> = const { std::cell::RefCell::new(None) };
}

/// Keeps the query and hash of a direct load of the help page, which the router
/// drops when it starts. Call before the app launches.
pub fn remember_arrival() {
    let Some(location) = web_sys::window().map(|w| w.location()) else {
        return;
    };
    let path = location.pathname().unwrap_or_default();
    let help_path = Route::Help {}.to_string();
    if path.strip_suffix('/').unwrap_or(&path) != help_path {
        return;
    }
    ARRIVAL.with(|slot| *slot.borrow_mut() = Some(live_arrival()));
}

fn live_arrival() -> Arrival {
    web_sys::window()
        .map(|w| w.location())
        .map(|location| Arrival {
            search: location.search().unwrap_or_default(),
            hash: location.hash().unwrap_or_default(),
        })
        .unwrap_or_default()
}

fn take_arrival() -> Arrival {
    ARRIVAL
        .with(|slot| slot.borrow_mut().take())
        .unwrap_or_else(live_arrival)
}

fn set_smooth_scrolling(on: bool) {
    let Some(root) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.document_element())
    else {
        return;
    };
    let classes = root.class_list();
    let _ = if on {
        classes.add_1(SMOOTH_SCROLL_CLASS)
    } else {
        classes.remove_1(SMOOTH_SCROLL_CLASS)
    };
}

fn restore_url(arrival: &Arrival) {
    if arrival.search.is_empty() && arrival.hash.is_empty() {
        return;
    }
    let Some(history) = web_sys::window().and_then(|w| w.history().ok()) else {
        return;
    };
    let state = history.state().unwrap_or(wasm_bindgen::JsValue::NULL);
    let url = format!("{}{}{}", Route::Help {}, arrival.search, arrival.hash);
    let _ = history.replace_state_with_url(&state, "", Some(&url));
}

fn focus_without_scrolling(el: Option<web_sys::Element>) {
    if let Some(el) =
        el.and_then(|el| wasm_bindgen::JsCast::dyn_into::<web_sys::HtmlElement>(el).ok())
    {
        let options = web_sys::FocusOptions::new();
        options.set_prevent_scroll(true);
        let _ = el.focus_with_options(&options);
    }
}

fn arrive_at(hash: String) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let land = wasm_bindgen::closure::Closure::once_into_js(move || land_on(&hash));
    let _ = window.request_animation_frame(wasm_bindgen::JsCast::unchecked_ref(&land));
}

fn land_on(hash: &str) {
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let Some(page) = doc.get_element_by_id(TOP_ID) else {
        return;
    };
    let target = hash
        .strip_prefix('#')
        .filter(|id| !id.is_empty())
        .and_then(|id| doc.get_element_by_id(id))
        .filter(|el| {
            let node: &web_sys::Node = el;
            page.contains(Some(node))
        });
    let Some(target) = target else {
        focus_without_scrolling(doc.get_element_by_id(TITLE_ID));
        return;
    };
    let focus = match target.tag_name().as_str() {
        "DETAILS" => {
            let _ = target.set_attribute("open", "");
            target.query_selector(":scope > summary").ok().flatten()
        }
        "SECTION" => target.query_selector(":scope > h2").ok().flatten(),
        _ => None,
    };
    target.scroll_into_view();
    focus_without_scrolling(focus);
}

fn density_options() -> String {
    DENSITY_MODES
        .iter()
        .map(|mode| format!("{} ({})", mode.label(), mode.description().to_lowercase()))
        .collect::<Vec<_>>()
        .join(", ")
}

#[component]
fn HelpSection(section: Section, children: Element) -> Element {
    let heading_id = format!("{}-heading", section.id);
    rsx! {
        section {
            id: section.id,
            class: "help-section",
            "aria-labelledby": "{heading_id}",
            h2 {
                id: "{heading_id}",
                class: "help-section-title",
                tabindex: "-1",
                "{section.title}"
            }
            {children}
        }
    }
}

#[component]
fn Faq(id: &'static str, question: &'static str, children: Element) -> Element {
    rsx! {
        details { id, class: "help-faq-item",
            summary { class: "help-faq-question", "{question}" }
            div { class: "help-faq-answer", {children} }
        }
    }
}

#[component]
pub fn Help() -> Element {
    let arrival = use_hook(|| {
        let arrival = take_arrival();
        restore_url(&arrival);
        arrival
    });
    let from_call = is_from_call(&arrival.search);
    let nav = use_navigator();
    let previous_title = use_hook(|| {
        let doc = web_sys::window().and_then(|w| w.document());
        let previous = doc.as_ref().map(|d| d.title());
        if let Some(doc) = &doc {
            doc.set_title(DOCUMENT_TITLE);
        }
        set_smooth_scrolling(true);
        std::rc::Rc::new(previous)
    });
    use_drop(move || {
        if let (Some(doc), Some(title)) = (
            web_sys::window().and_then(|w| w.document()),
            previous_title.as_ref(),
        ) {
            doc.set_title(title);
        }
        set_smooth_scrolling(false);
    });
    let hash = arrival.hash.clone();

    rsx! {
        main { id: TOP_ID, class: "help-page", "data-testid": "help-page",
            div { class: "help-shell",
                header { class: "help-header",
                    if !from_call {
                        Link {
                            to: Route::Home {},
                            class: "help-back",
                            "data-testid": "help-back-home",
                            onclick_only: true,
                            onclick: move |_| {
                                set_smooth_scrolling(false);
                                nav.push(Route::Home {});
                            },
                            svg {
                                "aria-hidden": "true",
                                xmlns: "http://www.w3.org/2000/svg",
                                view_box: "0 0 24 24",
                                fill: "none",
                                stroke: "currentColor",
                                stroke_width: "2",
                                stroke_linecap: "round",
                                stroke_linejoin: "round",
                                polyline { points: "15 18 9 12 15 6" }
                            }
                            "Back to home"
                        }
                    }
                    h1 {
                        id: TITLE_ID,
                        class: "help-title",
                        tabindex: "-1",
                        "aria-describedby": from_call.then_some(CALL_NOTE_ID),
                        onmounted: move |_| arrive_at(hash.clone()),
                        "Help & FAQ"
                    }
                    if from_call {
                        p {
                            id: CALL_NOTE_ID,
                            class: "help-call-note",
                            role: "note",
                            "data-testid": "help-call-note",
                            "You opened this page from a call. The call has its own tab: switch back to it any time, or close this tab. On phones, some browsers pause your camera or microphone while the call's tab is in the background."
                        }
                    }
                    p { class: "help-intro",
                        "How to start and join meetings, what every control does, and answers to common questions."
                    }
                }

                nav { class: "help-toc", "aria-labelledby": "help-toc-title",
                    p { id: "help-toc-title", class: "help-toc-title", "On this page" }
                    ol { class: "help-toc-list", role: "list",
                        for section in SECTIONS {
                            li { key: "{section.id}",
                                a { href: "#{section.id}", "{section.title}" }
                            }
                        }
                    }
                }

                HelpSection { section: START,
                    ol { class: "help-steps",
                        li {
                            "Enter your "
                            b { "Display Name" }
                            ", the name others see. It can use letters (a-z, A-Z), numbers (0-9), spaces, hyphens (-), underscores (_) and apostrophes ('), up to {DISPLAY_NAME_MAX_LEN} characters."
                        }
                        li {
                            "Enter a "
                            b { "Meeting ID" }
                            " someone shared with you, or leave it empty and choose "
                            b { "Generate a New Meeting ID" }
                            " to create a meeting. Meeting IDs can use {MEETING_ID_ALLOWED_CHARS}, up to {MEETING_ID_MAX_LEN} characters."
                        }
                        li {
                            "Choose "
                            b { "Start or Join Meeting" }
                            "."
                        }
                    }
                    ul { class: "help-list",
                        li {
                            b { "Meetings" }
                            " lists meetings you own and meetings you've joined. Select one to fill in its ID, and use the filter and sort buttons to narrow the list. Owners and co-hosts open a meeting's settings with its pencil button; owners can also delete a meeting."
                        }
                        li {
                            "If your organization uses sign-in, its sign-in button, such as "
                            b { "Sign in with Google" }
                            ", is at the top right of the home page; once you're signed in, your name there opens "
                            b { "Sign out" }
                            ". Sign in to see your meetings list."
                        }
                        li {
                            b { "About videocall-ui" }
                            " at the bottom of the home page shows version details, What's new and a link to this guide."
                        }
                    }
                }

                HelpSection { section: JOINING,
                    ul { class: "help-list",
                        li { "The join screen asks for camera and microphone access and shows a live preview of your camera." }
                        li { "Turn your camera and microphone on or off before you go in. With the microphone on, the level meter moves when you speak." }
                        li { "Pick the camera, microphone and, where your browser supports it, the speaker to use. This browser remembers your choices." }
                        li { "If you're signed in and your camera is off, select the avatar in the preview to set one." }
                        li {
                            "Choose "
                            b { "Join Meeting" }
                            ". If you own the meeting, the button says "
                            b { "Start Meeting" }
                            " and you can set meeting options first."
                        }
                    }
                    p { "Depending on the meeting, you may also:" }
                    ul { class: "help-list",
                        li { "be asked for the meeting password;" }
                        li { "wait in a waiting room until a host admits you;" }
                        li { "wait for the host to start the meeting. You join automatically when it begins." }
                    }
                }

                HelpSection { section: CONTROLS,
                    p {
                        "The action bar holds your call controls. Point at a control, or move to it with the keyboard, to see what it does. Some controls appear only for hosts, or when your organization has turned them on."
                    }
                    dl { class: "help-defs", "data-testid": "help-controls",
                        for slot in ALL_SLOTS.iter().copied() {
                            div { key: "{slot.slug()}", class: "help-def", "data-slot": slot.slug(),
                                dt { "{slot.display_name()}" }
                                dd { "{slot.help_text()}" }
                            }
                        }
                        div { class: "help-def",
                            dt { "Hang up" }
                            dd { "Leave the call. Other participants stay connected, unless you're the last host and End meeting when host leaves is on." }
                        }
                    }
                    h3 { "Arranging the action bar" }
                    ul { class: "help-list",
                        li {
                            "When the bar doesn't fit, extra controls move into "
                            b { "More actions" }
                            "."
                        }
                        li {
                            "The "
                            b { "Action bar position" }
                            " menu puts the bar at the bottom, left or right, and turns on "
                            b { "Auto-hide action bar" }
                            ", which fades the bar out when idle, or "
                            b { "Auto-shrink action bar" }
                            ", which shows fewer buttons until you need them."
                        }
                        li {
                            "While the bar is shrunk, point at it or press Tab to reach every control. On a touch screen, tap its handle. Choose which buttons stay under "
                            b { "Buttons to keep" }
                            " in the Action Bar section of Settings; with none selected, the bar shrinks to a small handle."
                        }
                        li {
                            "Choose "
                            b { "Edit Buttons" }
                            " in that menu, or hold a button for 2 seconds, then drag controls into a new order (or use the arrow keys, Home and End) and remove the ones you don't need. Microphone and Camera always stay. Select the check mark ("
                            b { "Done editing buttons" }
                            ") or press Escape when you finish."
                        }
                        li {
                            b { "Reset Buttons" }
                            " brings back the original bar and the default kept buttons, including controls you removed."
                        }
                    }
                }

                HelpSection { section: TILES,
                    ul { class: "help-list",
                        li { "Select a tile's pin button to keep it at the front of the grid. Pinning changes only your view." }
                        li { "Select a tile's enlarge button to show it larger at the front of the grid. Other tiles may get smaller to fit, and the same button restores it. The button appears only when a tile can get noticeably bigger, so you may not see it on phones, in crowded grids, or in wide, short windows with few participants. Other participants don't see this change." }
                        li {
                            "To rename yourself, open the participants list and select the pencil next to your name. When your own tile is in the grid (its "
                            b { "Move self view to grid" }
                            " button puts it there), you can also select your name on the tile. Press Enter to save or Escape to cancel."
                        }
                        li { "When someone's camera is off, their tile shows their avatar." }
                        li {
                            "By default, a glow highlights whoever is speaking. Turn it off or change it in "
                            b { "Settings" }
                            " under Appearance."
                        }
                        li {
                            b { "Density mode" }
                            " sets how many tiles fit on screen: {density_options()}."
                        }
                        li { "Shared screens get their own tile with controls to zoom in and out, reset the zoom, enlarge it and pin it. When zoomed in, drag or use the arrow keys to look around. You can also view someone else's share at actual size and, on a large enough screen, open a share in a separate window." }
                    }
                }

                HelpSection { section: MEETING_INFO,
                    ul { class: "help-list",
                        li { "The bar along the bottom of a call shows that the meeting is live, how long it has run, the meeting ID and how many people are in it." }
                        li { "Select the participant count to open the participants list." }
                        li {
                            "Select anywhere else on that bar to open "
                            b { "Meeting info" }
                            ": the meeting ID and link (with "
                            b { "Copy link" }
                            "), when the meeting started, how long it and you have been in it, the number of participants, the app version and What's new."
                        }
                        li {
                            "To invite someone, send them the meeting link. When you're the only one in the meeting, the "
                            b { "Your meeting is ready!" }
                            " card has a button that copies it."
                        }
                        li { "The question-mark button at the right end of the bar opens this guide in a new tab, so the call page stays open." }
                    }
                }

                HelpSection { section: SETTINGS,
                    p {
                        "Open "
                        b { "Settings" }
                        " from the action bar. Use the arrow keys to move between its tabs and Escape to close it."
                    }
                    dl { class: "help-defs",
                        div { class: "help-def",
                            dt { "Audio" }
                            dd { "Choose your microphone and speaker. If there's no speaker choice, or your browser can't switch speakers, sound plays through the speaker your device's own settings choose." }
                        }
                        div { class: "help-def",
                            dt { "Video" }
                            dd { "Choose your camera." }
                        }
                        div { class: "help-def",
                            dt { "Network" }
                            dd { "See which connection type is in use and pick WebTransport or WebSocket. Turn on Remember protocol choice to keep it across browser sessions. Applying a change reloads the page." }
                        }
                        div { class: "help-def",
                            dt { "Appearance" }
                            dd { "Choose a Dark, Light or System theme, import a theme file (.json) or go back to the default, and adjust the speaker highlight." }
                        }
                        div { class: "help-def",
                            dt { "Preferences" }
                            dd {
                                ul { class: "help-list",
                                    li { "Place the action bar and turn on auto-hide." }
                                    li { "Choose how many recent actions to remember." }
                                    li { "Choose tile density." }
                                    li { "Put your own video in a corner or in the grid, or hide it. Others still see you." }
                                    li { "Set how many video tiles to show." }
                                    li { "Choose whether someone joining or leaving shows a message, plays a sound, or both." }
                                    li { "Choose whether a hand being raised or lowered plays a sound." }
                                }
                            }
                        }
                        div { class: "help-def",
                            dt { "Profile" }
                            dd { "Set your avatar by uploading a PNG, JPEG or WebP image, or by using your Gravatar if your organization allows it. It shows on your tile when your camera is off. Available when you're signed in." }
                        }
                    }
                }

                HelpSection { section: HOSTS,
                    p { "Whoever creates a meeting owns it. The owner can make other signed-in participants co-hosts, who share the host controls." }
                    h3 { "Meeting options" }
                    p {
                        "Set these on the join screen, during the call with "
                        b { "Meeting options" }
                        ", or on the meeting's settings page (the pencil in "
                        b { "Meetings" }
                        " on the home page). Changes apply to everyone immediately."
                    }
                    dl { class: "help-defs",
                        div { class: "help-def",
                            dt { "Waiting Room" }
                            dd { "People wait until a host admits them." }
                        }
                        div { class: "help-def",
                            dt { "Admitted can admit" }
                            dd { "People already in the meeting can admit others too. Needs the waiting room." }
                        }
                        div { class: "help-def",
                            dt { "End meeting when host leaves" }
                            dd { "The meeting ends for everyone when the last host leaves." }
                        }
                        div { class: "help-def",
                            dt { "Allow guests" }
                            dd { "People can join without an account. The settings page shows the guest join link." }
                        }
                        div { class: "help-def",
                            dt { "Allow recording for all" }
                            dd { "Every admitted participant except guests gets the Record button. When off, only hosts get it." }
                        }
                        div { class: "help-def",
                            dt { "Join & leave notifications" }
                            dd { "Turn join and leave messages or sounds on or off for everyone, or leave the choice to each person." }
                        }
                    }
                    h3 { "Managing people" }
                    ul { class: "help-list",
                        li {
                            "When people are waiting, a "
                            b { "Waiting to join" }
                            " panel lets you admit or reject each person, or admit everyone at once."
                        }
                        li {
                            "A participant's "
                            b { "Host actions" }
                            " menu on their tile offers Mute and Disable video while their microphone or camera is on, Remove from meeting and Transfer host; the owner also gets Make co-host. The same choices are under "
                            b { "More options" }
                            " next to their name in the participants list. Some aren't offered for other hosts or for guests."
                        }
                        li {
                            "The participants list's own "
                            b { "Host actions" }
                            " menu has Mute all and Disable video for all. A muted person sees a message and can unmute themselves."
                        }
                        li { "Remove from meeting isn't a ban: without a waiting room, a removed person can join again. With the waiting room on, they have to be admitted again; keep Admitted can admit off if only hosts should decide." }
                        li { "The owner can add co-hosts by email or user ID in the Co-hosts section of the join screen or the settings page." }
                        li { "From the settings page, the owner can end the meeting for everyone or delete it. Deleting can't be undone." }
                    }
                }

                HelpSection { section: SHORTCUTS,
                    dl { class: "help-defs help-keys",
                        div { class: "help-def",
                            dt {
                                kbd { "Ctrl" }
                                " + "
                                kbd { "K" }
                                span { class: "help-key-alt",
                                    " ("
                                    kbd {
                                        span { "aria-hidden": "true", "\u{2318}" }
                                        span { class: "visually-hidden", "Command" }
                                    }
                                    " + "
                                    kbd { "K" }
                                    " on Mac)"
                                }
                            }
                            dd { "Search meetings from any page." }
                        }
                        div { class: "help-def",
                            dt { kbd { "Esc" } }
                            dd { "Close Settings, Meeting info, About, search, the Diagnostics panel or the participants list." }
                        }
                        div { class: "help-def",
                            dt { kbd { "\u{2190}" } " " kbd { "\u{2192}" } " " kbd { "\u{2191}" } " " kbd { "\u{2193}" } }
                            dd { "Move between Settings tabs. While customizing the action bar, move the focused control." }
                        }
                        div { class: "help-def",
                            dt { kbd { "Home" } " " kbd { "End" } }
                            dd { "While customizing the action bar, move the focused control to the start or end." }
                        }
                    }
                }

                HelpSection { section: FAQ,
                    div { class: "help-faq",
                        Faq { id: "faq-blocked-devices", question: "My browser blocked my camera or microphone. What do I do?",
                            p { "The join screen asks for camera and microphone access when it opens. If you said no, allow both for this site in your browser's site settings, then reload the page." }
                            p { "In a call, a \"!\" on the microphone or camera button means that device isn't available. Select the button to try again." }
                        }
                        Faq { id: "faq-others-cant-hear-me", question: "Other people can't hear me.",
                            p { "Check that the microphone button isn't muted (a line through the microphone means muted). In Settings, open Audio and make sure the right microphone is selected. On the join screen, the level meter moves when the microphone picks up your voice." }
                            p { "If a host muted you, a message says so. Select the microphone button to unmute." }
                        }
                        Faq { id: "faq-cant-hear-anyone", question: "I can't hear anyone.",
                            p { "Check your device's volume, then choose the right speaker in Settings under Audio. If there's no speaker choice there, or your browser can't switch speakers, sound plays through the speaker your device's own settings choose." }
                        }
                        Faq { id: "faq-paused-video", question: "Why is someone's video paused?",
                            p { "On a busy device the app can pause some videos to keep the call smooth. Those people stay audible and show their avatar with a play button; select it to resume their video. A notice such as \"2 videos paused\" offers Show all videos." }
                            p { "To decide for good, open Settings, then Preferences, then Video tiles. Showing every video may stutter on a slower device." }
                        }
                        Faq { id: "faq-signal-indicator", question: "What does the signal indicator on a tile mean?",
                            p { "It rates how healthy a connection is right now, and its colour changes with it. Your own tile always shows one for your connection; select it to open Diagnostics." }
                            p { "To see it on other people's tiles, open Diagnostics and turn on Show diagnostics on tiles, then select a tile's indicator for details." }
                        }
                        Faq { id: "faq-invite", question: "How do I invite someone?",
                            p { "Select the bar along the bottom of the call to open Meeting info, choose Copy link and send the link. When you're the only one in the meeting, the Your meeting is ready! card can copy it too. People can also enter the meeting ID on the home page." }
                        }
                        Faq { id: "faq-browsers", question: "Which browsers are supported?",
                            p { "Use a current desktop version of Chrome, Edge, Brave or Safari. When the home page or join screen opens, the app checks that your browser has the video features it needs and names anything missing." }
                            p { "Firefox works only if your organization has turned it on. On iPhone and iPad, screen sharing isn't available." }
                        }
                        Faq { id: "faq-webtransport-websocket", question: "What are WebTransport and WebSocket?",
                            p { "They're the two ways the app can connect to the meeting server. Settings shows which one is in use under Network, and lets you choose. Applying a change reloads the page." }
                            dl { class: "help-defs",
                                div { class: "help-def",
                                    dt { "WebTransport" }
                                    dd { "{TRANSPORT_WEBTRANSPORT_DESC}" }
                                }
                                div { class: "help-def",
                                    dt { "WebSocket" }
                                    dd { "{TRANSPORT_WEBSOCKET_DESC}" }
                                }
                            }
                            p { "If your organization hasn't turned WebTransport on, it shows as unavailable." }
                        }
                        Faq { id: "faq-account", question: "Do I need an account?",
                            p { "That depends on your organization. Where sign-in is required, opening a meeting link takes you to sign in, unless the host allows guests; then you can join as a guest with just a display name." }
                            p { "Guests can't use Record, set an avatar or become co-hosts." }
                        }
                        Faq { id: "faq-hosts", question: "What can hosts do?",
                            p {
                                "Hosts admit people from the waiting room, mute people or turn off their camera, remove people, hand over the host role, change meeting options, run a countdown timer and turn captions on for everyone. See "
                                a { class: "help-link", href: "#{HOSTS.id}", "{HOSTS.title}" }
                                "."
                            }
                        }
                        Faq { id: "faq-recordings", question: "Where are recordings saved?",
                            p { "On your device. If your browser supports it, it asks where to save the file when you start recording; otherwise the file downloads when you stop. When someone uses Record, everyone in the call sees a recording notice. The app can't detect other ways of recording, such as screen capture or a modified app." }
                        }
                    }
                }

                p { class: "help-top-link",
                    a { class: "help-link", href: "#{TOP_ID}", "Back to top" }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_ids_are_unique_and_not_the_top_anchor() {
        for (i, section) in SECTIONS.iter().enumerate() {
            assert_ne!(section.id, TOP_ID);
            assert!(
                SECTIONS[i + 1..].iter().all(|s| s.id != section.id),
                "{}",
                section.id
            );
        }
    }

    #[test]
    fn density_options_name_every_mode() {
        let text = density_options();
        for mode in DENSITY_MODES {
            assert!(text.contains(mode.label()), "{text}");
        }
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod dom_tests {
    use super::*;
    use crate::components::action_bar_layout::ActionBarSlot;
    use gloo_timers::future::TimeoutFuture;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::wasm_bindgen_test;

    #[derive(Clone, Routable, PartialEq, Debug)]
    #[rustfmt::skip]
    enum HelpTestRoute {
        #[route("/help-test-elsewhere")]
        HelpTestElsewhere {},
        #[route("/:..segments")]
        HelpAt { segments: Vec<String> },
    }

    #[component]
    fn HelpTestElsewhere() -> Element {
        rsx! { p { "elsewhere" } }
    }

    #[component]
    fn HelpAt(segments: Vec<String>) -> Element {
        let _ = segments;
        let mut shown = use_signal(|| true);
        rsx! {
            button { id: "help-test-hide", onclick: move |_| shown.set(false), "hide" }
            button { id: "help-test-show", onclick: move |_| shown.set(true), "show" }
            Link { id: "help-test-leave", to: HelpTestRoute::HelpTestElsewhere {}, "leave" }
            if shown() {
                Help {}
            }
        }
    }

    fn current_url() -> String {
        let location = gloo_utils::window().location();
        format!(
            "{}{}{}",
            location.pathname().unwrap_or_default(),
            location.search().unwrap_or_default(),
            location.hash().unwrap_or_default()
        )
    }

    fn set_url(url: &str) {
        gloo_utils::window()
            .history()
            .unwrap()
            .replace_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(url))
            .unwrap();
    }

    async fn mount_help_loaded_at(url: &str) -> (web_sys::Element, String) {
        let original = current_url();
        set_url(url);
        remember_arrival();
        (mount_help().await, original)
    }

    async fn wait_until(mut done: impl FnMut() -> bool) -> bool {
        for _ in 0..100 {
            if done() {
                return true;
            }
            TimeoutFuture::new(20).await;
        }
        done()
    }

    fn active_id() -> String {
        gloo_utils::document()
            .active_element()
            .map(|el| el.id())
            .unwrap_or_default()
    }

    fn app() -> Element {
        rsx! { Router::<HelpTestRoute> {} }
    }

    async fn mount_help() -> web_sys::Element {
        let doc = gloo_utils::document();
        let mount = doc.create_element("div").unwrap();
        doc.body().unwrap().append_child(&mount).unwrap();
        dioxus::web::launch::launch_virtual_dom(
            VirtualDom::new(app),
            dioxus::web::Config::new().rootelement(mount.clone()),
        );
        for _ in 0..100 {
            if mount
                .query_selector("[data-testid='help-page']")
                .unwrap()
                .is_some()
            {
                return mount;
            }
            TimeoutFuture::new(20).await;
        }
        panic!("the help page never rendered");
    }

    fn all(mount: &web_sys::Element, selector: &str) -> Vec<web_sys::Element> {
        let nodes = mount.query_selector_all(selector).unwrap();
        (0..nodes.length())
            .filter_map(|i| nodes.item(i))
            .filter_map(|n| n.dyn_into::<web_sys::Element>().ok())
            .collect()
    }

    #[wasm_bindgen_test]
    async fn the_page_is_one_main_landmark_with_one_h1_and_an_h2_per_section() {
        let mount = mount_help().await;
        let main = all(&mount, "main");
        let h1 = all(&mount, "h1");
        let h2: Vec<String> = all(&mount, "main section > h2")
            .iter()
            .map(|h| h.text_content().unwrap_or_default())
            .collect();
        let back = mount
            .query_selector("[data-testid='help-back-home']")
            .unwrap()
            .and_then(|a| a.get_attribute("href"));
        let call_note = mount
            .query_selector("[data-testid='help-call-note']")
            .unwrap()
            .is_some();
        let described_by = h1[0].get_attribute("aria-describedby");
        mount.remove();

        assert_eq!(main.len(), 1);
        assert_eq!(h1.len(), 1);
        assert_eq!(h1[0].text_content().as_deref(), Some("Help & FAQ"));
        assert_eq!(
            h2,
            SECTIONS
                .iter()
                .map(|s| s.title.to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(back.as_deref(), Some("/"));
        assert!(!call_note, "a plain visit is not call mode");
        assert_eq!(described_by, None);
    }

    #[wasm_bindgen_test]
    async fn the_tab_is_titled_while_the_page_is_open_and_gets_its_old_title_back() {
        let doc = gloo_utils::document();
        doc.set_title("Before help");
        let root = doc.document_element().unwrap();
        let mount = mount_help().await;
        let while_open = doc.title();
        let smooth_while_open = root.class_list().contains(SMOOTH_SCROLL_CLASS);
        mount
            .query_selector("#help-test-hide")
            .unwrap()
            .unwrap()
            .unchecked_ref::<web_sys::HtmlElement>()
            .click();
        let closed = wait_until(|| {
            mount
                .query_selector("[data-testid='help-page']")
                .unwrap()
                .is_none()
        })
        .await;
        let after = doc.title();
        let smooth_after = root.class_list().contains(SMOOTH_SCROLL_CLASS);
        mount.remove();

        assert_eq!(while_open, DOCUMENT_TITLE);
        assert!(closed, "premise: the page unmounted");
        assert_eq!(after, "Before help");
        assert!(
            smooth_while_open,
            "smooth scrolling is on while the page is open"
        );
        assert!(!smooth_after, "and off again for the rest of the app");
    }

    #[wasm_bindgen_test]
    async fn opened_from_a_call_it_says_the_call_is_still_open_and_offers_no_way_home() {
        let (mount, original) = mount_help_loaded_at("/help?from=call").await;
        let note_el = mount
            .query_selector("[role='note'][data-testid='help-call-note']")
            .unwrap();
        let note = note_el.as_ref().and_then(|n| n.text_content());
        let h1 = mount.query_selector("h1").unwrap().unwrap();
        let note_follows_h1 = note_el.as_ref().is_some_and(|n| {
            h1.compare_document_position(n) & web_sys::Node::DOCUMENT_POSITION_FOLLOWING != 0
        });
        let described_by_note = h1
            .get_attribute("aria-describedby")
            .and_then(|id| gloo_utils::document().get_element_by_id(&id))
            .zip(note_el.as_ref())
            .is_some_and(|(described, n)| described.is_same_node(Some(n)));
        let back = mount
            .query_selector("[data-testid='help-back-home']")
            .unwrap()
            .is_some();
        let url = current_url();
        mount.remove();
        set_url(&original);

        assert_eq!(
            note.as_deref(),
            Some("You opened this page from a call. The call has its own tab: switch back to it any time, or close this tab. On phones, some browsers pause your camera or microphone while the call's tab is in the background.")
        );
        assert!(
            note_follows_h1,
            "the note follows the focused heading, so it is read after it"
        );
        assert!(described_by_note, "the heading is described by the note");
        assert!(!back, "Back to home would re-join from this tab");
        assert_eq!(
            url, "/help?from=call",
            "the query survives the router start-up"
        );
    }

    #[wasm_bindgen_test]
    async fn arriving_without_a_hash_puts_focus_on_the_heading() {
        if let Some(active) = gloo_utils::document()
            .active_element()
            .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok())
        {
            let _ = active.blur();
        }
        let mount = mount_help().await;
        let focused = wait_until(|| active_id() == TITLE_ID).await;
        let tabindex = mount
            .query_selector("h1")
            .unwrap()
            .and_then(|h| h.get_attribute("tabindex"));
        mount.remove();

        assert!(focused, "focus is on {:?}", active_id());
        assert_eq!(tabindex.as_deref(), Some("-1"));
    }

    #[wasm_bindgen_test]
    async fn a_link_to_a_question_opens_it_in_view_without_moving_focus_to_the_heading() {
        let (mount, original) = mount_help_loaded_at("/help#faq-browsers").await;
        let opened = wait_until(|| {
            mount
                .query_selector("#faq-browsers[open]")
                .unwrap()
                .is_some()
        })
        .await;
        let in_view = wait_until(|| {
            let top = mount
                .query_selector("#faq-browsers")
                .unwrap()
                .unwrap()
                .get_bounding_client_rect()
                .top();
            let height = gloo_utils::window()
                .inner_height()
                .unwrap()
                .as_f64()
                .unwrap();
            (0.0..height).contains(&top)
        })
        .await;
        let others_open = all(&mount, "details[open]").len();
        let summary = mount
            .query_selector("#faq-browsers > summary")
            .unwrap()
            .unwrap();
        let focused_summary = wait_until(|| {
            gloo_utils::document()
                .active_element()
                .is_some_and(|active| active.is_same_node(Some(&summary)))
        })
        .await;
        let url = current_url();
        mount.remove();
        set_url(&original);
        gloo_utils::window().scroll_to_with_x_and_y(0.0, 0.0);

        assert!(opened, "the linked question opens");
        assert!(in_view, "the linked question scrolls into view");
        assert_eq!(others_open, 1);
        assert!(
            focused_summary,
            "Tab continues from the linked question, not the top"
        );
        assert_eq!(
            url, "/help#faq-browsers",
            "the hash survives the router start-up"
        );
    }

    #[wasm_bindgen_test]
    async fn a_link_to_a_section_moves_focus_to_its_heading() {
        let (mount, original) = mount_help_loaded_at("/help#settings").await;
        let focused = wait_until(|| active_id() == "settings-heading").await;
        let focused_on = active_id();
        mount.remove();
        set_url(&original);
        gloo_utils::window().scroll_to_with_x_and_y(0.0, 0.0);

        assert!(focused, "focus is on {focused_on:?}");
    }

    #[wasm_bindgen_test]
    async fn a_hash_naming_an_element_outside_the_page_is_ignored() {
        let (mount, original) = mount_help_loaded_at("/help#help-test-hide").await;
        let focused = wait_until(|| active_id() == TITLE_ID).await;
        let focused_on = active_id();
        mount.remove();
        set_url(&original);

        assert!(focused, "focus is on {focused_on:?}");
    }

    #[wasm_bindgen_test]
    async fn only_a_load_of_the_help_path_is_remembered() {
        let (mount, original) = mount_help_loaded_at("/x?from=call#faq-browsers").await;
        let focused = wait_until(|| active_id() == TITLE_ID).await;
        let call_note = mount
            .query_selector("[data-testid='help-call-note']")
            .unwrap()
            .is_some();
        let url = current_url();
        let focused_on = active_id();
        mount.remove();
        set_url(&original);

        assert!(!call_note, "another path's query is not this page's");
        assert_eq!(url, "/x", "nothing is restored");
        assert!(focused, "focus is on {focused_on:?}");
    }

    #[wasm_bindgen_test]
    async fn a_trailing_slash_still_opens_call_mode() {
        let (mount, original) = mount_help_loaded_at("/help/?from=call").await;
        let call_note = mount
            .query_selector("[data-testid='help-call-note']")
            .unwrap()
            .is_some();
        mount.remove();
        set_url(&original);

        assert!(call_note);
    }

    #[wasm_bindgen_test]
    async fn an_arrival_is_used_by_one_mount_only() {
        let (first, original) = mount_help_loaded_at("/help?from=call").await;
        let first_note = first
            .query_selector("[data-testid='help-call-note']")
            .unwrap()
            .is_some();
        first.remove();
        set_url(&original);
        let second = mount_help().await;
        let second_note = second
            .query_selector("[data-testid='help-call-note']")
            .unwrap()
            .is_some();
        second.remove();

        assert!(first_note, "premise: the first mount is in call mode");
        assert!(!second_note, "a later visit is a plain visit");
    }

    #[wasm_bindgen_test]
    async fn every_id_on_the_page_is_unique() {
        let mount = mount_help().await;
        let mut ids: Vec<String> = all(&mount, "main [id]").iter().map(|el| el.id()).collect();
        mount.remove();
        let total = ids.len();
        ids.sort();
        ids.dedup();

        assert!(total > SECTIONS.len());
        assert_eq!(ids.len(), total);
    }

    #[wasm_bindgen_test]
    async fn every_contents_link_targets_a_section_on_the_page() {
        let mount = mount_help().await;
        let links = all(&mount, "nav.help-toc a");
        let mut missing = Vec::new();
        for link in &links {
            let href = link.get_attribute("href").unwrap_or_default();
            let target = href.strip_prefix('#').unwrap_or_default().to_string();
            let found = gloo_utils::document()
                .get_element_by_id(&target)
                .is_some_and(|el| {
                    let node: &web_sys::Node = &el;
                    el.tag_name() == "SECTION" && mount.contains(Some(node))
                });
            if !found {
                missing.push(href);
            }
        }
        mount.remove();

        assert_eq!(links.len(), SECTIONS.len());
        assert!(missing.is_empty(), "no section for {missing:?}");
    }

    #[wasm_bindgen_test]
    async fn the_controls_section_has_one_entry_per_action_bar_slot() {
        let mount = mount_help().await;
        let entries: Vec<(String, String, String)> =
            all(&mount, "[data-testid='help-controls'] [data-slot]")
                .iter()
                .map(|row| {
                    let text = |sel: &str| {
                        row.query_selector(sel)
                            .unwrap()
                            .and_then(|el| el.text_content())
                            .unwrap_or_default()
                    };
                    (
                        row.get_attribute("data-slot").unwrap_or_default(),
                        text("dt"),
                        text("dd"),
                    )
                })
                .collect();
        mount.remove();

        let expected: Vec<(String, String, String)> = ALL_SLOTS
            .iter()
            .map(|slot: &ActionBarSlot| {
                (
                    slot.slug().to_string(),
                    slot.display_name().to_string(),
                    slot.help_text().to_string(),
                )
            })
            .collect();
        assert_eq!(entries, expected);
        assert!(expected
            .iter()
            .any(|(slug, _, _)| slug == ActionBarSlot::Captions.slug()));
    }

    #[wasm_bindgen_test]
    async fn the_page_fits_a_phone_width_without_sideways_scrolling() {
        let doc = gloo_utils::document();
        let style = doc.create_element("style").unwrap();
        style.set_text_content(Some(&format!(
            "{}{}",
            include_str!("../../static/style.css"),
            include_str!("../../static/global.css"),
        )));
        doc.head().unwrap().append_child(&style).unwrap();
        let mount = mount_help().await;
        mount.set_attribute("style", "width: 320px;").unwrap();
        for details in all(&mount, "details") {
            details.set_attribute("open", "").unwrap();
        }
        TimeoutFuture::new(50).await;
        let main = mount.query_selector("main").unwrap().unwrap();
        let (scroll_w, client_w) = (main.scroll_width(), main.client_width());
        let right_edge = main.get_bounding_client_rect().right();
        let sticking_out: Vec<String> = all(&mount, "main *")
            .iter()
            .filter(|el| el.get_bounding_client_rect().right() > right_edge + 0.5)
            .map(|el| el.tag_name())
            .collect();
        style.remove();
        mount.remove();

        assert!(client_w <= 320, "premise: a 320px column, got {client_w}");
        assert!(scroll_w <= client_w, "{scroll_w} > {client_w}");
        assert!(sticking_out.is_empty(), "{sticking_out:?}");
    }

    #[wasm_bindgen_test]
    async fn focusing_the_heading_on_arrival_leaves_the_scroll_position_alone() {
        let doc = gloo_utils::document();
        let window = gloo_utils::window();
        let spacer = doc.create_element("div").unwrap();
        spacer.set_attribute("style", "height: 4000px;").unwrap();
        doc.body().unwrap().append_child(&spacer).unwrap();
        let original = current_url();
        set_url("/help");
        window.scroll_to_with_x_and_y(0.0, 600.0);
        let before = window.scroll_y().unwrap();
        let mount = mount_help().await;
        let focused = wait_until(|| active_id() == TITLE_ID).await;
        let after = window.scroll_y().unwrap();
        mount.remove();
        spacer.remove();
        set_url(&original);
        window.scroll_to_with_x_and_y(0.0, 0.0);

        assert!(before > 0.0, "premise: the page is scrolled");
        assert!(focused, "premise: the heading took focus");
        assert_eq!(
            after, before,
            "focusing the heading must not scroll the page"
        );
    }

    #[wasm_bindgen_test]
    async fn the_theme_background_stays_behind_the_whole_scroll_without_covering_links() {
        let doc = gloo_utils::document();
        let window = gloo_utils::window();
        let style = doc.create_element("style").unwrap();
        style.set_text_content(Some(&format!(
            "{}{}",
            include_str!("../../static/style.css"),
            include_str!("../../static/global.css"),
        )));
        doc.head().unwrap().append_child(&style).unwrap();
        let mount = mount_help().await;
        let main = mount.query_selector("main").unwrap().unwrap();
        let root = doc.document_element().unwrap();
        let end = f64::from(root.scroll_height() - root.client_height());
        window.scroll_to_with_x_and_y(0.0, end);
        let reached_end = wait_until(|| window.scroll_y().unwrap() >= end - 1.0).await;
        let layer = window
            .get_computed_style_with_pseudo_elt(&main, "::before")
            .unwrap()
            .unwrap();
        let prop = |name: &str| layer.get_property_value(name).unwrap();
        let (content, position, top, bottom_edge, image, pointer, z_index) = (
            prop("content"),
            prop("position"),
            prop("top"),
            prop("bottom"),
            prop("background-image"),
            prop("pointer-events"),
            prop("z-index"),
        );
        let isolation = window
            .get_computed_style(&main)
            .unwrap()
            .unwrap()
            .get_property_value("isolation")
            .unwrap();
        let catch_presses = doc.create_element("style").unwrap();
        catch_presses.set_text_content(Some(
            ".help-page::before { pointer-events: auto !important; }",
        ));
        doc.head().unwrap().append_child(&catch_presses).unwrap();
        let link = mount.query_selector(".help-top-link a").unwrap().unwrap();
        let rect = link.get_bounding_client_rect();
        let hit = doc.element_from_point(
            ((rect.left() + rect.right()) / 2.0) as f32,
            ((rect.top() + rect.bottom()) / 2.0) as f32,
        );
        let link_is_hit = hit.is_some_and(|h| h.is_same_node(Some(&link)));
        catch_presses.remove();
        let pill_background = |selector: &str| {
            let el = mount.query_selector(selector).unwrap().unwrap();
            window
                .get_computed_style(&el)
                .unwrap()
                .unwrap()
                .get_property_value("background-color")
                .unwrap()
        };
        let pills = [
            pill_background(".help-top-link a"),
            pill_background("[data-testid='help-back-home']"),
        ];
        let no_background = pill_background(".help-top-link");
        mount.remove();
        style.remove();
        window.scroll_to_with_x_and_y(0.0, 0.0);

        assert!(reached_end, "premise: the page scrolled to its end");
        assert_ne!(content, "none", "the page paints its own theme layer");
        assert_eq!(
            position, "fixed",
            "the layer stays in the viewport while the page scrolls"
        );
        assert_eq!((top.as_str(), bottom_edge.as_str()), ("0px", "0px"));
        assert_ne!(image, "none", "the layer carries the theme image");
        assert_eq!(pointer, "none");
        assert_eq!(z_index, "-1", "the layer sits behind the page's content");
        assert_eq!(
            isolation, "isolate",
            "the page's own stacking context keeps the layer above the body's background"
        );
        assert!(
            link_is_hit,
            "even a layer that caught presses would sit under the page's links"
        );
        for background in pills {
            assert_ne!(
                background, no_background,
                "Back to top and Back to home sit on their own surface"
            );
        }
    }

    #[wasm_bindgen_test]
    async fn smooth_scrolling_is_on_only_while_the_page_is_open() {
        let doc = gloo_utils::document();
        let style = doc.create_element("style").unwrap();
        style.set_text_content(Some(&format!(
            "{}{}",
            include_str!("../../static/style.css"),
            include_str!("../../static/global.css"),
        )));
        doc.head().unwrap().append_child(&style).unwrap();
        let root = doc.document_element().unwrap();
        let scroll_behavior = || {
            gloo_utils::window()
                .get_computed_style(&root)
                .unwrap()
                .unwrap()
                .get_property_value("scroll-behavior")
                .unwrap()
        };
        let reduced_motion = gloo_utils::window()
            .match_media("(prefers-reduced-motion: reduce)")
            .unwrap()
            .is_some_and(|m| m.matches());
        let mount = mount_help().await;
        let while_open = scroll_behavior();
        mount
            .query_selector("#help-test-hide")
            .unwrap()
            .unwrap()
            .unchecked_ref::<web_sys::HtmlElement>()
            .click();
        let closed = wait_until(|| {
            mount
                .query_selector("[data-testid='help-page']")
                .unwrap()
                .is_none()
        })
        .await;
        let after = scroll_behavior();
        mount.remove();
        style.remove();

        assert!(
            !reduced_motion,
            "premise: the browser does not ask for reduced motion"
        );
        assert!(closed, "premise: the page unmounted");
        assert_eq!(while_open, "smooth");
        assert_eq!(after, "auto");
    }

    #[wasm_bindgen_test]
    async fn back_to_home_turns_smooth_scrolling_off_before_the_router_scrolls_to_the_top() {
        let doc = gloo_utils::document();
        let style = doc.create_element("style").unwrap();
        style.set_text_content(Some(&format!(
            "{}{}",
            include_str!("../../static/style.css"),
            include_str!("../../static/global.css"),
        )));
        doc.head().unwrap().append_child(&style).unwrap();
        let original = current_url();
        set_url("/help");
        let mount = mount_help().await;
        let root = doc.document_element().unwrap();
        let smooth_while_open = gloo_utils::window()
            .get_computed_style(&root)
            .unwrap()
            .unwrap()
            .get_property_value("scroll-behavior")
            .unwrap()
            == "smooth";
        js_sys::eval(
            "window.__help3351Scrolls = []; window.__help3351ScrollTo = window.scrollTo; \
             window.scrollTo = function (...args) { \
               window.__help3351Scrolls.push(getComputedStyle(document.documentElement).scrollBehavior); \
               return window.__help3351ScrollTo.apply(this, args); \
             };",
        )
        .unwrap();
        mount
            .query_selector("[data-testid='help-back-home']")
            .unwrap()
            .unwrap()
            .unchecked_ref::<web_sys::HtmlElement>()
            .click();
        let navigated = wait_until(|| current_url() == "/").await;
        let scrolls = js_sys::eval(
            "const s = window.__help3351Scrolls.join(','); \
             window.scrollTo = window.__help3351ScrollTo; \
             delete window.__help3351ScrollTo; delete window.__help3351Scrolls; s",
        )
        .unwrap()
        .as_string()
        .unwrap_or_default();
        mount.remove();
        style.remove();
        set_url(&original);

        assert!(smooth_while_open, "premise: the page scrolls smoothly");
        assert!(navigated, "Back to home still goes home");
        assert_eq!(
            scrolls, "auto",
            "the router's jump to the top must not animate over the next page"
        );
    }

    #[wasm_bindgen_test]
    async fn back_onto_a_question_link_shows_it_after_the_router_restores_the_scroll() {
        ARRIVAL.with(|slot| slot.borrow_mut().take());
        let window = gloo_utils::window();
        let original = current_url();
        set_url("/help");
        window.scroll_to_with_x_and_y(0.0, 0.0);
        let mount = mount_help().await;
        let viewport = window.inner_height().unwrap().as_f64().unwrap();
        let question_top = || {
            mount
                .query_selector("#faq-browsers")
                .unwrap()
                .map(|q| q.get_bounding_client_rect().top())
        };
        let starts_below = question_top().is_some_and(|top| top > viewport);
        set_url("/help#faq-browsers");
        mount
            .query_selector("#help-test-leave")
            .unwrap()
            .unwrap()
            .unchecked_ref::<web_sys::HtmlElement>()
            .click();
        let left = wait_until(|| {
            mount
                .query_selector("[data-testid='help-page']")
                .unwrap()
                .is_none()
        })
        .await;
        js_sys::eval(
            "window.__help3351Frames = []; window.__help3351Raf = window.requestAnimationFrame; \
             window.requestAnimationFrame = (cb) => window.__help3351Frames.push(cb);",
        )
        .unwrap();
        window.history().unwrap().back().unwrap();
        let remounted = wait_until(|| {
            mount
                .query_selector("[data-testid='help-page']")
                .unwrap()
                .is_some()
        })
        .await;
        TimeoutFuture::new(100).await;
        let held_frames = js_sys::eval(
            "window.requestAnimationFrame = window.__help3351Raf; \
             const frames = window.__help3351Frames; \
             delete window.__help3351Raf; delete window.__help3351Frames; \
             frames.forEach((cb) => cb(performance.now())); frames.length",
        )
        .unwrap()
        .as_f64()
        .unwrap_or_default();
        let focused = wait_until(|| {
            gloo_utils::document()
                .active_element()
                .zip(mount.query_selector("#faq-browsers > summary").unwrap())
                .is_some_and(|(active, summary)| active.is_same_node(Some(&summary)))
        })
        .await;
        TimeoutFuture::new(100).await;
        let top = question_top();
        let url = current_url();
        mount.remove();
        set_url(&original);
        window.scroll_to_with_x_and_y(0.0, 0.0);

        assert!(starts_below, "premise: the question starts below the fold");
        assert!(left, "premise: the page unmounted");
        assert!(remounted, "premise: Back mounted the page again");
        assert!(
            held_frames >= 1.0,
            "premise: the router's scroll restore was still waiting for a frame"
        );
        assert_eq!(
            url, "/help#faq-browsers",
            "premise: Back returned to the link"
        );
        assert!(focused, "the question takes focus");
        assert!(
            top.is_some_and(|t| (0.0..viewport).contains(&t)),
            "the question is in view once the router has restored the scroll, at {top:?}"
        );
    }

    #[wasm_bindgen_test]
    async fn coming_back_to_a_question_link_opens_the_question_again() {
        ARRIVAL.with(|slot| slot.borrow_mut().take());
        let original = current_url();
        set_url("/help");
        let mount = mount_help().await;
        let click = |selector: &str| {
            mount
                .query_selector(selector)
                .unwrap()
                .unwrap()
                .unchecked_ref::<web_sys::HtmlElement>()
                .click();
        };
        click("#help-test-hide");
        let left = wait_until(|| {
            mount
                .query_selector("[data-testid='help-page']")
                .unwrap()
                .is_none()
        })
        .await;
        set_url("/help#faq-browsers");
        click("#help-test-show");
        let opened = wait_until(|| {
            mount
                .query_selector("#faq-browsers[open]")
                .unwrap()
                .is_some()
        })
        .await;
        let focused_summary = wait_until(|| {
            gloo_utils::document()
                .active_element()
                .zip(mount.query_selector("#faq-browsers > summary").unwrap())
                .is_some_and(|(active, summary)| active.is_same_node(Some(&summary)))
        })
        .await;
        mount.remove();
        set_url(&original);
        gloo_utils::window().scroll_to_with_x_and_y(0.0, 0.0);

        assert!(left, "premise: the page unmounted");
        assert!(opened, "the question in the address bar opens");
        assert!(focused_summary, "and takes focus");
    }

    fn install_app_css() -> web_sys::Element {
        let doc = gloo_utils::document();
        let style = doc.create_element("style").unwrap();
        style.set_text_content(Some(&format!(
            "{}{}{}",
            include_str!("../../static/tailwind.css"),
            include_str!("../../static/style.css"),
            include_str!("../../static/global.css"),
        )));
        doc.head().unwrap().append_child(&style).unwrap();
        style
    }

    fn computed(el: &web_sys::Element, property: &str) -> String {
        gloo_utils::window()
            .get_computed_style(el)
            .unwrap()
            .unwrap()
            .get_property_value(property)
            .unwrap()
    }

    #[wasm_bindgen_test]
    async fn steps_are_numbered_and_lists_are_bulleted_despite_the_css_reset() {
        let style = install_app_css();
        let mount = mount_help().await;
        let marker_types = |selector: &str| -> Vec<String> {
            all(&mount, selector)
                .iter()
                .map(|list| computed(list, "list-style-type"))
                .collect()
        };
        let steps = marker_types(".help-steps");
        let lists = marker_types(".help-list");
        let items: Vec<String> = all(&mount, ".help-steps > li, .help-list > li")
            .iter()
            .map(|li| computed(li, "display"))
            .collect();
        mount.remove();
        style.remove();

        assert!(
            !steps.is_empty() && !lists.is_empty(),
            "premise: the page has both kinds of list"
        );
        assert!(steps.iter().all(|t| t == "decimal"), "steps: {steps:?}");
        assert!(lists.iter().all(|t| t == "disc"), "lists: {lists:?}");
        assert!(
            items.iter().all(|d| d == "list-item"),
            "every item keeps its marker box: {items:?}"
        );
    }

    #[wasm_bindgen_test]
    async fn the_intro_sits_on_the_sections_surface_in_both_themes() {
        let style = install_app_css();
        let root = gloo_utils::document().document_element().unwrap();
        let theme_before = root.get_attribute("data-theme");
        let mount = mount_help().await;
        let background = |selector: &str| {
            computed(
                &mount.query_selector(selector).unwrap().unwrap(),
                "background-color",
            )
        };
        let mut seen = Vec::new();
        for theme in ["dark", "light"] {
            root.set_attribute("data-theme", theme).unwrap();
            seen.push((
                theme,
                background(".help-intro"),
                background(".help-section"),
                background(".help-top-link"),
            ));
        }
        mount.remove();
        style.remove();
        match theme_before {
            Some(theme) => root.set_attribute("data-theme", &theme).unwrap(),
            None => root.remove_attribute("data-theme").unwrap(),
        }

        for (theme, intro, section, bare) in seen {
            assert_ne!(
                section, bare,
                "premise: in {theme}, sections have a surface"
            );
            assert_eq!(
                intro, section,
                "in {theme}, the intro reads on the sections' surface, not on the image"
            );
        }
    }

    #[wasm_bindgen_test]
    async fn the_page_makes_no_network_or_media_requests() {
        js_sys::eval(
            "window.__help3351 = { calls: [], fetch: window.fetch, gum: null }; \
             window.fetch = function (input) { \
               window.__help3351.calls.push('fetch ' + (typeof input === 'string' ? input : input.url)); \
               return window.__help3351.fetch.apply(this, arguments); }; \
             if (navigator.mediaDevices) { \
               window.__help3351.gum = navigator.mediaDevices.getUserMedia; \
               navigator.mediaDevices.getUserMedia = function () { \
                 window.__help3351.calls.push('getUserMedia'); \
                 return window.__help3351.gum.apply(navigator.mediaDevices, arguments); }; }",
        )
        .unwrap();
        let mount = mount_help().await;
        TimeoutFuture::new(300).await;
        let calls = js_sys::eval(
            "var h = window.__help3351; window.fetch = h.fetch; \
             if (h.gum) { navigator.mediaDevices.getUserMedia = h.gum; } \
             JSON.stringify(h.calls)",
        )
        .unwrap()
        .as_string()
        .unwrap();
        mount.remove();

        assert_eq!(calls, "[]");
    }

    #[wasm_bindgen_test]
    async fn faq_answers_open_and_close_from_their_summary() {
        let mount = mount_help().await;
        let items = all(&mount, "#faq details.help-faq-item");
        let first = items.first().cloned().expect("the FAQ has questions");
        let summary = first
            .query_selector("summary")
            .unwrap()
            .unwrap()
            .dyn_into::<web_sys::HtmlElement>()
            .unwrap();
        let closed_at_start = items.iter().all(|d| !d.has_attribute("open"));
        summary.click();
        let opened = first.has_attribute("open");
        summary.click();
        let closed_again = !first.has_attribute("open");
        mount.remove();

        assert!(items.len() >= 8, "only {} questions", items.len());
        assert!(closed_at_start);
        assert!(opened, "selecting the question opens its answer");
        assert!(closed_again, "selecting it again closes the answer");
    }
}
