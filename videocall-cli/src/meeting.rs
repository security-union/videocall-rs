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

//! `videocall-cli meeting`: create and list meetings with this machine's device key.

use std::fmt::Write as _;

use videocall_meeting_types::{requests::CreateMeetingRequest, responses::ListMeetingsResponse};

use crate::auth::{store::Store, ApiOrigin, AuthError, Credentials};
use crate::cli_args::MeetingCommand;

/// Run a `meeting` subcommand. Needs no camera.
pub async fn run(command: &MeetingCommand) -> anyhow::Result<()> {
    let api = match command {
        MeetingCommand::Create(args) => &args.api,
        MeetingCommand::List(args) => &args.api,
    };
    let creds = Credentials::load(Store::open_default()?, ApiOrigin::from_url(&api.api_url)?);
    let creds = creds.await?;
    let output = match command {
        MeetingCommand::Create(args) => {
            let request = CreateMeetingRequest {
                meeting_id: args.meeting_id.clone(),
                attendees: Vec::new(),
                password: None,
                waiting_room_enabled: None,
            };
            let created = creds.call(|c| {
                let request = request.clone();
                async move { c.create_meeting(&request).await }
            });
            created_message(&created.await.map_err(readable)?.meeting_id)
        }
        MeetingCommand::List(args) => {
            let (limit, offset) = (args.limit, args.offset);
            let listed = creds.call(|c| async move { c.list_meetings(limit, offset).await });
            list_message(&listed.await.map_err(readable)?)
        }
    };
    println!("{output}");
    Ok(())
}

/// The server's own message rather than the raw JSON body, when there is one.
fn readable(error: AuthError) -> anyhow::Error {
    match &error {
        AuthError::Api(e) => e.api_error().map(|server| anyhow::anyhow!(server.message)),
        _ => None,
    }
    .unwrap_or_else(|| error.into())
}

fn created_message(id: &str) -> String {
    format!(
        "Created meeting {id}. Start it and stream to it with:\n\n    \
         videocall-cli stream --meeting-id {id} --video-device-index <INDEX>"
    )
}

fn list_message(listed: &ListMeetingsResponse) -> String {
    if listed.total == 0 {
        return "No meetings yet. Create one with `videocall-cli meeting create`.".into();
    }
    if listed.meetings.is_empty() {
        return format!(
            "No meetings at offset {}; you have {}.",
            listed.offset, listed.total
        );
    }
    let format = time::format_description::parse("[year]-[month]-[day] [hour]:[minute] UTC")
        .expect("valid format description");
    // Ids from browser auto-create are not validated, so drop terminal escapes.
    let printable = |s: &str| s.chars().filter(|c| !c.is_control()).collect::<String>();
    let mut rows = vec![["MEETING", "STATE", "IN", "WAITING", "CREATED"].map(String::from)];
    for m in &listed.meetings {
        let created = time::OffsetDateTime::from_unix_timestamp(m.created_at).ok();
        let created = created
            .and_then(|t| t.format(&format).ok())
            .unwrap_or_default();
        let (admitted, waiting) = (m.participant_count.to_string(), m.waiting_count.to_string());
        rows.push([
            printable(&m.meeting_id),
            printable(&m.state),
            admitted,
            waiting,
            created,
        ]);
    }
    let width = |i: usize| rows.iter().map(|r| r[i].len()).max().unwrap_or(0);
    let (w0, w1, w2, w3) = (width(0), width(1), width(2), width(3));
    let mut out = String::new();
    for [id, state, admitted, waiting, created] in &rows {
        let _ = writeln!(
            out,
            "{id:<w0$}  {state:<w1$}  {admitted:>w2$}  {waiting:>w3$}  {created}"
        );
    }
    let last = listed.offset + listed.meetings.len() as i64;
    let _ = write!(
        out,
        "\nShowing {}-{last} of {}.",
        listed.offset + 1,
        listed.total
    );
    if last < listed.total {
        let _ = write!(out, " Next page: --offset {last}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli_args::{Mode, Opt};
    use clap::Parser;

    #[test]
    fn parses_meeting_list() {
        let parse = |args: &str| match Opt::try_parse_from(args.split(' ')).map(|o| o.mode) {
            Ok(Mode::Meeting(MeetingCommand::List(list))) => Some((list.limit, list.offset)),
            _ => None,
        };
        assert_eq!(parse("cli meeting list"), Some((20, 0)));
        assert_eq!(parse("cli meeting list --limit 101"), None);
        assert_eq!(parse("cli meeting list --offset -1"), None);
    }

    #[test]
    fn formats_list_and_created() {
        let summary = |id: &str, state: &str, created_at: i64, admitted: i64, waiting: i64| {
            serde_json::json!({
                "meeting_id": id, "host": null, "state": state, "has_password": false,
                "created_at": created_at, "participant_count": admitted,
                "started_at": created_at, "ended_at": null, "waiting_count": waiting,
                "waiting_room_enabled": true,
            })
        };
        let listed = serde_json::json!({
            "meetings": [
                summary("standup-with-a-long-id", "active", 1_795_000_000, 3, 12),
                summary("a\u{1b}[2Jbc", "idle", 1_764_324_000, 0, 0),
            ],
            "total": 45, "limit": 20, "offset": 0,
        });
        assert_eq!(
            list_message(&serde_json::from_value(listed.clone()).unwrap()),
            "MEETING                 STATE   IN  WAITING  CREATED\n\
             standup-with-a-long-id  active   3       12  2026-11-18 11:06 UTC\n\
             a[2Jbc                  idle     0        0  2025-11-28 10:00 UTC\n\
             \n\
             Showing 1-2 of 45. Next page: --offset 2"
        );
        let page = |offset: i64, count: usize| {
            let mut page = listed.clone();
            page["offset"] = offset.into();
            page["meetings"] = page["meetings"].as_array().unwrap()[..count]
                .to_vec()
                .into();
            list_message(&serde_json::from_value(page).unwrap())
        };
        assert_eq!(page(50, 0), "No meetings at offset 50; you have 45.");
        assert!(page(43, 2).ends_with("Showing 44-45 of 45."));
        assert!(created_message("abc").contains("stream --meeting-id abc --video-device-index"));
    }
}
