// SPDX-License-Identifier: MIT OR Apache-2.0

//! Meeting-password prompt — the client half of issue 1613.
//!
//! `meeting-api` verifies a meeting's password on every non-owner join path and
//! answers `403 MEETING_PASSWORD_REQUIRED` (protected, none supplied) or
//! `403 INVALID_MEETING_PASSWORD` (wrong password, or a stored hash the server
//! could not parse — deliberately indistinguishable on the wire). This module
//! turns those two codes into a prompt and feeds the entered value back into
//! the same join call.
//!
//! # Why the prompt is driven by the 403, not by `has_password`
//!
//! `has_password` is a display boolean on a meeting *listing*. It is absent on
//! every path that reaches a meeting without going through that listing (a deep
//! link, an invite, a page refresh, the `/meeting/{id}/guest` URL); it goes
//! stale the moment a host adds or removes a password on a page that is already
//! open; and it says nothing about whether *this* caller is exempt — the server
//! exempts the meeting owner (`creator_id`), which the client cannot decide
//! authoritatively. The server knows all three, so the server's answer is the
//! trigger.
//!
//! Two properties fall out of that and are worth stating because they are the
//! reason for the choice: a meeting that gains a password *after* the page
//! loaded still prompts correctly, and the owner is never prompted because the
//! owner never receives the 403. There is deliberately **no** client-side owner
//! check — a client-side check would be both redundant (the server is the gate)
//! and unreliable (the client's view of ownership is the response it has not
//! made yet).
//!
//! # Handling of the plaintext
//!
//! The entered value lives in a component-local `Signal<String>` and travels no
//! further than the `password` argument of
//! [`crate::meeting_api::join_meeting`] / [`crate::meeting_api::join_meeting_as_guest`].
//! It is never written to `localStorage`, `sessionStorage`, a cookie, or the
//! URL — unlike the display name, which `save_display_name_to_storage` does
//! persist — and it is never logged at any level. The joining page drops its
//! copy as soon as the join reaches a state that cannot need it again.

use crate::meeting_api::JoinError;
use dioxus::prelude::*;
use wasm_bindgen::JsCast;

/// DOM id of the password field. Three things share it: the `<label for>`, the
/// re-focus helper below (which finds the field by id, not by element handle),
/// and the submit handler's focus park.
pub const PASSWORD_INPUT_ID: &str = "meeting-password";

/// DOM id of the error paragraph, referenced by the field's `aria-describedby`.
/// `e2e/tests/guest-join.spec.ts` asserts that attribute's value literally.
const PASSWORD_ERROR_ID: &str = "meeting-password-error";

/// DOM id of the heading, referenced by the dialog's `aria-labelledby`.
const PASSWORD_HEADING_ID: &str = "meeting-password-heading";

/// DOM id of the instruction paragraph. It is the field's `aria-describedby`
/// target whenever there is no error, so that attribute always points at a node
/// that exists — an empty `aria-describedby` is a dangling IDREF list that
/// automated a11y checks flag.
const PASSWORD_INSTRUCTION_ID: &str = "meeting-password-instruction";

/// Body copy. Identical for every reason on purpose: it states what the field
/// is for, and that does not change between "you have not supplied one", "the
/// one you supplied was wrong" and "the server would not look at it right now".
/// What distinguishes those is the heading and the error region — see
/// [`PasswordPromptReason`].
const PASSWORD_INSTRUCTION: &str = "This meeting is protected by a password. Enter it to join.";

/// Why the prompt is on screen.
///
/// Two of these are verdicts on the password ([`Self::Required`],
/// [`Self::Invalid`]); the other two are transient refusals that happened
/// *before* the server evaluated anything. That distinction is not cosmetic —
/// see [`Self::discards_entered_value`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PasswordPromptReason {
    /// `MEETING_PASSWORD_REQUIRED` (403) — the meeting has a password and the
    /// attempt carried none. Nothing the user typed was rejected, so nothing is
    /// shown as an error.
    Required,
    /// `INVALID_MEETING_PASSWORD` (403) — the attempt carried a password and the
    /// server verified it as wrong. The only reason that is a verdict on what
    /// the user typed.
    Invalid,
    /// `TOO_MANY_PASSWORD_ATTEMPTS` (429), or `RATE_LIMIT_EXCEEDED` (429) on an
    /// attempt that carried a password — the failed-attempt budget for this
    /// `(client IP, meeting)` window is spent.
    ///
    /// `meeting-api/src/password.rs` rejects in `consume_attempt`, *before*
    /// `verify_offloaded`, so the supplied password was never hashed.
    Throttled,
    /// `VERIFIER_OVERLOADED` (503) — no Argon2 permit became available inside
    /// the server's queue timeout and the request was shed. Also rejected
    /// before verification, and immediately retryable.
    Overloaded,
}

