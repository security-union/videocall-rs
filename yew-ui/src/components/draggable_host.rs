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

use crate::components::host::Host;
use gloo_storage::{LocalStorage, Storage};
use serde::{Deserialize, Serialize};
use videocall_client::VideoCallClient;
use wasm_bindgen::JsCast;
use web_sys::MouseEvent;
use yew::prelude::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HostPosition {
    x: f64,
    y: f64,
    viewport_width: f64,
    viewport_height: f64,
}

const STORAGE_KEY: &str = "videocall_host_position";

#[derive(Debug)]
pub enum Msg {
    StartDrag(MouseEvent),
    StartDragTouch(web_sys::TouchEvent),
    Drag(MouseEvent),
    DragTouch(web_sys::TouchEvent),
    EndDrag,
}

pub struct DraggableHost {
    position: Option<(f64, f64)>,
    is_dragging: bool,
    drag_offset: Option<(f64, f64)>,
    element_size: Option<(f64, f64)>,
}

#[derive(Properties, Debug, PartialEq)]
pub struct DraggableHostProps {
    #[prop_or_default]
    pub id: String,
    pub client: VideoCallClient,
    pub share_screen: bool,
    pub mic_enabled: bool,
    pub video_enabled: bool,
    pub on_encoder_settings_update: Callback<String>,
    pub device_settings_open: bool,
    pub on_device_settings_toggle: Callback<MouseEvent>,
    #[prop_or_default]
    pub on_microphone_error: Callback<String>,
}

impl Component for DraggableHost {
    type Message = Msg;
    type Properties = DraggableHostProps;

    fn create(_ctx: &Context<Self>) -> Self {
        // Load position from localStorage
        let position = Self::load_position();

        Self {
            position,
            is_dragging: false,
            drag_offset: None,
            element_size: None,
        }
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Self::Message) -> bool {
        match msg {
            Msg::StartDrag(event) => {
                if let Some(window) = web_sys::window() {
                    let client_x = event.client_x() as f64;
                    let client_y = event.client_y() as f64;

                    // Get current position or calculate default
                    let (current_x, current_y) = self.position.unwrap_or_else(|| {
                        Self::calculate_default_position(&window, self.element_size)
                    });

                    // Calculate offset from click point to element origin
                    self.drag_offset = Some((client_x - current_x, client_y - current_y));
                    self.is_dragging = true;

                    // Add global event listeners
                    Self::add_global_listeners(ctx);
                }
                true
            }
            Msg::StartDragTouch(event) => {
                let touches = event.touches();
                if touches.length() == 1 {
                    if let Some(touch) = touches.get(0) {
                        if let Some(window) = web_sys::window() {
                            let client_x = touch.client_x() as f64;
                            let client_y = touch.client_y() as f64;

                            let (current_x, current_y) = self.position.unwrap_or_else(|| {
                                Self::calculate_default_position(&window, self.element_size)
                            });

                            self.drag_offset = Some((client_x - current_x, client_y - current_y));
                            self.is_dragging = true;

                            Self::add_global_listeners(ctx);
                        }
                    }
                }
                true
            }
            Msg::Drag(event) => {
                if self.is_dragging {
                    if let Some((offset_x, offset_y)) = self.drag_offset {
                        if let Some(window) = web_sys::window() {
                            let client_x = event.client_x() as f64;
                            let client_y = event.client_y() as f64;

                            let new_x = client_x - offset_x;
                            let new_y = client_y - offset_y;

                            // Constrain to viewport bounds
                            self.position = Some(Self::constrain_position(
                                new_x,
                                new_y,
                                &window,
                                self.element_size,
                            ));
                        }
                    }
                    true
                } else {
                    false
                }
            }
            Msg::DragTouch(event) => {
                if self.is_dragging {
                    let touches = event.touches();
                    if let Some(touch) = touches.get(0) {
                        if let Some((offset_x, offset_y)) = self.drag_offset {
                            if let Some(window) = web_sys::window() {
                                event.prevent_default();
                                let client_x = touch.client_x() as f64;
                                let client_y = touch.client_y() as f64;

                                let new_x = client_x - offset_x;
                                let new_y = client_y - offset_y;

                                self.position = Some(Self::constrain_position(
                                    new_x,
                                    new_y,
                                    &window,
                                    self.element_size,
                                ));
                            }
                        }
                    }
                    true
                } else {
                    false
                }
            }
            Msg::EndDrag => {
                if self.is_dragging {
                    self.is_dragging = false;
                    self.drag_offset = None;

                    // Save position to localStorage
                    if let Some(pos) = self.position {
                        Self::save_position(pos);
                    }

                    Self::remove_global_listeners(ctx);
                    true
                } else {
                    false
                }
            }
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let onmousedown = ctx.link().callback(Msg::StartDrag);
        let ontouchstart = ctx.link().callback(|e: TouchEvent| {
            let event: &web_sys::Event = e.as_ref();
            let touch_event: web_sys::TouchEvent = event.clone().unchecked_into();
            Msg::StartDragTouch(touch_event)
        });

        // Global listeners for drag continuation and end
        let onmousemove = if self.is_dragging {
            ctx.link().callback(Msg::Drag)
        } else {
            Callback::noop()
        };

        let ontouchmove = if self.is_dragging {
            ctx.link().callback(|e: TouchEvent| {
                let event: &web_sys::Event = e.as_ref();
                let touch_event: web_sys::TouchEvent = event.clone().unchecked_into();
                Msg::DragTouch(touch_event)
            })
        } else {
            Callback::noop()
        };

        let onmouseup = if self.is_dragging {
            ctx.link().callback(|_| Msg::EndDrag)
        } else {
            Callback::noop()
        };

        let ontouchend = if self.is_dragging {
            ctx.link().callback(|_| Msg::EndDrag)
        } else {
            Callback::noop()
        };

        // Calculate position style
        let position_style = if let Some((x, y)) = self.position {
            format!("position: absolute; left: {x}px; top: {y}px;")
        } else {
            // Use default CSS positioning (bottom-right)
            String::new()
        };

        let cursor_style = if self.is_dragging {
            "cursor: grabbing; user-select: none;"
        } else {
            "cursor: grab;"
        };

        let wrapper_style = format!("{position_style} {cursor_style}");

        html! {
            <nav
                class="host"
                style={wrapper_style}
                onmousemove={onmousemove}
                onmouseup={onmouseup}
                ontouchmove={ontouchmove}
                ontouchend={ontouchend}
            >
                <div
                    class="drag-handle"
                    onmousedown={onmousedown}
                    ontouchstart={ontouchstart}
                    style="position: relative; width: 100%; height: 100%;"
                >
                    <Host
                        id={ctx.props().id.clone()}
                        client={ctx.props().client.clone()}
                        share_screen={ctx.props().share_screen}
                        mic_enabled={ctx.props().mic_enabled}
                        video_enabled={ctx.props().video_enabled}
                        on_encoder_settings_update={ctx.props().on_encoder_settings_update.clone()}
                        device_settings_open={ctx.props().device_settings_open}
                        on_device_settings_toggle={ctx.props().on_device_settings_toggle.clone()}
                        on_microphone_error={ctx.props().on_microphone_error.clone()}
                    />
                </div>
            </nav>
        }
    }
}

impl DraggableHost {
    fn load_position() -> Option<(f64, f64)> {
        if let Ok(stored) = LocalStorage::get::<HostPosition>(STORAGE_KEY) {
            if let Some(window) = web_sys::window() {
                let current_width = window.inner_width().ok()?.as_f64()?;
                let current_height = window.inner_height().ok()?.as_f64()?;

                // Check if viewport has changed significantly (>20%)
                let width_ratio =
                    (current_width - stored.viewport_width).abs() / stored.viewport_width;
                let height_ratio =
                    (current_height - stored.viewport_height).abs() / stored.viewport_height;

                if width_ratio < 0.2 && height_ratio < 0.2 {
                    // Validate position is still within bounds
                    let constrained = Self::constrain_position(stored.x, stored.y, &window, None);
                    return Some(constrained);
                }
            }
        }
        None
    }

