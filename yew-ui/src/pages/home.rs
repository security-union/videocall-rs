use yew::prelude::*;
use yew_router::prelude::*;

use crate::Route;

#[function_component(Home)]
pub fn home () -> Html {
    let navigator = use_navigator().unwrap();

    let onsubmit = Callback::from(move |_| navigator.push(&Route::Meeting {
        id: "123".to_string(),
        email: "gg@gg.com".to_string(),
    }));
    html! {
        <div class="home-page">
            <div>
                <h1>{ "Welcome on zoom-rs !" }</h1>
                <p>{ "This is a web app to manage your zoom meetings." }</p>
            </div>
            <form {onsubmit}>
                <input type="text" placeholder="Username" />
                <input type="text" placeholder="Meeting ID" />
                <input type="submit" value="Join" />
            </form>
        </div>
    }
}