impl PasswordPromptReason {
    /// Accessible name of the dialog. Distinct per reason so a screen reader
    /// user hears which situation they are in without having to reach the error
    /// region.
    pub fn heading(self) -> &'static str {
        match self {
            Self::Required => "Password required",
            Self::Invalid => "Incorrect password",
            Self::Throttled => "Too many attempts",
            Self::Overloaded => "Server busy",
        }
    }

    /// The rejection message, or `None` when nothing has been rejected.
    ///
    /// `None` is what keeps the `role="alert"` node out of the DOM on the first
    /// prompt, which is what lets the first rejection *insert* it — a live
    /// region that is present from the start is announced only when its text
    /// changes, and would therefore stay silent for a prompt that opens straight
    /// into [`Self::Invalid`].
    ///
    /// The two transient messages deliberately do NOT say the password was
    /// wrong, because the server does not know: it refused before verifying.
    /// They also carry no countdown — the server's window is an internal
    /// constant (`PASSWORD_ATTEMPT_WINDOW_SECS`) that is never sent to the
    /// client, and inventing a timer here would be a number with nothing behind
    /// it.
    pub fn error(self) -> Option<&'static str> {
        match self {
            Self::Required => None,
            Self::Invalid => {
                Some("That password was incorrect. Check it with the meeting host and try again.")
            }
            Self::Throttled => {
                Some("Too many attempts. Wait about a minute, then try again — this one was not checked.")
            }
            Self::Overloaded => {
                Some("We couldn't check your password just now. Try again in a moment.")
            }
        }
    }

    /// Should the value in the field be discarded when this reason arrives?
    ///
    /// Only [`Self::Invalid`] — the one reason that is a verdict on what the
    /// user typed. [`Self::Throttled`] and [`Self::Overloaded`] are refusals the
    /// server issued *before* running Argon2 (`consume_attempt` and the permit
    /// acquisition both return early), so the value in the field may be exactly
    /// right; clearing it would make the user retype a password the server never
    /// looked at, and on the throttled path they would have to retype it once
    /// per rejected retry until the window expires.
    ///
    /// [`Self::Required`] never has anything to discard: it can only arrive on
    /// an attempt that carried no password.
    pub fn discards_entered_value(self) -> bool {
        matches!(self, Self::Invalid)
    }
}

/// Map a failed join to the prompt it should raise, if any.
///
/// `None` means "this is not a password problem" — the caller must fall through
/// to its existing error handling rather than prompting.
///
/// # Why this needs `supplied_password`
///
/// `RATE_LIMIT_EXCEEDED` is **not** a password code. `POST /join` runs the
/// generic rename limiter first, and only for requests carrying a
/// `display_name`, so on the authenticated path it *shadows*
/// `TOO_MANY_PASSWORD_ATTEMPTS` — a real UI client that guesses wrong five
/// times sees `RATE_LIMIT_EXCEEDED`, and handling only the password-specific
/// code would miss the common case. But the same code also fires for ordinary
/// rename spam on a meeting that may have no password at all, and mapping it
/// unconditionally would raise a password prompt on an unprotected meeting.
///
/// `supplied_password` resolves that: it is true only when the attempt being
/// answered actually carried a password, which means the flow is already in the
/// prompt. Every other reason is unambiguous and ignores it.
pub fn password_prompt_reason(
    error: &JoinError,
    supplied_password: bool,
) -> Option<PasswordPromptReason> {
    match error {
        JoinError::MeetingPasswordRequired => Some(PasswordPromptReason::Required),
        JoinError::InvalidMeetingPassword => Some(PasswordPromptReason::Invalid),
        JoinError::TooManyPasswordAttempts => Some(PasswordPromptReason::Throttled),
        JoinError::VerifierOverloaded => Some(PasswordPromptReason::Overloaded),
        JoinError::RateLimitExceeded if supplied_password => Some(PasswordPromptReason::Throttled),
        _ => None,
    }
}

