/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

use yew::prelude::*;

#[derive(Properties, Debug, PartialEq)]
pub struct ConfigErrorProps {
    pub message: String,
}

#[function_component(ConfigError)]
pub fn config_error(props: &ConfigErrorProps) -> Html {
    html! {
        <div class="error-container">
            <p class="error-message">{ props.message.clone() }</p>
            <img src="/assets/street_fighter.gif" alt="Permission instructions" class="instructions-gif" />
        </div>
    }
}
