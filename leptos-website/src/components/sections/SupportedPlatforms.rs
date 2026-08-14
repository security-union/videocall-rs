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

use crate::components::CTAButton::{ButtonSize, ButtonVariant, CTAButton};
use crate::components::Reveal::RevealOnView;
use leptos::prelude::*;
// removed: use leptos::html::Div;
#[cfg(feature = "hydrate")]
use std::cell::{Cell, RefCell};
#[cfg(feature = "hydrate")]
use std::rc::Rc;
#[cfg(feature = "hydrate")]
use wasm_bindgen::closure::Closure;
#[cfg(feature = "hydrate")]
use wasm_bindgen::JsCast;
#[cfg(feature = "hydrate")]
use web_sys::{window, HtmlElement};

#[component]
pub fn SupportedPlatformsSection() -> impl IntoView {
    view! {
        <section id="supported-platforms" aria-labelledby="platforms-title" class="px-6 md:px-10 py-24 md:py-32">
            <div class="max-w-content mx-auto">
                <RevealOnView class="">
                    <p class="section-index" aria-hidden="true">"03 — Platforms"</p>
                    <h2 id="platforms-title" class="text-h2 text-fg mt-4">"Runs where your hardware runs"</h2>
                    <p class="text-body-lg text-fg-2 mt-4 max-w-2xl">
                        "From a browser tab to an embedded Linux board. Chromium and Safari on the desktop and on iOS, native mobile SDKs, and headless capture on a Raspberry Pi or Jetson."
                    </p>
                </RevealOnView>

                <div class="mt-12">
                    <PlatformsCarousel/>
                </div>

                <div class="flex flex-col sm:flex-row gap-3 mt-4">
                    <CTAButton
                        variant=ButtonVariant::Secondary
                        size=ButtonSize::Medium
                        href=Some("https://app.videocall.rs".to_string())
                    >
                        "Open in browser"
                    </CTAButton>
                    <CTAButton
                        variant=ButtonVariant::Tertiary
                        size=ButtonSize::Medium
                        href=Some("https://crates.io/crates/videocall-cli".to_string())
                    >
                        "Install videocall-cli"
                    </CTAButton>
                </div>
            </div>
        </section>
    }
}

