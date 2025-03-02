use crate::components::Footer::*;
// use crate::components::Header::*;
use leptos::*;
use leptos_meta::Style;

#[component]
pub fn Page(children: Children) -> impl IntoView {
    view! {
        <Style>{include_str!("../global.css")}</Style>
        <div class="min-h-screen text-foreground bg-background overflow-x-hidden">
            <div class="w-full min-h-[70vh]">
                {children()}
            </div>
            <Footer/>
        </div>
    }
}
