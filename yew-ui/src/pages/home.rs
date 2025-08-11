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

use web_sys::HtmlInputElement;
use yew::prelude::*;
use yew_router::prelude::*;

use crate::components::browser_compatibility::BrowserCompatibility;
use crate::context::{
    is_valid_username, load_username_from_storage, save_username_to_storage, UsernameCtx,
};
use crate::Route;
use web_time::SystemTime;

#[function_component(Home)]
pub fn home() -> Html {
    html! {
      <div class="w-full max-w-sm">
        <div class="relative">
          <input class="peer w-full rounded-md border border-slate-300 bg-transparent px-3 pt-5 pb-2 text-slate-900 text-sm transition-all duration-200 focus:outline-none focus:border-blue-500" type="text" placeholder=" " />
          <label class="pointer-events-none absolute left-3 top-2 z-10 origin-left bg-white px-1 text-slate-500 text-sm transition-all duration-200 ease-in-out peer-focus:-translate-y-3 peer-focus:scale-90 peer-focus:text-blue-600 peer-not-placeholder-shown:-translate-y-3 peer-not-placeholder-shown:scale-90 peer-not-placeholder-shown:text-slate-600">
            {"Type Here..."}
          </label>
        </div>
      </div>
    }
}
