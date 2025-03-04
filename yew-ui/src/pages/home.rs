use web_sys::HtmlInputElement;
use yew::prelude::*;
use yew_router::prelude::*;

use crate::Route;

const TEXT_INPUT_CLASSES: &str = "bg-background-light/20 backdrop-filter-blur text-white border border-white/10 outline-none focus:ring-2 focus:ring-primary rounded-xl p-4 w-full placeholder:text-foreground-subtle transition-all duration-300 hover:border-white/20";

#[function_component(Home)]
pub fn home() -> Html {
    let navigator = use_navigator().unwrap();

    let username_ref = use_node_ref();
    let meeting_id_ref = use_node_ref();
    
    // Tab state for features section
    let active_tab = use_state(|| 0);

    let onsubmit = {
        let username_ref = username_ref.clone();
        let meeting_id_ref = meeting_id_ref.clone();
        Callback::from(move |e: SubmitEvent| {
            e.prevent_default();
            let username = username_ref.cast::<HtmlInputElement>().unwrap().value();
            let meeting_id = meeting_id_ref.cast::<HtmlInputElement>().unwrap().value();
            navigator.push(&Route::Meeting {
                id: meeting_id,
                email: username,
            })
        })
    };
    
    let open_github = Callback::from(|_| {
        let window = web_sys::window().expect("no global window exists");
        let _ = window.open_with_url("https://github.com/darioalessandro/videocall-rs");
    });
    
    let set_active_tab = {
        let active_tab = active_tab.clone();
        Callback::from(move |tab: usize| {
            active_tab.set(tab);
        })
    };
    
    html! {
        <div class="hero-container">
            <div class="floating-element floating-element-1"></div>
            <div class="floating-element floating-element-2"></div>
            <div class="floating-element floating-element-3"></div>
            
            // GitHub corner ribbon
            <a href="https://github.com/darioalessandro/videocall-rs" class="github-corner" aria-label="View source on GitHub">
                <svg width="80" height="80" viewBox="0 0 250 250" style="fill:#7928CA; color:#0D131F; position: absolute; top: 0; border: 0; right: 0;" aria-hidden="true">
                    <path d="M0,0 L115,115 L130,115 L142,142 L250,250 L250,0 Z"></path>
                    <path d="M128.3,109.0 C113.8,99.7 119.0,89.6 119.0,89.6 C122.0,82.7 120.5,78.6 120.5,78.6 C119.2,72.0 123.4,76.3 123.4,76.3 C127.3,80.9 125.5,87.3 125.5,87.3 C122.9,97.6 130.6,101.9 134.4,103.2" fill="currentColor" style="transform-origin: 130px 106px;" class="octo-arm"></path>
                    <path d="M115.0,115.0 C114.9,115.1 118.7,116.5 119.8,115.4 L133.7,101.6 C136.9,99.2 139.9,98.4 142.2,98.6 C133.8,88.0 127.5,74.4 143.8,58.0 C148.5,53.4 154.0,51.2 159.7,51.0 C160.3,49.4 163.2,43.6 171.4,40.1 C171.4,40.1 176.1,42.5 178.8,56.2 C183.1,58.6 187.2,61.8 190.9,65.4 C194.5,69.0 197.7,73.2 200.1,77.6 C213.8,80.2 216.3,84.9 216.3,84.9 C212.7,93.1 206.9,96.0 205.4,96.6 C205.1,102.4 203.0,107.8 198.3,112.5 C181.9,128.9 168.3,122.5 157.7,114.1 C157.9,116.9 156.7,120.9 152.7,124.9 L141.0,136.5 C139.8,137.7 141.6,141.9 141.8,141.8 Z" fill="currentColor" class="octo-body"></path>
                </svg>
            </a>
            
            <div class="hero-content">
                <h1 class="hero-title text-center">{ "videocall.rs" }</h1>
                <h2 class="hero-subtitle text-center text-xl mb-3">{ "Video calls with anyone, anywhere" }</h2>
                
                // Tech stack badges
                <div class="flex justify-center gap-2 mb-4">
                    <div class="tech-badge">{"Rust"}</div>
                    <div class="tech-badge">{"WebRTC"}</div>
                    <div class="tech-badge">{"Yew"}</div>
                    <div class="tech-badge">{"E2EE"}</div>
                </div>
                
                // Features section
                <div class="features-container mb-6">
                    <div class="features-tabs">
                        <button 
                            class={if *active_tab == 0 { "feature-tab active" } else { "feature-tab" }}
                            onclick={let cb = set_active_tab.clone(); Callback::from(move |_| cb.emit(0))}
                        >
                            {"Secure"}
                        </button>
                        <button 
                            class={if *active_tab == 1 { "feature-tab active" } else { "feature-tab" }}
                            onclick={let cb = set_active_tab.clone(); Callback::from(move |_| cb.emit(1))}
                        >
                            {"Fast"}
                        </button>
                        <button 
                            class={if *active_tab == 2 { "feature-tab active" } else { "feature-tab" }}
                            onclick={let cb = set_active_tab.clone(); Callback::from(move |_| cb.emit(2))}
                        >
                            {"Open Source"}
                        </button>
                    </div>
                    <div class="feature-content">
                        {match *active_tab {
                            0 => html! {
                                <>
                                    <h3 class="feature-title">{"End-to-End Encryption"}</h3>
                                    <p class="feature-description">{"Built with strong cryptography using modern Rust libraries for secure, trustless communication. All data remains encrypted in transit and no keys are stored on servers."}</p>
                                </>
                            },
                            1 => html! {
                                <>
                                    <h3 class="feature-title">{"High Performance"}</h3>
                                    <p class="feature-description">{"Leveraging Rust's zero-cost abstractions and WebAssembly for maximum efficiency. Optimized WebRTC implementation with low latency for smooth video calls."}</p>
                                </>
                            },
                            2 => html! {
                                <>
                                    <h3 class="feature-title">{"100% Open Source"}</h3>
                                    <p class="feature-description">{"Fully transparent codebase under permissive licensing. Active community of contributors. Audit the code yourself - no black boxes or proprietary elements."}</p>
                                </>
                            },
                            _ => html! {}
                        }}
                    </div>
                </div>
                
                <form {onsubmit} class="w-full">
                    <div class="space-y-6">
                        <div>
                            <label for="username" class="block text-white/80 text-sm font-medium mb-2 ml-1">{"Username"}</label>
                            <input
                                id="username"
                                class={TEXT_INPUT_CLASSES}
                                type="text"
                                placeholder="Enter your name"
                                ref={username_ref}
                                required={true}
                                pattern="^[a-zA-Z0-9_]*$"
                            />
                        </div>
                        
                        <div>
                            <label for="meeting-id" class="block text-white/80 text-sm font-medium mb-2 ml-1">{"Meeting ID"}</label>
                            <input
                                id="meeting-id"
                                class={TEXT_INPUT_CLASSES}
                                type="text"
                                placeholder="Enter meeting code"
                                ref={meeting_id_ref}
                                required={true}
                                pattern="^[a-zA-Z0-9_]*$"
                            />
                            <p class="text-sm text-foreground-subtle mt-2 ml-1">{ "Characters allowed: a-z, A-Z, 0-9, and _" }</p>
                        </div>
                        
                        <div class="mt-8">
                            <button type="submit" class="cta-button w-full flex items-center justify-center gap-2">
                                <span class="text-lg">{ "Join Meeting" }</span>
                                <svg xmlns="http://www.w3.org/2000/svg" width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                    <path d="M5 12h14"></path>
                                    <path d="m12 5 7 7-7 7"></path>
                                </svg>
                                <span class="cta-glow"></span>
                            </button>
                        </div>
                    </div>
                </form>
                
                <div class="mt-8 text-center">
                    <p class="text-white/60 text-sm mb-4">{"Start a secure, end-to-end encrypted video meeting"}</p>
                    
                    // Code snippet
                    <div class="code-snippet mb-4">
                        <pre><code>{r#"git clone https://github.com/darioalessandro/videocall-rs
cd videocall-rs
cargo run"#}</code></pre>
                    </div>
                    
                    // Developer call-to-action
                    <button 
                        onclick={open_github}
                        class="secondary-button flex items-center justify-center mx-auto gap-2"
                    >
                        <svg xmlns="http://www.w3.org/2000/svg" width="18" height="18" viewBox="0 0 24 24" fill="currentColor">
                            <path d="M12 0c-6.626 0-12 5.373-12 12 0 5.302 3.438 9.8 8.207 11.387.599.111.793-.261.793-.577v-2.234c-3.338.726-4.033-1.416-4.033-1.416-.546-1.387-1.333-1.756-1.333-1.756-1.089-.745.083-.729.083-.729 1.205.084 1.839 1.237 1.839 1.237 1.07 1.834 2.807 1.304 3.492.997.107-.775.418-1.305.762-1.604-2.665-.305-5.467-1.334-5.467-5.931 0-1.311.469-2.381 1.236-3.221-.124-.303-.535-1.524.117-3.176 0 0 1.008-.322 3.301 1.23.957-.266 1.983-.399 3.003-.404 1.02.005 2.047.138 3.006.404 2.291-1.552 3.297-1.23 3.297-1.23.653 1.653.242 2.874.118 3.176.77.84 1.235 1.911 1.235 3.221 0 4.609-2.807 5.624-5.479 5.921.43.372.823 1.102.823 2.222v3.293c0 .319.192.694.801.576 4.765-1.589 8.199-6.086 8.199-11.386 0-6.627-5.373-12-12-12z" />
                        </svg>
                        <span>{"Contribute on GitHub"}</span>
                    </button>
                </div>
            </div>
        </div>
    }
}
