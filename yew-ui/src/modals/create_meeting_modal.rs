use web_sys::HtmlInputElement;
use yew::prelude::*;

#[derive(Properties, PartialEq)]
pub struct CreateMeetingModalProps {
    pub is_open: bool,
    pub on_close: Callback<()>,
    pub on_create: Callback<Option<String>>,
}

#[function_component(CreateMeetingModal)]
pub fn create_meeting_modal(props: &CreateMeetingModalProps) -> Html {
    let password_ref = use_node_ref();

    let on_submit = {
        let password_ref = password_ref.clone();
        let on_create = props.on_create.clone();
        let on_close = props.on_close.clone();

        Callback::from(move |e: SubmitEvent| {
            e.prevent_default();

            let pwd = password_ref.cast::<HtmlInputElement>().unwrap().value();
            let password = if pwd.is_empty() {
                None
            } else {
                Some(pwd)
            };

            on_create.emit(password);
            on_close.emit(());
        })
    };

    let on_backdrop_click = {
        let on_close = props.on_close.clone();
        Callback::from(move |e: MouseEvent| {
            e.stop_propagation();
            on_close.emit(());
        })
    };

    let on_modal_click = Callback::from(|e: MouseEvent| {
        e.stop_propagation();
    });

    if !props.is_open {
        return html! {};
    }

    html! {
        <div onclick={on_backdrop_click} class="glass-backdrop">
            <div onclick={on_modal_click} class="card-apple" style="max-width: 500px; width: 90%;">
                <div style="display: flex; justify-content: space-between; align-items: center; margin-bottom: 1.5rem;">
                    <h3 style="margin: 0; font-size: 1.25rem; font-weight: 600;">{"Create New Meeting"}</h3>
                    <button type="button" onclick={props.on_close.reform(|_| ())} style="background: none; border: none; color: rgba(255,255,255,0.6); cursor: pointer; font-size: 1.75rem; line-height: 1; padding: 0; width: 28px; height: 28px; display: flex; align-items: center; justify-content: center;">{"×"}</button>
                </div>

                <form onsubmit={on_submit}>
                    <div style="margin-bottom: 1.5rem;">
                        <label for="meeting-password" style="display: block; margin-bottom: 0.5rem; font-size: 0.875rem; font-weight: 500; color: rgba(255,255,255,0.8);">
                            {"Password (optional)"}
                        </label>
                        <input
                            id="meeting-password"
                            class="input-apple"
                            type="password"
                            placeholder="Enter password for meeting"
                            ref={password_ref}
                            autofocus={true}
                        />
                        <p style="margin-top: 0.5rem; margin-bottom: 0; font-size: 0.75rem; color: rgba(255,255,255,0.5);">
                            {"Leave empty for no password protection"}
                        </p>
                    </div>

                    <div style="display: flex; gap: 0.75rem; margin-top: 1.5rem;">
                        <button
                            type="button"
                            class="btn-apple btn-secondary"
                            onclick={props.on_close.reform(|_| ())}
                            style="flex: 1;"
                        >
                            {"Cancel"}
                        </button>
                        <button
                            type="submit"
                            class="btn-apple btn-primary"
                            style="flex: 1;"
                        >
                            {"Start Meeting"}
                        </button>
                    </div>
                </form>
            </div>
        </div>
    }
}