#[island]
fn PlatformsCarousel() -> impl IntoView {
    // Use Wikimedia thumbnail endpoints (PNG) for reliability and CORS-friendliness
    #[derive(Clone, Copy)]
    struct PlatformItem {
        name: &'static str,
        src: &'static str,
    }

    const ITEMS: [PlatformItem; 10] = [
        PlatformItem {
            name: "Chrome",
            src: "/images/platforms/chrome.svg",
        },
        PlatformItem {
            name: "Safari",
            src: "/images/platforms/safari.svg",
        },
        PlatformItem {
            name: "Brave",
            src: "/images/platforms/brave.svg",
        },
        PlatformItem {
            name: "Edge",
            src: "/images/platforms/edge.svg",
        },
        PlatformItem {
            name: "Raspberry Pi",
            src: "/images/platforms/raspberry-pi.svg",
        },
        PlatformItem {
            name: "Linux",
            src: "/images/platforms/linux.svg",
        },
        PlatformItem {
            name: "Chromium",
            src: "/images/platforms/chromium.svg",
        },
        PlatformItem {
            name: "Mac OS",
            src: "/images/platforms/apple.svg",
        },
        PlatformItem {
            name: "iOS",
            src: "/images/platforms/ios.svg",
        },
        PlatformItem {
            name: "Android",
            src: "/images/platforms/android.svg",
        },
    ];

    const TRACK_ID: &str = "platforms-track";

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            let Some(win) = window() else { return };
            let Some(doc) = win.document() else { return };
            let Some(el) = doc.get_element_by_id(TRACK_ID) else {
                return;
            };
            let Ok(track_el) = el.dyn_into::<HtmlElement>() else {
                return;
            };
            if let Ok(Some(mql)) = win.match_media("(prefers-reduced-motion: reduce)") {
                if mql.matches() {
                    return;
                }
            }

            let speed_px_per_s: f64 = 90.0;
            let gap_px: f64 = 16.0; // matches gap-4

            // Duplicate children once so the track is seamless. Clones are
            // decorative repeats, so hide them from the accessibility tree.
            let children: Vec<_> = (0..track_el.children().length())
                .filter_map(|i| track_el.children().item(i))
                .collect::<Vec<_>>();
            for child in children.iter() {
                if let Ok(clone) = child.clone_node_with_deep(true) {
                    if let Some(clone_el) = clone.dyn_ref::<web_sys::Element>() {
                        let _ = clone_el.set_attribute("aria-hidden", "true");
                    }
                    let _ = track_el.append_child(&clone);
                }
            }

            // Tiles are equal width per breakpoint, so the advance distance is
            // measured once (and again on resize) rather than every frame — no
            // per-frame layout flush from reading `offset_width`.
            let child_w = Rc::new(Cell::new(0.0_f64));
            let measure: Rc<dyn Fn()> = {
                let track_el = track_el.clone();
                let child_w = child_w.clone();
                Rc::new(move || {
                    if let Some(first) = track_el.first_element_child() {
                        if let Ok(fe) = first.dyn_into::<HtmlElement>() {
                            child_w.set(fe.offset_width() as f64 + gap_px);
                        }
                    }
                })
            };
            measure();

            let prev_time = Rc::new(RefCell::new(None::<f64>));
            let offset = Rc::new(RefCell::new(0.0_f64));
            let on_screen = Rc::new(Cell::new(true));
            let scheduled = Rc::new(Cell::new(false));
            type RafClosure = Closure<dyn FnMut(f64)>;
            let raf: Rc<RefCell<Option<RafClosure>>> = Rc::new(RefCell::new(None));

            // The ticker only runs while the marquee is on-screen and the tab is
            // visible; otherwise no frame is requested at all.
            let should_run: Rc<dyn Fn() -> bool> = {
                let doc = doc.clone();
                let on_screen = on_screen.clone();
                Rc::new(move || on_screen.get() && !doc.hidden())
            };
            let schedule: Rc<dyn Fn()> = {
                let win = win.clone();
                let raf = raf.clone();
                let scheduled = scheduled.clone();
                let should_run = should_run.clone();
                Rc::new(move || {
                    if scheduled.get() || !should_run() {
                        return;
                    }
                    if let Some(cb) = raf.borrow().as_ref() {
                        scheduled.set(true);
                        let _ = win.request_animation_frame(cb.as_ref().unchecked_ref());
                    }
                })
            };

            {
                let prev_time = prev_time.clone();
                let offset = offset.clone();
                let child_w = child_w.clone();
                let track_el = track_el.clone();
                let scheduled = scheduled.clone();
                let should_run = should_run.clone();
                let schedule = schedule.clone();
                *raf.borrow_mut() = Some(Closure::wrap(Box::new(move |t: f64| {
                    scheduled.set(false);
                    if !should_run() {
                        // Reset the clock so resuming doesn't jump by the paused gap.
                        *prev_time.borrow_mut() = None;
                        return;
                    }
                    let dt = {
                        let mut p = prev_time.borrow_mut();
                        let dt = p.map(|prev| (t - prev) / 1000.0).unwrap_or(0.0);
                        *p = Some(t);
                        dt
                    };
                    let mut off = offset.borrow_mut();
                    *off += speed_px_per_s * dt;
                    let w = child_w.get();
                    while w > 0.0 && *off > w {
                        if let Some(first) = track_el.first_element_child() {
                            let _ = track_el.append_child(&first);
                            *off -= w;
                        } else {
                            break;
                        }
                    }
                    let _ = track_el
                        .style()
                        .set_property("transform", &format!("translateX(-{}px)", *off));
                    drop(off);
                    schedule();
                }) as Box<dyn FnMut(f64)>));
            }

            // Pause/resume when the marquee scrolls off-screen.
            let io_cb = Closure::wrap(Box::new({
                let on_screen = on_screen.clone();
                let prev_time = prev_time.clone();
                let schedule = schedule.clone();
                move |entries: js_sys::Array, _obs: web_sys::IntersectionObserver| {
                    let mut visible = false;
                    for entry in entries.iter() {
                        let entry: web_sys::IntersectionObserverEntry = entry.unchecked_into();
                        if entry.is_intersecting() {
                            visible = true;
                        }
                    }
                    on_screen.set(visible);
                    if visible {
                        *prev_time.borrow_mut() = None;
                        schedule();
                    }
                }
            })
                as Box<dyn FnMut(js_sys::Array, web_sys::IntersectionObserver)>);
            if let Ok(io) = web_sys::IntersectionObserver::new(io_cb.as_ref().unchecked_ref()) {
                io.observe(&track_el);
                // Page-lifetime observer for a page-lifetime marquee.
                io_cb.forget();
                std::mem::forget(io);
            }

            // Pause/resume when the tab is hidden.
            let vis_cb = Closure::wrap(Box::new({
                let doc = doc.clone();
                let prev_time = prev_time.clone();
                let schedule = schedule.clone();
                move || {
                    if !doc.hidden() {
                        *prev_time.borrow_mut() = None;
                        schedule();
                    }
                }
            }) as Box<dyn FnMut()>);
            let _ = doc.add_event_listener_with_callback(
                "visibilitychange",
                vis_cb.as_ref().unchecked_ref(),
            );
            vis_cb.forget();

            // Re-measure tile width when the breakpoint may have changed.
            let resize_cb = Closure::wrap(Box::new({
                let measure = measure.clone();
                move || measure()
            }) as Box<dyn FnMut()>);
            let _ =
                win.add_event_listener_with_callback("resize", resize_cb.as_ref().unchecked_ref());
            resize_cb.forget();

            schedule();
        });
    }

    view! {
        <div class="relative">
            <div class="overflow-hidden mask-edge-fade">
                <div id=TRACK_ID class="flex gap-4 will-change-transform">
                    {move || {
                        ITEMS
                            .iter()
                            .map(|item| view! {
                                <div class="group flex-shrink-0 w-28 md:w-36 h-14 flex items-center justify-center">
                                    <img
                                        src=item.src
                                        alt=item.name
                                        class="h-9 md:h-10 w-auto grayscale opacity-70 group-hover:opacity-100 transition-opacity duration-200"
                                        loading="lazy"
                                    />
                                </div>
                            })
                            .collect_view()
                    }}
                </div>
            </div>
        </div>
    }
}
