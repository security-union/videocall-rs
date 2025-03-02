use leptos::*;

#[component]
pub fn SecondaryButton(
    #[prop(into)] title: String,
    #[prop(default = String::new(), into)] class: String,
    #[prop(default = None)] href: Option<String>,
    #[prop(default = None)] style: Option<String>,
) -> impl IntoView {
    let base_class = "secondary-button";
    let combined_class = format!("{} {}", base_class, class);

    view! {
        {move || match &href {
            Some(href) => view! {
                <a href=href class=&combined_class style=style.clone()>
                    <span>{title.clone()}</span>
                </a>
            }.into_view(),
            None => view! {
                <button class=&combined_class style=style.clone()>
                    <span>{title.clone()}</span>
                </button>
            }.into_view()
        }}
    }
} 