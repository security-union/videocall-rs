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

use crate::{
    messages::{
        server::{ClientMessage, Connect, Disconnect, JoinRoom, Leave},
        session::Message,
    },
    models::build_subject_and_queue,
    session_manager::{SessionEndResult, SessionManager},
};

use actix::{Actor, AsyncContext, Context, Handler, MessageResult, Recipient};
use futures::StreamExt;
use protobuf::Message as ProtoMessage;
use sqlx::PgPool;
use std::collections::HashMap;
use tokio::task::JoinHandle;
use tracing::{error, info, trace};
use videocall_types::protos::packet_wrapper::{packet_wrapper::PacketType, PacketWrapper};
use videocall_types::SYSTEM_USER_EMAIL;

use super::chat_session::SessionId;

pub struct ChatServer {
    nats_connection: async_nats::client::Client,
    sessions: HashMap<SessionId, Recipient<Message>>,
    active_subs: HashMap<SessionId, JoinHandle<()>>,
    session_manager: SessionManager,
}

impl ChatServer {
    pub async fn new(nats_connection: async_nats::client::Client, pool: PgPool) -> Self {
        ChatServer {
            nats_connection,
            active_subs: HashMap::new(),
            sessions: HashMap::new(),
            session_manager: SessionManager::new(pool),
        }
    }

    pub fn leave_rooms(
        &mut self,
        session_id: &SessionId,
        room: Option<&str>,
        user_id: Option<&str>,
    ) {
        // Remove the subscription task if it exists
        if let Some(task) = self.active_subs.remove(session_id) {
            task.abort();
        }

        // End session using SessionManager
        if let (Some(room_id), Some(uid)) = (room, user_id) {
            let room_id = room_id.to_string();
            let user_id = uid.to_string();
            let session_manager = self.session_manager.clone();
            let nc = self.nats_connection.clone();

            tokio::spawn(async move {
                match session_manager.end_session(&room_id, &user_id).await {
                    Ok(SessionEndResult::HostEndedMeeting) => {
                        info!(
                            "Host {} left room {} - ending meeting for all",
                            user_id, room_id
                        );
                        // Notify all participants using MEETING packet (protobuf)
                        let bytes = SessionManager::build_meeting_ended_packet(
                            &room_id,
                            "The host has ended the meeting",
                        );
                        let subject = format!("room.{}.system", room_id.replace(' ', "_"));
                        if let Err(e) = nc.publish(subject, bytes.into()).await {
                            error!("Error publishing MEETING_ENDED: {}", e);
                        }
                    }
                    Ok(SessionEndResult::LastParticipantLeft) => {
                        info!("Last participant {} left room {}", user_id, room_id);
                    }
                    Ok(SessionEndResult::MeetingContinues { remaining_count }) => {
                        info!(
                            "Participant {} left room {}, {} remaining",
                            user_id, room_id, remaining_count
                        );
                    }
                    Err(e) => {
                        error!("Error ending session for room {}: {}", room_id, e);
                    }
                }
            });
        }
    }

    /// Get the session manager (for use by chat_session)
    pub fn session_manager(&self) -> &SessionManager {
        &self.session_manager
    }
}

impl Actor for ChatServer {
    type Context = Context<Self>;
}

impl Handler<Connect> for ChatServer {
    type Result = ();

    fn handle(&mut self, msg: Connect, _ctx: &mut Self::Context) -> Self::Result {
        let Connect { id, addr } = msg;
        self.sessions.insert(id, addr);
    }
}

impl Handler<Disconnect> for ChatServer {
    type Result = ();

    fn handle(
        &mut self,
        Disconnect {
            session,
            room,
            user_id,
        }: Disconnect,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        self.leave_rooms(&session, Some(&room), Some(&user_id));
        let _ = self.sessions.remove(&session);
    }
}

impl Handler<Leave> for ChatServer {
    type Result = ();

    fn handle(
        &mut self,
        Leave {
            session,
            room,
            user_id,
        }: Leave,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        self.leave_rooms(&session, Some(&room), Some(&user_id));
    }
}

impl Handler<ClientMessage> for ChatServer {
    type Result = ();

