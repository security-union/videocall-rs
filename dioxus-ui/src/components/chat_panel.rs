// SPDX-License-Identifier: MIT OR Apache-2.0

//! Chat panel sidebar component.
//!
//! Slides in from the left, mirroring the peer list container pattern.
//! Connects to an external chat service via [`GenericChatAdapter`] and
//! polls for new messages.

use dioxus::prelude::*;
use dioxus_core::Task;
use std::cell::RefCell;
use std::rc::Rc;

use crate::chat::{
    ChatConfig, ChatMessage, ChatServiceAdapter, ChatState, GenericChatAdapter, JmapChatAdapter,
};
use crate::constants::app_config;

/// Enum dispatch wrapper so we can select the adapter at runtime based on
/// the `chatProtocol` config value without needing trait objects.
enum ChatAdapterKind {
    Rest(GenericChatAdapter),
    Jmap(JmapChatAdapter),
}

impl ChatServiceAdapter for ChatAdapterKind {
    async fn authenticate(
        &mut self,
        user_id: &str,
        display_name: &str,
    ) -> Result<(), crate::chat::ChatError> {
        match self {
            Self::Rest(a) => a.authenticate(user_id, display_name).await,
            Self::Jmap(a) => a.authenticate(user_id, display_name).await,
        }
    }

    async fn join_room(
        &mut self,
        meeting_id: &str,
    ) -> Result<crate::chat::types::ChatRoom, crate::chat::ChatError> {
        match self {
            Self::Rest(a) => a.join_room(meeting_id).await,
            Self::Jmap(a) => a.join_room(meeting_id).await,
        }
    }

    async fn send_message(
        &self,
        room_id: &str,
        content: &str,
    ) -> Result<ChatMessage, crate::chat::ChatError> {
        match self {
            Self::Rest(a) => a.send_message(room_id, content).await,
            Self::Jmap(a) => a.send_message(room_id, content).await,
        }
    }

    async fn get_messages(
        &self,
        room_id: &str,
        since: Option<f64>,
    ) -> Result<Vec<ChatMessage>, crate::chat::ChatError> {
        match self {
            Self::Rest(a) => a.get_messages(room_id, since).await,
            Self::Jmap(a) => a.get_messages(room_id, since).await,
        }
    }

    async fn disconnect(&mut self) -> Result<(), crate::chat::ChatError> {
        match self {
            Self::Rest(a) => a.disconnect().await,
            Self::Jmap(a) => a.disconnect().await,
        }
    }
}

/// Helper: take the adapter out of the cell, call `get_messages`, put it back,
/// and return the result. This avoids holding the `RefCell` borrow across an
/// `.await` point (which would panic in debug builds).
async fn poll_messages(
    cell: &Rc<RefCell<Option<ChatAdapterKind>>>,
    room_id: &str,
    since: Option<f64>,
) -> Option<Result<Vec<ChatMessage>, crate::chat::ChatError>> {
    let adapter = cell.borrow_mut().take()?;
    let result = adapter.get_messages(room_id, since).await;
    *cell.borrow_mut() = Some(adapter);
    Some(result)
}

/// Helper: take the adapter out, call `send_message`, put it back.
async fn send_msg(
    cell: &Rc<RefCell<Option<ChatAdapterKind>>>,
    room_id: &str,
    content: &str,
) -> Option<Result<ChatMessage, crate::chat::ChatError>> {
    let adapter = cell.borrow_mut().take()?;
    let result = adapter.send_message(room_id, content).await;
    *cell.borrow_mut() = Some(adapter);
    Some(result)
}

