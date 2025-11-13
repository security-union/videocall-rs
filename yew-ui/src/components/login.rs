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

use gloo_utils::window;
use yew::prelude::*;

use crate::constants::login_url;

#[function_component(Login)]
pub fn login() -> Html {
    let login = Callback::from(|_: MouseEvent| match login_url() {
        Ok(mut url) => {
            // Check if there's a returnTo parameter in the current URL
            if let Some(win) = window().location().search().ok() {
                if !win.is_empty() {
                    // Append the query parameters from the current URL to the backend login URL
                    url = format!("{}{}", url, win);
                }
            }
            let _ = window().location().set_href(&url);
        }
        Err(e) => log::error!("Failed to get login URL: {e:?}"),
    });

    html! {
        <div style="position: fixed; top: 0; left: 0; width: 100vw; height: 100vh; display: flex; flex-direction: column; align-items: center; justify-content: center; background: #000000; -webkit-font-smoothing: antialiased; -moz-osx-font-smoothing: grayscale;">
            <div class="flex flex-col items-center px-6 py-12">
                // Logo/Brand - Large, sleek, Apple-style
                <div style="margin-bottom: 4rem;">
                    <h1 style="font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; font-size: 5.5rem; font-weight: 300; letter-spacing: -0.03em; color: #ffffff; margin: 0;">{"videocall.rs"}</h1>
                </div>

                // Sign in box
                <div class="flex flex-col items-center">

                    // Google Sign-in button (image)
                    <button
                        onclick={login}
                        class="transition-transform hover:scale-[1.02] active:scale-[0.98]"
                        style="background: none; border: none; padding: 0; cursor: pointer;"
                    >
                        <img
                            src="/assets/btn_google.png"
                            alt="Sign in with Google"
                            class="h-[46px] w-auto"
                        />
                    </button>

                    <p style="margin-top: 2rem; text-align: center; font-size: 0.75rem; color: #86868b; font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;">
                        {"By signing in, you agree to our "}
                        <a href="https://github.com/security-union/videocall-rs" style="color: #0a84ff; text-decoration: none;" class="hover:underline">{"Terms of Service"}</a>
                        {" and "}
                        <a href="https://github.com/security-union/videocall-rs" style="color: #0a84ff; text-decoration: none;" class="hover:underline">{"Privacy Policy"}</a>
                    </p>
                </div>
            </div>
        </div>
    }
}