/// Reactive state of the prompt, owned by the joining page so its async join
/// handler can drive the prompt from outside the component.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PasswordPromptState {
    /// Which server code put the prompt on screen.
    pub reason: PasswordPromptReason,
    /// How many server outcomes this prompt has displayed. Bumped by
    /// [`Self::rejected`] for **every** refusal — a wrong password, a throttle
    /// and an overload alike.
    ///
    /// It is a sequence number, not a count of wrong guesses, and two separate
    /// things key off it:
    ///
    /// * the `key` on the `role="alert"` node, so Dioxus **re-creates** that
    ///   node on every refusal. Without that, a second consecutive refusal
    ///   renders identical text, Dioxus diffs it in place, and the live region
    ///   produces zero mutations — the rejection is announced to nobody;
    /// * the clear/refocus effect's edge, because `reason` alone cannot
    ///   distinguish "second wrong password" from "no change at all".
    ///
    /// Whether the entered value is *discarded* is decided separately, by
    /// [`PasswordPromptReason::discards_entered_value`] — advancing this counter
    /// never implies the user typed something wrong.
    pub outcome_seq: u32,
    /// A join carrying the entered password is in flight.
    pub submitting: bool,
}

impl PasswordPromptState {
    /// The prompt is appearing for the first time in this join.
    pub fn opened(reason: PasswordPromptReason) -> Self {
        Self {
            reason,
            outcome_seq: 0,
            submitting: false,
        }
    }

    /// The in-flight attempt came back refused, for any of the four reasons.
    pub fn rejected(self, reason: PasswordPromptReason) -> Self {
        Self {
            reason,
            outcome_seq: self.outcome_seq.saturating_add(1),
            submitting: false,
        }
    }

    /// An attempt has been handed to the join call.
    pub fn submitted(self) -> Self {
        Self {
            submitting: true,
            ..self
        }
    }
}

/// The prompt state a rejected join should move to.
///
/// `supplied_password` is what distinguishes the two cases, and it has to be
/// the *attempt's* answer rather than the prompt's own state: "the join carried
/// no password, so we have just discovered the meeting has one" opens a fresh
/// prompt, while "an attempt that carried a password was refused" advances
/// `outcome_seq` — which is what re-creates the live region and re-arms the
/// clear/refocus edge. A second consecutive refusal leaves `reason` unchanged
/// and would otherwise be indistinguishable from no change at all.
pub fn next_prompt_state(
    current: PasswordPromptState,
    reason: PasswordPromptReason,
    supplied_password: bool,
) -> PasswordPromptState {
    if supplied_password {
        current.rejected(reason)
    } else {
        PasswordPromptState::opened(reason)
    }
}

/// Move focus to an element by DOM id. Same shape as the helper in
/// `device_settings_modal.rs`; kept local so this module does not depend on a
/// modal that has nothing else to do with it.
fn focus_element_by_id(id: &str) {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(id))
    {
        if let Ok(html) = el.dyn_into::<web_sys::HtmlElement>() {
            let _ = html.focus();
        }
    }
}