/// Props for the ChatPanel component.
#[component]
pub fn ChatPanel(
    visible: bool,
    on_close: EventHandler<MouseEvent>,
    meeting_id: String,
    user_id: String,
    display_name: String,
    chat_state: ChatState,
) -> Element {
    let mut input_text = use_signal(String::new);
    let mut loading = use_signal(|| false);
    let mut sending = use_signal(|| false);

    // Shared adapter cell — created once when the panel mounts with a valid
    // meeting_id, then reused for send/poll operations.
    let adapter: Signal<Rc<RefCell<Option<ChatAdapterKind>>>> =
        use_signal(|| Rc::new(RefCell::new(None)));

    // Track the last message timestamp for incremental polling.
    let last_ts: Signal<Rc<RefCell<Option<f64>>>> = use_signal(|| Rc::new(RefCell::new(None)));

    // Track the poll task so we can cancel it on cleanup.
    let poll_task: Signal<Rc<RefCell<Option<Task>>>> = use_signal(|| Rc::new(RefCell::new(None)));

    // Initialise the adapter and start polling when the panel becomes visible.
    {
        let mut chat_state = chat_state;
        use_effect(move || {
            let visible = visible;
            let meeting_id = meeting_id.clone();
            let user_id = user_id.clone();
            let display_name = display_name.clone();
            let adapter_cell = (adapter)();
            let last_ts_cell = (last_ts)();
            let poll_task_cell = (poll_task)();

            if !visible || meeting_id.is_empty() {
                // Tear down: cancel poll, disconnect adapter.
                if let Some(task) = poll_task_cell.borrow_mut().take() {
                    task.cancel();
                }
                if let Some(mut taken) = adapter_cell.borrow_mut().take() {
                    wasm_bindgen_futures::spawn_local(async move {
                        let _ = taken.disconnect().await;
                    });
                }
                chat_state.is_connected.set(false);
                chat_state.current_room_id.set(None);
                return;
            }

            // Already initialised — don't re-init.
            if adapter_cell.borrow().is_some() {
                return;
            }

            loading.set(true);
            chat_state.error.set(None);

            let adapter_cell2 = adapter_cell.clone();
            let last_ts_cell2 = last_ts_cell.clone();
            let poll_task_cell2 = poll_task_cell.clone();

            wasm_bindgen_futures::spawn_local(async move {
                let cfg = match app_config()
                    .and_then(|c| ChatConfig::from_runtime_config(&c).map_err(|e| e.to_string()))
                {
                    Ok(cfg) => cfg,
                    Err(e) => {
                        chat_state.error.set(Some(e));
                        loading.set(false);
                        return;
                    }
                };

                let poll_interval_ms = cfg.poll_interval_ms;
                let mut new_adapter = if cfg.protocol == "jmap" {
                    ChatAdapterKind::Jmap(JmapChatAdapter::new(cfg))
                } else {
                    ChatAdapterKind::Rest(GenericChatAdapter::new(cfg))
                };

                // Authenticate
                if let Err(e) = new_adapter.authenticate(&user_id, &display_name).await {
                    chat_state.error.set(Some(e.to_string()));
                    loading.set(false);
                    return;
                }

                // Join room
                match new_adapter.join_room(&meeting_id).await {
                    Ok(room) => {
                        chat_state.current_room_id.set(Some(room.id.clone()));
                        chat_state.is_connected.set(true);

                        // Fetch initial messages
                        match new_adapter.get_messages(&room.id, None).await {
                            Ok(msgs) => {
                                if let Some(last) = msgs.last() {
                                    *last_ts_cell2.borrow_mut() = Some(last.timestamp);
                                }
                                chat_state.messages.set(msgs);
                            }
                            Err(e) => {
                                log::warn!("Chat: failed to fetch initial messages: {e}");
                            }
                        }

                        *adapter_cell2.borrow_mut() = Some(new_adapter);
                        loading.set(false);

                        // Start poll loop
                        let room_id = room.id;
                        let adapter_for_poll = adapter_cell.clone();
                        let last_ts_for_poll = last_ts_cell.clone();
                        let task = dioxus::prelude::spawn(async move {
                            loop {
                                gloo_timers::future::sleep(std::time::Duration::from_millis(
                                    poll_interval_ms.into(),
                                ))
                                .await;

                                let since = *last_ts_for_poll.borrow();
                                let result =
                                    poll_messages(&adapter_for_poll, &room_id, since).await;

                                match result {
                                    Some(Ok(new_msgs)) if !new_msgs.is_empty() => {
                                        if let Some(last) = new_msgs.last() {
                                            *last_ts_for_poll.borrow_mut() = Some(last.timestamp);
                                        }
                                        chat_state.messages.write().extend(new_msgs);
                                    }
                                    Some(Err(e)) => {
                                        log::warn!("Chat poll error: {e}");
                                    }
                                    _ => {}
                                }
                            }
                        });
                        *poll_task_cell2.borrow_mut() = Some(task);
                    }
                    Err(e) => {
                        chat_state.error.set(Some(e.to_string()));
                        loading.set(false);
                    }
                }
            });
        });
    }

    // Send message handler
    let on_send = {
        let adapter_cell = (adapter)();
        let mut chat_state = chat_state;
        move |_: Event<FormData>| {
            let text = input_text().trim().to_string();
            if text.is_empty() {
                return;
            }
            let room_id = match (chat_state.current_room_id)() {
                Some(id) => id,
                None => return,
            };
            input_text.set(String::new());
            sending.set(true);

            let adapter_cell = adapter_cell.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let result = send_msg(&adapter_cell, &room_id, &text).await;

                match result {
                    Some(Ok(msg)) => {
                        chat_state.messages.write().push(msg);
                    }
                    Some(Err(e)) => {
                        log::error!("Chat: failed to send message: {e}");
                        chat_state.error.set(Some(format!("Failed to send: {e}")));
                    }
                    None => {
                        log::error!("Chat: adapter not connected");
                    }
                }
                sending.set(false);
            });
        }
    };

    let panel_class = if visible { "visible" } else { "" };

    rsx! {
        div {
            id: "chat-panel-container",
            class: panel_class,

            div { class: "sidebar-header",
                h2 { "Chat" }
                button {
                    class: "close-button",
                    onclick: move |e| on_close.call(e),
                    "\u{00d7}"
                }
            }

            if loading() {
                div { class: "chat-messages",
                    div { class: "chat-loading",
                        div { class: "loading-spinner", style: "width: 24px; height: 24px;" }
                        p { "Connecting to chat..." }
                    }
                }
            } else if let Some(error) = (chat_state.error)() {
                div { class: "chat-messages",
                    div { class: "chat-error",
                        p { "Chat unavailable" }
                        p { class: "chat-error-detail", "{error}" }
                    }
                }
            } else {
                div { class: "chat-messages",
                    if (chat_state.messages)().is_empty() {
                        div { class: "chat-empty",
                            p { "No messages yet" }
                            p { class: "chat-empty-hint", "Send a message to start the conversation" }
                        }
                    }
                    for msg in (chat_state.messages)().iter() {
                        {render_message(msg)}
                    }
                }
            }

            form {
                class: "chat-input-area",
                onsubmit: on_send,
                input {
                    class: "chat-input",
                    r#type: "text",
                    placeholder: "Type a message...",
                    value: "{input_text}",
                    disabled: !(chat_state.is_connected)() || sending(),
                    oninput: move |e: Event<FormData>| {
                        input_text.set(e.value());
                    },
                }
                button {
                    class: "chat-send-button",
                    r#type: "submit",
                    disabled: input_text().trim().is_empty() || !(chat_state.is_connected)() || sending(),
                    svg {
                        xmlns: "http://www.w3.org/2000/svg",
                        width: "18",
                        height: "18",
                        view_box: "0 0 24 24",
                        fill: "none",
                        stroke: "currentColor",
                        stroke_width: "2",
                        stroke_linecap: "round",
                        stroke_linejoin: "round",
                        line { x1: "22", y1: "2", x2: "11", y2: "13" }
                        polygon { points: "22 2 15 22 11 13 2 9 22 2" }
                    }
                }
            }
        }
    }
}

/// Render a single chat message.
fn render_message(msg: &ChatMessage) -> Element {
    let time_str = format_timestamp(msg.timestamp);
    let sender = msg.sender_name.clone();
    let content = msg.content.clone();

    rsx! {
        div { class: "chat-message",
            div { class: "chat-message-header",
                span { class: "chat-sender", "{sender}" }
                span { class: "chat-timestamp", "{time_str}" }
            }
            p { class: "chat-content", "{content}" }
        }
    }
}

/// Format a millisecond timestamp into a short time string (HH:MM).
fn format_timestamp(ts: f64) -> String {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ts));
    let hours = date.get_hours();
    let minutes = date.get_minutes();
    format!("{hours:02}:{minutes:02}")
}
