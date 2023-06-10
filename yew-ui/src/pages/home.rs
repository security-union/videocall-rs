use yew::prelude::*;
use yew_router::prelude::*;
use web_sys::HtmlInputElement;

use crate::Route;

#[function_component(Home)]
pub fn home () -> Html {
    let navigator = use_navigator().unwrap();

    let username_ref = use_node_ref();
    let meeting_id_ref = use_node_ref();

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
    html! {
        <div class="home-page">
            <div>
                <h1>{ "Welcome on zoom-rs !" }</h1>
                <p>{ "This is a web app to manage your zoom meetings." }</p>
            </div>
            <form {onsubmit}>
                <input label="username" type="text" placeholder="Username" ref={username_ref}  />
                <input label="meeting_id" type="text" placeholder="Meeting ID" ref={meeting_id_ref} />
                <input type="submit" value="Join" />
            </form>
        </div>
    }
}