    fn save_position(pos: (f64, f64)) {
        if let Some(window) = web_sys::window() {
            if let (Ok(width), Ok(height)) = (window.inner_width(), window.inner_height()) {
                if let (Some(w), Some(h)) = (width.as_f64(), height.as_f64()) {
                    let position = HostPosition {
                        x: pos.0,
                        y: pos.1,
                        viewport_width: w,
                        viewport_height: h,
                    };
                    let _ = LocalStorage::set(STORAGE_KEY, position);
                }
            }
        }
    }

    fn calculate_default_position(
        window: &web_sys::Window,
        element_size: Option<(f64, f64)>,
    ) -> (f64, f64) {
        if let (Ok(width), Ok(height)) = (window.inner_width(), window.inner_height()) {
            if let (Some(w), Some(h)) = (width.as_f64(), height.as_f64()) {
                let (elem_width, elem_height) = element_size.unwrap_or((240.0, 180.0));

                // Check if mobile (viewport width < 768px)
                let is_mobile = w < 768.0;

                if is_mobile {
                    // Mobile: bottom-right with mobile spacing
                    let x = w - elem_width - 8.0;
                    let y = h - elem_height - 78.0; // Account for controls
                    return (x.max(0.0), y.max(0.0));
                } else {
                    // Desktop: bottom-right with desktop spacing
                    let x = w - elem_width - 16.0;
                    let y = h - elem_height - 16.0;
                    return (x.max(0.0), y.max(0.0));
                }
            }
        }
        (16.0, 16.0) // Fallback position
    }

    fn constrain_position(
        x: f64,
        y: f64,
        window: &web_sys::Window,
        element_size: Option<(f64, f64)>,
    ) -> (f64, f64) {
        if let (Ok(width), Ok(height)) = (window.inner_width(), window.inner_height()) {
            if let (Some(w), Some(h)) = (width.as_f64(), height.as_f64()) {
                let (elem_width, elem_height) = element_size.unwrap_or((240.0, 180.0));

                let max_x = w - elem_width;
                let max_y = h - elem_height;

                let constrained_x = x.max(0.0).min(max_x);
                let constrained_y = y.max(0.0).min(max_y);

                return (constrained_x, constrained_y);
            }
        }
        (x.max(0.0), y.max(0.0))
    }

    fn add_global_listeners(_ctx: &Context<Self>) {
        // In a real implementation, add global mousemove/touchmove/mouseup/touchend listeners
        // For now, rely on event bubbling
    }

    fn remove_global_listeners(_ctx: &Context<Self>) {
        // Remove global listeners when drag ends
    }
}
