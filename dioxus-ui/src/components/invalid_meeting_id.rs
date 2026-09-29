// SPDX-License-Identifier: MIT OR Apache-2.0

//! Full-page notice for a meeting URL whose ID the shared meeting-ID rule
//! rejects, whether the client or the meeting API refused it.

use crate::context::{
    describe_disallowed_chars, validate_meeting_id, MeetingIdError, MEETING_ID_ALLOWED_CHARS,
};
use crate::meeting_api::JoinError;
use crate::theme::color as theme_color;
use dioxus::prelude::*;

const INVALID_MEETING_ID_CODE: &str = "INVALID_MEETING_ID";
const API_MESSAGE_PREFIX: &str = "Invalid meeting ID: ";

/// The notice's hint on the meeting and guest routes.
pub const JOIN_HINT: &str = "Check the link you were given, or ask the host for the meeting ID.";
/// The notice's hint on the meeting settings route.
pub const SETTINGS_HINT: &str =
    "Check the link, or open the meeting from your list on the home page.";

/// Why the meeting ID taken from a URL is refused, or `None` when it is valid.
pub fn meeting_route_id_error(id: &str) -> Option<String> {
    let detail = match validate_meeting_id(id).err()? {
        MeetingIdError::InvalidChars(chars) => format!(
            "contains characters that are not allowed ({}); use only {MEETING_ID_ALLOWED_CHARS}",
            describe_disallowed_chars(chars)
        ),
        err => err.to_string(),
    };
    Some(format!("Meeting ID {detail}."))
}

/// The reason to show when the meeting API refused the meeting ID itself
/// (HTTP 400 `INVALID_MEETING_ID`), or `None` for any other error.
pub fn invalid_meeting_id_message(error: &JoinError) -> Option<String> {
    let JoinError::ServerError { status: 400, body } = error else {
        return None;
    };
    let response: serde_json::Value = serde_json::from_str(body).ok()?;
    let result = &response["result"];
    if result["code"] != INVALID_MEETING_ID_CODE {
        return None;
    }
    let message = result["message"].as_str().unwrap_or_default();
    Some(match message.strip_prefix(API_MESSAGE_PREFIX) {
        Some(detail) => format!("Meeting ID {}.", detail.trim_end_matches('.')),
        None => message.to_string(),
    })
}

#[component]
pub fn InvalidMeetingIdNotice(
    reason: String,
    #[props(default = JOIN_HINT)] hint: &'static str,
) -> Element {
    rsx! {
        div { class: "error-container", "data-testid": "meeting-error",
            div { class: "error-card card-apple", "data-testid": "meeting-invalid-id",
                svg {
                    xmlns: "http://www.w3.org/2000/svg",
                    width: "64",
                    height: "64",
                    view_box: "0 0 24 24",
                    fill: "none",
                    stroke: theme_color::WARNING_ICON,
                    stroke_width: "1.5",
                    "aria-hidden": "true",
                    "focusable": "false",
                    circle { cx: "12", cy: "12", r: "10" }
                    line { x1: "12", y1: "8", x2: "12", y2: "12" }
                    line { x1: "12", y1: "16", x2: "12.01", y2: "16" }
                }
                h2 {
                    id: "meeting-invalid-id-heading",
                    tabindex: "-1",
                    onmounted: move |e| {
                        let element = e.data();
                        spawn(async move {
                            let _ = element.set_focus(true).await;
                        });
                    },
                    "Invalid meeting ID"
                }
                p {
                    class: "error-card__reason",
                    "data-testid": "meeting-invalid-id-reason",
                    "{reason}"
                }
                p { "data-testid": "meeting-invalid-id-hint", "{hint}" }
                button {
                    class: "btn-apple btn-primary",
                    onclick: move |_| {
                        if let Some(w) = web_sys::window() {
                            let _ = w.location().set_href("/");
                        }
                    },
                    "Return to Home"
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use videocall_meeting_types::{APIError, APIResponse};

    fn server_error(status: u16, error: APIError) -> JoinError {
        JoinError::ServerError {
            status,
            body: serde_json::to_string(&APIResponse::error(error)).unwrap(),
        }
    }

    #[test]
    fn route_ids_the_shared_rule_accepts_are_not_refused() {
        for id in ["abc_123", "my-meeting", "a~b", "Mixed_Case-9~x"] {
            assert_eq!(meeting_route_id_error(id), None, "{id:?}");
        }
    }

    #[test]
    fn route_id_refusal_names_the_offending_characters() {
        let reason = meeting_route_id_error("a.b c").expect("a.b c is invalid");
        assert_eq!(
            reason,
            format!(
                "Meeting ID contains characters that are not allowed ('.', space); \
                 use only {MEETING_ID_ALLOWED_CHARS}."
            )
        );
        assert!(meeting_route_id_error(&"a".repeat(256)).is_some());
    }

    #[test]
    fn route_id_refusal_names_hard_to_read_characters_readably() {
        for (id, listed) in [
            ("o'brien", "(apostrophe)"),
            ("a\\b", "('\\')"),
            ("a\tb\u{202e}", "('\\t', '\\u{202e}')"),
        ] {
            let reason = meeting_route_id_error(id).expect("invalid");
            assert!(reason.contains(listed), "{id:?}: {reason}");
        }
    }

    #[test]
    fn the_api_refusal_reads_like_the_client_refusal() {
        let too_long = "a".repeat(256);
        for id in ["a.b", "caf\u{e9}", too_long.as_str()] {
            let err = validate_meeting_id(id).unwrap_err();
            // The body meeting-api's `From<MeetingIdError> for AppError` sends.
            let error = server_error(400, APIError::invalid_meeting_id(&err.to_string()));
            let reason = invalid_meeting_id_message(&error).expect("INVALID_MEETING_ID");
            assert_eq!(Some(reason.clone()), meeting_route_id_error(id), "{id:?}");
            assert!(!reason.contains("Invalid meeting ID"), "{reason}");
        }
    }

    #[test]
    fn the_api_refusal_keeps_the_servers_character_list() {
        let err = validate_meeting_id("a b").unwrap_err();
        let error = server_error(400, APIError::invalid_meeting_id(&err.to_string()));
        let reason = invalid_meeting_id_message(&error).expect("INVALID_MEETING_ID");
        assert_eq!(reason, format!("Meeting ID {err}."));
        assert!(reason.contains("(' ')"), "{reason}");
    }

    #[test]
    fn an_unrecognised_api_message_is_shown_verbatim() {
        let body = serde_json::json!({
            "success": false,
            "result": { "code": INVALID_MEETING_ID_CODE, "message": "Nope" },
        });
        let error = JoinError::ServerError {
            status: 400,
            body: body.to_string(),
        };
        assert_eq!(invalid_meeting_id_message(&error).as_deref(), Some("Nope"));
    }

    #[test]
    fn other_errors_are_not_treated_as_an_invalid_meeting_id() {
        let other_code = server_error(400, APIError::too_many_attendees(101, 100));
        assert_eq!(invalid_meeting_id_message(&other_code), None);

        let wrong_status = server_error(500, APIError::invalid_meeting_id("x"));
        assert_eq!(invalid_meeting_id_message(&wrong_status), None);

        let not_json = JoinError::ServerError {
            status: 400,
            body: "INVALID_MEETING_ID".to_string(),
        };
        assert_eq!(invalid_meeting_id_message(&not_json), None);

        assert_eq!(
            invalid_meeting_id_message(&JoinError::MeetingNotActive),
            None
        );
    }
}
