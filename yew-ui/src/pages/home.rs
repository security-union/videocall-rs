use web_sys::HtmlInputElement;
use yew::prelude::*;
use yew_router::prelude::*;

use crate::Route;

const TEXT_INPUT_CLASSES: &'static str = "rounded-md mx-2 p-2 text-black required:ring-2 required:ring-red-500 required:valid:ring-2 required:valid:ring-green-500";

#[function_component(Home)]
pub fn home() -> Html {
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
        <div class="flex justify-center items-center content-center flex-col m-auto">
            <div class="flex items-center flex-col">
                <h1 class="text-xl">{ "videocall-rs" }</h1>
                <p class="text-xs">{ "Create your videocall by creating a user and meeting id." }</p>
                <p class="text-xs">{ "Characters allowed: a-z, A-Z, 0-9, and _" }</p>
            </div>
            <form {onsubmit}>
                <div class="py-4">
                    <input
                        class={TEXT_INPUT_CLASSES}
                        label="username"
                        type="text"
                        placeholder="Username"
                        ref={username_ref}
                        required={true}
                        pattern="^[a-zA-Z0-9_]*$"
                    />
                    <input
                        class={TEXT_INPUT_CLASSES}
                        label="meeting_id"
                        type="text"
                        placeholder="Meeting ID"
                        ref={meeting_id_ref}
                        required={true}
                        pattern="^[a-zA-Z0-9_]*$"
                    />
                </div>
                <input type="submit" value="JOIN" class="py-2 px-4 pointer bg-yew-blue rounded-md w-full cursor-pointer" />
            </form>
        </div>
    }
}
