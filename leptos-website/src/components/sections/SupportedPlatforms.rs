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
use leptos::*;
// removed: use leptos::html::Div;
#[cfg(feature = "hydrate")]
use std::cell::RefCell;
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
        <section id="supported-platforms" class="relative">
            <div class="text-center mb-12">
                <h2 class="text-4xl md:text-5xl font-semibold tracking-tight mb-4">"Supported Platforms"</h2>
                <p class="text-lg md:text-xl text-white/50 max-w-2xl mx-auto">
                    "Runs beautifully on modern browsers and embedded devices"
                </p>
            </div>

            <PlatformsCarousel/>

            <div class="flex flex-col sm:flex-row gap-4 justify-center items-center">
                <CTAButton
                    variant=ButtonVariant::Primary
                    size=ButtonSize::Medium
                    href=Some("https://app.videocall.rs".to_string())
                >
                    "Try meeting in a browser"
                </CTAButton>
                <CTAButton
                    variant=ButtonVariant::Secondary
                    size=ButtonSize::Medium
                    href=Some("https://crates.io/crates/videocall-cli".to_string())
                >
                    "Try videocall-cli"
                </CTAButton>
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
        create_effect(move |_| {
            if let Some(win) = window() {
                if let Some(doc) = win.document() {
                    if let Some(el) = doc.get_element_by_id(TRACK_ID) {
                        let Ok(track_el) = el.dyn_into::<HtmlElement>() else { return; };
                        if let Ok(Some(mql)) = win.match_media("(prefers-reduced-motion: reduce)") {
                            if mql.matches() {
                                return;
                            }
                        }

                        let speed_px_per_s: f64 = 90.0;
                        let gap_px: f64 = 16.0; // matches gap-4

                        // Ensure enough content by duplicating once at start
                        let children: Vec<_> = (0..track_el.children().length())
                            .filter_map(|i| track_el.children().item(i))
                            .collect::<Vec<_>>();
                        for child in children.iter() {
                            if let Ok(clone) = child.clone_node_with_deep(true) {
                                let _ = track_el.append_child(&clone);
                            }
                        }

                        let prev_time = Rc::new(RefCell::new(None::<f64>));
                        let offset = Rc::new(RefCell::new(0.0_f64));
                        let f: Rc<RefCell<Option<Closure<dyn FnMut(f64)>>>> = Rc::new(RefCell::new(None));
                        let g = f.clone();
                        let win_clone = win.clone();
                        let track_clone = track_el.clone();
                        *g.borrow_mut() = Some(Closure::wrap(Box::new(move |t: f64| {
                            let dt = {
                                let mut p = prev_time.borrow_mut();
                                let dt = if let Some(prev) = *p { (t - prev) / 1000.0 } else { 0.0 };
                                *p = Some(t);
                                dt
                            };

                            // Advance offset
                            {
                                let mut off = offset.borrow_mut();
                                *off += speed_px_per_s * dt;
                                let _ = track_clone
                                    .style()
                                    .set_property("transform", &format!("translateX(-{}px)", *off));

                                // Recycle children when they leave fully
                                loop {
                                    if let Some(first) = track_clone.first_element_child() {
                                        if let Ok(first_el) = first.dyn_into::<HtmlElement>() {
                                            let w = first_el.offset_width() as f64 + gap_px;
                                            if *off > w {
                                                let _ = track_clone.append_child(&first_el);
                                                *off -= w;
                                                let _ = track_clone
                                                    .style()
                                                    .set_property("transform", &format!("translateX(-{}px)", *off));
                                                continue;
                                            }
                                        }
                                    }
                                    break;
                                }
                            }

                            let _ = win_clone
                                .request_animation_frame(f.borrow().as_ref().unwrap().as_ref().unchecked_ref());
                        }) as Box<dyn FnMut(f64)>));

                        let _ = win
                            .request_animation_frame(g.borrow().as_ref().unwrap().as_ref().unchecked_ref());
                    }
                }
            }
        });
    }

    view! {
        <div class="relative mb-12">
            <div class="overflow-hidden mask-edge-fade">
                <div id=TRACK_ID class="flex gap-4 will-change-transform">
                    {move || {
                        ITEMS
                            .iter()
                            .map(|item| view! {
                                <div class="card-apple group flex-shrink-0 w-44 p-6 flex flex-col items-center justify-center">
                                    <div class="flex items-center justify-center w-full h-24">
                                        <img
                                            src=item.src
                                            alt=item.name
                                            class="h-16 w-auto opacity-90 grayscale group-hover:grayscale-0 group-hover:opacity-100 transition-all duration-300"
                                            loading="lazy"
                                        />
                                    </div>
                                    <div class="mt-4 text-sm text-foreground-secondary">{item.name}</div>
                                </div>
                            })
                            .collect_view()
                    }}
                </div>
            </div>
            <style>
                {".mask-edge-fade {{ -webkit-mask-image: linear-gradient(to right, rgba(0,0,0,0) 0, rgba(0,0,0,1) 48px, rgba(0,0,0,1) calc(100% - 48px), rgba(0,0,0,0) 100%); mask-image: linear-gradient(to right, rgba(0,0,0,0) 0, rgba(0,0,0,1) 48px, rgba(0,0,0,1) calc(100% - 48px), rgba(0,0,0,0) 100%); -webkit-mask-repeat: no-repeat; mask-repeat: no-repeat; -webkit-mask-size: 100% 100%; mask-size: 100% 100%; }}"}
            </style>
        </div>
    }
}