    fn handle(&mut self, msg: ClientMessage, ctx: &mut Self::Context) -> Self::Result {
        let ClientMessage {
            session,
            room,
            msg,
            user: _,
        } = msg;
        trace!("got message in server room {room} session {session}");
        let nc = self.nats_connection.clone();
        let subject = format!("room.{room}.{session}");
        let subject = subject.replace(' ', "_");
        let b = bytes::Bytes::from(msg.data.to_vec());
        let fut = async move {
            match nc.publish(subject.clone(), b).await {
                Ok(_) => trace!("published message to {subject}"),
                Err(e) => error!("error publishing message to {subject}: {e}"),
            }
        };
        let fut = actix::fut::wrap_future::<_, Self>(fut);
        ctx.spawn(fut);
    }
}

impl Handler<JoinRoom> for ChatServer {
    type Result = MessageResult<JoinRoom>;

    fn handle(
        &mut self,
        JoinRoom {
            session,
            room,
            user_id,
        }: JoinRoom,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        if self.active_subs.contains_key(&session) {
            return MessageResult(Ok(()));
        }

        let session_manager = self.session_manager.clone();
        let room_clone = room.clone();
        let user_id_clone = user_id.clone();
        let nc = self.nats_connection.clone();

        let (subject, queue) = build_subject_and_queue(&room, session.as_str());
        let session_recipient = match self.sessions.get(&session) {
            Some(addr) => addr.clone(),
            None => {
                return MessageResult(Err("Session not found".into()));
            }
        };

        let nc2 = self.nats_connection.clone();
        let session_clone = session.clone();

        let handle = tokio::spawn(async move {
            // Start session using SessionManager - await result before subscribing
            match session_manager
                .start_session(&room_clone, &user_id_clone)
                .await
            {
                Ok(result) => {
                    info!(
                        "Session started for room {} (first: {}) at {}",
                        room_clone, result.is_first_participant, result.start_time_ms
                    );
                    send_meeting_info(&nc, &room_clone, result.start_time_ms).await;
                }
                Err(e) => {
                    error!(
                        "Error starting session for room {}: {} - rejecting join",
                        room_clone, e
                    );
                    // Session rejected - don't subscribe, just return
                    return;
                }
            }

            match nc2.queue_subscribe(subject, queue).await {
                Ok(mut sub) => {
                    while let Some(msg) = sub.next().await {
                        if let Err(e) = handle_msg(
                            session_recipient.clone(),
                            room_clone.clone(),
                            session_clone.clone(),
                        )(msg)
                        {
                            error!("Error handling message: {}", e);
                            break;
                        }
                    }
                }
                Err(e) => {
                    error!("{}", e)
                }
            }
        });

        self.active_subs.insert(session, handle);

        MessageResult(Ok(()))
    }
}

async fn send_meeting_info(nc: &async_nats::client::Client, room: &str, start_time_ms: u64) {
    let message = format!("MEETING_INFO:{start_time_ms}");

    let packet = PacketWrapper {
        packet_type: PacketType::CONNECTION.into(),
        email: SYSTEM_USER_EMAIL.to_string(),
        data: message.as_bytes().to_vec(),
        ..Default::default()
    };

    match packet.write_to_bytes() {
        Ok(packet_byte) => {
            let subject = format!("room.{}.system", room.replace(' ', "_"));
            match nc.publish(subject.clone(), packet_byte.into()).await {
                Ok(_) => info!("Sent meeting start time {} to {}", start_time_ms, subject),
                Err(e) => error!("Failed to send meeting info to room {}: {}", room, e),
            }
        }
        Err(e) => error!("Failed to serialize packet: {}", e),
    }
}

fn handle_msg(
    session_recipient: Recipient<Message>,
    room: String,
    session: SessionId,
) -> impl Fn(async_nats::Message) -> Result<(), std::io::Error> {
    move |msg| {
        if msg.subject == format!("room.{room}.{session}").replace(' ', "_").into() {
            return Ok(());
        }
        let message = Message {
            msg: msg.payload.to_vec(),
            session: session.clone(),
        };

        session_recipient.try_send(message).map_err(|e| {
            error!("error sending message to session {}: {}", session, e);
            std::io::Error::other(e)
        })
    }
}