/// The password prompt.
///
/// Rendered *as* the joining view (it replaces the join form / the joining
/// spinner rather than floating over them), which is why `aria-modal="true"` is
/// truthful here: while it is on screen there is no other content on the page
/// for assistive technology to reach.
///
/// # Focus
///
/// * **On open** — the `onmounted` handler on the field moves focus into it, so
///   a keyboard or screen-reader user lands on the thing they have to fill in.
/// * **On a rejected attempt** — the effect below clears the field and moves
///   focus back to it. Focus may be on the submit button at that point (the
///   user clicked it), and leaving it there would make the user tab backwards
///   to retry.
/// * **On close** — the prompt does not close itself. Both exits are the
///   parent's: a successful join swaps in the meeting view (which takes focus
///   with it), and `on_cancel` returns to a view the parent chooses and focuses
///   — the guest page puts focus back on the name field it came from, the
///   meeting page navigates away. Focus is never dropped to `<body>`.
///
/// # How the error reaches a screen reader
///
/// Two independent paths, because either one alone has a hole:
/// * `role="alert"` (implicit `aria-live="assertive"`, never overridden) on a
///   node that is *inserted* on the first rejection — announced on insertion.
/// * The field's `aria-describedby` points at that node and focus is moved to
///   the field, so the error is read as part of the field's description. This
///   is what covers the second and later rejections, where the alert node's
///   text does not change and a live region would stay silent.
///
/// The cost is that the first rejection may be announced twice. That is the
/// right trade against a rejection that is announced not at all.
#[component]
pub fn MeetingPasswordPrompt(
    /// Owned by the parent so the async join handler can update it.
    state: Signal<PasswordPromptState>,
    /// Label of the secondary action. The parent decides where it goes, so it
    /// also supplies the wording.
    cancel_label: String,
    /// Called with the entered password, verbatim and untrimmed.
    on_submit: EventHandler<String>,
    /// Called when the user backs out (button, or Escape).
    on_cancel: EventHandler<()>,
) -> Element {
    // The plaintext lives here and nowhere else on the client. Dropped with the
    // component, which the parent unmounts on a successful join.
    let mut entered = use_signal(String::new);

    // Edge-trigger on the outcome sequence ONLY. `use_memo` re-notifies its
    // subscribers only when its output actually changes, so flipping
    // `submitting` — which happens on every submit — does not reach the effect
    // below and cannot wipe the field out from under an in-flight attempt.
    let outcome_seq = use_memo(move || state.read().outcome_seq);

    use_effect(move || {
        if outcome_seq() == 0 {
            // First render of this prompt: `onmounted` on the field has already
            // moved focus there and there is nothing entered to clear.
            return;
        }
        // A refusal. Whether the value goes with it depends on WHY: only a
        // verdict on the password itself (`Invalid`) justifies clearing it. A
        // throttle or an overload refused the attempt before the server ran
        // Argon2, so what is in the field may be exactly right — and on the
        // throttled path, clearing would make the user retype it on every
        // rejected retry until the window expires.
        //
        // Peek before writing: this effect also runs on the first render of a
        // prompt that opens straight into `outcome_seq >= 1` (a retained
        // password replayed on the meeting-activation re-join was refused),
        // where the field is already empty and an unconditional `set` would
        // dirty every subscriber for nothing.
        if state.peek().reason.discards_entered_value() && !entered.peek().is_empty() {
            entered.set(String::new());
        }
        // Focus returns to the field either way — it is where the retry
        // happens, whether that means retyping or just pressing Join again.
        //
        // NOTE: this call is NOT what announces the error. `focus()` on an
        // already-focused element fires no focus event, and the submit handler
        // parks focus on this very field before every submit, so the
        // `aria-describedby` re-read never happens. The announcement comes
        // entirely from the keyed `role="alert"` node below.
        focus_element_by_id(PASSWORD_INPUT_ID);
    });

    let current = state();
    let reason = current.reason;
    let submitting = current.submitting;
    let error_message = reason.error();
    // Deliberately NOT `trim()`ed: a meeting password may legitimately begin or
    // end with a space, and trimming here would make such a password
    // untypeable. The check is only "did the user type anything at all".
    let has_input = !entered.read().is_empty();

    rsx! {
        div {
            class: "password-prompt-container",
            "data-testid": "meeting-password-prompt",
            // Focusable-by-script so the Escape handler below still reaches a
            // keydown after a click on the backdrop. Without it `activeElement`
            // becomes `<body>`, keydown never enters this subtree (events
            // bubble up, not down), and Escape silently stops working — while
            // the visible secondary button keeps working, which is worse than
            // no shortcut at all. `-1` keeps it out of the tab order.
            tabindex: "-1",
            // Escape backs out through the same handler as the visible
            // secondary button, so the keyboard shortcut can never do something
            // the button does not say it does.
            onkeydown: move |e: Event<KeyboardData>| {
                if e.key() == Key::Escape && !state.peek().submitting {
                    on_cancel.call(());
                }
            },

            div {
                class: "card-apple password-prompt-card",
                role: "dialog",
                "aria-modal": "true",
                "aria-labelledby": PASSWORD_HEADING_ID,

                div { class: "password-prompt-icon",
                    svg {
                        xmlns: "http://www.w3.org/2000/svg",
                        width: "48",
                        height: "48",
                        view_box: "0 0 24 24",
                        fill: "none",
                        stroke: "currentColor",
                        stroke_width: "1.5",
                        stroke_linecap: "round",
                        stroke_linejoin: "round",
                        "aria-hidden": "true",
                        rect {
                            x: "3",
                            y: "11",
                            width: "18",
                            height: "11",
                            rx: "2",
                        }
                        path { d: "M7 11V7a5 5 0 0 1 10 0v4" }
                    }
                }

                h2 { id: PASSWORD_HEADING_ID, "{reason.heading()}" }

                p {
                    id: PASSWORD_INSTRUCTION_ID,
                    class: "password-prompt-message",
                    "{PASSWORD_INSTRUCTION}"
                }

                form {
                    class: "password-prompt-form",
                    onsubmit: move |e: Event<FormData>| {
                        e.prevent_default();
                        if state.peek().submitting {
                            return;
                        }
                        let value = entered.peek().clone();
                        if value.is_empty() {
                            return;
                        }
                        // Park focus on the field before handing the attempt
                        // over. The submit button is `disabled` for the duration
                        // of the request, and disabling the focused element
                        // drops focus to `<body>` — so a user who submitted by
                        // clicking would spend the round trip with no focus at
                        // all. The field stays focusable throughout (it goes
                        // `readonly`, not `disabled`, for exactly this reason).
                        focus_element_by_id(PASSWORD_INPUT_ID);
                        on_submit.call(value);
                    },

                    label {
                        r#for: PASSWORD_INPUT_ID,
                        class: "password-prompt-label",
                        "Meeting password"
                    }

                    input {
                        id: PASSWORD_INPUT_ID,
                        class: "input-apple",
                        r#type: "password",
                        // `off`, not `current-password`. A meeting password is a
                        // per-room secret the host hands out, not a credential
                        // belonging to this user: offering to save it would put
                        // one meeting's shared secret in the user's vault under
                        // this origin, which is exactly the persistence this
                        // flow is built to avoid. (Browsers may still ignore
                        // `off` on a password field — this states the intent and
                        // suppresses it everywhere that honours it.)
                        autocomplete: "off",
                        "data-testid": "meeting-password-input",
                        // Keyed on "the server judged this value wrong", NOT on
                        // "there is an error on screen". A throttle or an
                        // overload puts a message up without the server having
                        // verified anything, and announcing the field as
                        // invalid there would tell a screen-reader user their
                        // password was rejected when it was never read.
                        "aria-invalid": if reason.discards_entered_value() { "true" } else { "false" },
                        // Always a live IDREF: the error node when there is one,
                        // the instruction otherwise. Moving focus to this field
                        // after a rejection is therefore what re-reads the error
                        // on the second and later attempts, where the alert
                        // node's text has not changed and a live region would
                        // stay silent.
                        "aria-describedby": if error_message.is_some() { PASSWORD_ERROR_ID } else { PASSWORD_INSTRUCTION_ID },
                        // `readonly`, NOT `disabled`: a disabled control is not
                        // focusable, so disabling it mid-request would eject
                        // focus to `<body>`. `readonly` blocks the edit and
                        // keeps the field as the focus anchor for the whole
                        // round trip.
                        readonly: submitting,
                        value: "{entered}",
                        oninput: move |e: Event<FormData>| entered.set(e.value()),
                        onmounted: move |e| {
                            let element = e.data();
                            spawn(async move {
                                let _ = element.set_focus(true).await;
                            });
                        },
                    }

                    if let Some(message) = error_message {
                        p {
                            // Keyed on the outcome sequence so Dioxus REPLACES
                            // this node on every refusal instead of diffing its
                            // text in place. That is the only thing that makes
                            // rejections 2, 3, 4... audible: consecutive
                            // refusals of the same kind render identical text,
                            // an unkeyed node produces zero DOM mutations, and
                            // a live region with no mutation announces nothing.
                            // The `aria-describedby` path cannot cover it —
                            // focus is already parked on the field, and
                            // `focus()` on a focused element fires no event.
                            key: "{current.outcome_seq}",
                            id: PASSWORD_ERROR_ID,
                            class: "password-prompt-error",
                            role: "alert",
                            "data-testid": "meeting-password-error",
                            "data-outcome-seq": "{current.outcome_seq}",
                            "{message}"
                        }
                    }

                    div { class: "password-prompt-actions",
                        button {
                            r#type: "button",
                            class: "btn-apple btn-secondary",
                            disabled: submitting,
                            onclick: move |_| on_cancel.call(()),
                            "{cancel_label}"
                        }
                        button {
                            r#type: "submit",
                            class: "btn-apple btn-primary",
                            "data-testid": "meeting-password-submit",
                            disabled: submitting || !has_input,
                            if submitting {
                                "Joining..."
                            } else {
                                "Join meeting"
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_code_maps_to_the_required_prompt() {
        assert_eq!(
            password_prompt_reason(&JoinError::MeetingPasswordRequired, false),
            Some(PasswordPromptReason::Required)
        );
    }

    #[test]
    fn invalid_code_maps_to_the_invalid_prompt() {
        assert_eq!(
            password_prompt_reason(&JoinError::InvalidMeetingPassword, true),
            Some(PasswordPromptReason::Invalid)
        );
    }

    /// The two throttle/overload codes are password-specific — the server can
    /// only reach either one on a request that supplied something to verify —
    /// so they raise the prompt regardless of what the caller passes for
    /// `supplied_password`.
    #[test]
    fn throttle_and_overload_codes_map_regardless_of_context() {
        for supplied in [false, true] {
            assert_eq!(
                password_prompt_reason(&JoinError::TooManyPasswordAttempts, supplied),
                Some(PasswordPromptReason::Throttled),
                "TooManyPasswordAttempts with supplied={supplied}"
            );
            assert_eq!(
                password_prompt_reason(&JoinError::VerifierOverloaded, supplied),
                Some(PasswordPromptReason::Overloaded),
                "VerifierOverloaded with supplied={supplied}"
            );
        }
    }

    /// `RATE_LIMIT_EXCEEDED` is the generic rename limiter, and it is the code a
    /// real authenticated client actually sees when it guesses wrong too often
    /// (`POST /join` runs that limiter first, for any request carrying a
    /// `display_name`). So it MUST raise the throttled prompt — but only for an
    /// attempt that carried a password. The same code fires for ordinary rename
    /// spam on a meeting with no password at all, where raising a password
    /// prompt would be a fabrication.
    #[test]
    fn rename_limiter_is_a_throttle_only_when_a_password_was_supplied() {
        assert_eq!(
            password_prompt_reason(&JoinError::RateLimitExceeded, true),
            Some(PasswordPromptReason::Throttled),
            "a throttled password attempt must stay in the prompt"
        );
        assert_eq!(
            password_prompt_reason(&JoinError::RateLimitExceeded, false),
            None,
            "rename spam on a passwordless meeting must not raise a password prompt"
        );
    }

    /// The gate that keeps the prompt from swallowing unrelated failures. Every
    /// one of these reached the join flow's own error rendering before issue
    /// 1613 and must keep reaching it — a prompt shown for "guests are not
    /// allowed" would ask the user for a password that cannot help them.
    #[test]
    fn non_password_errors_do_not_raise_a_prompt() {
        for error in [
            JoinError::GuestsNotAllowed,
            JoinError::JoiningNotAllowed,
            JoinError::NotAuthenticated,
            JoinError::MeetingNotActive,
            JoinError::Forbidden("nope".to_string()),
            JoinError::NotFound("gone".to_string()),
            JoinError::ServerError {
                status: 500,
                body: String::new(),
            },
        ] {
            // Checked under BOTH contexts: `supplied_password` widens the
            // mapping, so a test that only passed `false` would not notice a
            // future arm that leaked one of these through on `true`.
            for supplied in [false, true] {
                assert_eq!(
                    password_prompt_reason(&error, supplied),
                    None,
                    "{error:?} must not raise a password prompt (supplied={supplied})"
                );
            }
        }
    }

    /// Only a verdict on the password itself may discard what the user typed.
    /// A throttle or an overload refused the attempt before the server ran
    /// Argon2, so the value in the field may be exactly right — clearing it
    /// would make the user retype a password nobody ever looked at, once per
    /// rejected retry until the window expires.
    #[test]
    fn only_a_real_verdict_discards_the_entered_value() {
        assert!(PasswordPromptReason::Invalid.discards_entered_value());
        assert!(!PasswordPromptReason::Throttled.discards_entered_value());
        assert!(!PasswordPromptReason::Overloaded.discards_entered_value());
        assert!(!PasswordPromptReason::Required.discards_entered_value());
    }

    /// Every reason that reports a rejection must say something, and no two may
    /// collapse onto the same copy — "you need a password", "that one was
    /// wrong", "too many tries" and "server busy" call for different actions.
    #[test]
    fn every_reason_reads_differently() {
        let all = [
            PasswordPromptReason::Required,
            PasswordPromptReason::Invalid,
            PasswordPromptReason::Throttled,
            PasswordPromptReason::Overloaded,
        ];
        let headings: Vec<&str> = all.iter().map(|r| r.heading()).collect();
        for (i, a) in headings.iter().enumerate() {
            for b in headings.iter().skip(i + 1) {
                assert_ne!(a, b, "two reasons share the heading {a:?}");
            }
        }
        // Only `Required` is silent; it is the one reason that has rejected
        // nothing.
        assert_eq!(PasswordPromptReason::Required.error(), None);
        for reason in [
            PasswordPromptReason::Invalid,
            PasswordPromptReason::Throttled,
            PasswordPromptReason::Overloaded,
        ] {
            assert!(
                reason.error().is_some_and(|m| !m.is_empty()),
                "{reason:?} reports a rejection with no message"
            );
        }
        // The transient messages must not imply the user got it wrong — the
        // server refused before verifying, so it has no idea. Asserted on the
        // property (no fault language) rather than on an exact phrase, so
        // rewording the copy does not silently drop the guarantee.
        for reason in [
            PasswordPromptReason::Throttled,
            PasswordPromptReason::Overloaded,
        ] {
            let message = reason.error().expect("checked above").to_lowercase();
            for blamed in ["incorrect", "wrong", "invalid"] {
                assert!(
                    !message.contains(blamed),
                    "{reason:?} blames the user with {blamed:?}, but the server never \
                     checked the password: {message:?}"
                );
            }
        }
    }

    /// Issue 1613 requires "this meeting needs a password" and "that password
    /// was wrong" to read differently. Pins that the two reasons cannot
    /// collapse onto the same user-visible copy.
    #[test]
    fn the_two_reasons_say_different_things() {
        assert_ne!(
            PasswordPromptReason::Required.heading(),
            PasswordPromptReason::Invalid.heading()
        );
        assert_eq!(PasswordPromptReason::Required.error(), None);
        assert!(PasswordPromptReason::Invalid
            .error()
            .is_some_and(|m| !m.is_empty()));
    }

    #[test]
    fn a_fresh_prompt_has_no_rejected_attempts() {
        let state = PasswordPromptState::opened(PasswordPromptReason::Required);
        assert_eq!(state.outcome_seq, 0);
        assert!(!state.submitting);
        assert_eq!(state.reason, PasswordPromptReason::Required);
    }

    /// The field-clear/refocus edge trigger keys off `attempt`, so a second
    /// wrong password must advance it even though `reason` does not move.
    #[test]
    fn every_rejection_advances_the_attempt_counter() {
        let first = PasswordPromptState::opened(PasswordPromptReason::Required)
            .submitted()
            .rejected(PasswordPromptReason::Invalid);
        assert_eq!(first.outcome_seq, 1);
        assert_eq!(first.reason, PasswordPromptReason::Invalid);
        assert!(!first.submitting, "a rejection ends the in-flight attempt");

        let second = first.submitted().rejected(PasswordPromptReason::Invalid);
        assert_eq!(
            second.outcome_seq, 2,
            "a second rejection with an unchanged reason must still advance"
        );
    }

    #[test]
    fn submitting_marks_the_attempt_in_flight_without_advancing_it() {
        let state = PasswordPromptState::opened(PasswordPromptReason::Required).submitted();
        assert!(state.submitting);
        assert_eq!(state.outcome_seq, 0);
    }

    /// The first 403 arrives on a join that carried no password (the pages send
    /// `None` until the prompt exists). Nothing the user typed was refused, so
    /// the prompt opens rather than reporting a rejected attempt.
    #[test]
    fn a_join_without_a_password_opens_the_prompt() {
        let opened = next_prompt_state(
            PasswordPromptState::opened(PasswordPromptReason::Required),
            PasswordPromptReason::Required,
            false,
        );
        assert_eq!(opened.outcome_seq, 0);
        assert_eq!(opened.reason, PasswordPromptReason::Required);
    }

    /// A join that carried a password and was refused is a rejected attempt,
    /// which must advance the counter every single time — this is the signal
    /// the prompt keys off to clear the field and return focus to it.
    #[test]
    fn a_refused_password_advances_the_attempt_every_time() {
        let mut state = PasswordPromptState::opened(PasswordPromptReason::Required);
        for expected in 1..=3u32 {
            state = next_prompt_state(
                state.submitted(),
                PasswordPromptReason::Invalid,
                /* supplied_password */ true,
            );
            assert_eq!(
                state.outcome_seq, expected,
                "rejection {expected} did not advance the attempt counter"
            );
            assert_eq!(state.reason, PasswordPromptReason::Invalid);
            assert!(!state.submitting);
        }
    }
}
