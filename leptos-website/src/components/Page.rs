use crate::components::Footer::*;
// use crate::components::Header::*;
use leptos::*;

#[component]
pub fn Page(children: Children) -> impl IntoView {
    view! { <div class="overflow-x-hidden bg-white dark:bg-black">{children()} <Footer/></div> }
}
