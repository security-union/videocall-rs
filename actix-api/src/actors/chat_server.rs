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
    meeting::MeetingManager,
    messages::{
        server::{ClientMessage, Connect, Disconnect, JoinRoom, Leave},
        session::Message,
    },
    models::build_subject_and_queue,
};

use actix::{Actor, AsyncContext, Context, Handler, MessageResult, Recipient};
use futures::StreamExt;
use protobuf::Message as ProtoMessage;
use std::collections::HashMap;
use tokio::task::JoinHandle;
use tracing::{error, info, trace};
use videocall_types::protos::packet_wrapper::{packet_wrapper::PacketType, PacketWrapper};

use super::chat_session::SessionId;

pub struct ChatServer {
    nats_connection: async_nats::client::Client,
    sessions: HashMap<SessionId, Recipient<Message>>,
    active_subs: HashMap<SessionId, JoinHandle<()>>,
    room_participants: HashMap<String, usize>,
    meeting_manager: MeetingManager,
}

impl ChatServer {
    pub async fn new(nats_connection: async_nats::client::Client) -> Self {
        ChatServer {
            nats_connection,
            active_subs: HashMap::new(),
            sessions: HashMap::new(),
            room_participants: HashMap::new(),
            meeting_manager: MeetingManager::new(),
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

        // Get the total subscribers from the NATS server
        // TODO: Get the total subscribers from the NATS server, to know the total number of participants in the room
        // let total_subscribers = self.nats_connection.total_subscribers().await;

        // Update participant count and end meeting if needed
        if let Some(room_id) = room {
            if let Some(count) = self.room_participants.get_mut(room_id) {
                if *count > 0 {
                    *count -= 1;
                }

                if let Some(uid) = user_id {
                    let room_id_clone = room_id.to_string();
                    let user_id_clone = uid.to_string();
                    let manager = self.meeting_manager.clone();
                    let nc = self.nats_connection.clone();
                    let remaining_count = *count;

                    tokio::spawn(async move {
                        let is_creator = manager.is_creator(&room_id_clone, &user_id_clone).await;

                        if is_creator && remaining_count > 0 {
                            info!(
                                "Creator {} leaving room {} - ending meeting for all",
                                user_id_clone, room_id_clone
                            );
                            match manager.end_meeting(&room_id_clone).await {
                                Ok(_) => {
                                    let meeting_ended_msg = serde_json::json!({
                                        "type": "meeting_ended",
                                        "room_id": room_id_clone,
                                        "message": "The host has ended the meeting"
                                    });

                                    let subject =
                                        format!("room.{}", room_id_clone.replace(' ', "_"));
                                    if let Err(e) = nc
                                        .publish(
                                            subject.clone(),
                                            bytes::Bytes::from(
                                                serde_json::to_vec(&meeting_ended_msg).unwrap(),
                                            ),
                                        )
                                        .await
                                    {
                                        error!("Error publishing meeting ended message: {}", e);
                                    }
                                }
                                Err(e) => {
                                    error!("Error ending meeting for room {}: {}", room_id_clone, e)
                                }
                            }

                            let message = "MEETING_ENDED:The host has ended the meeting";

                            let packet = PacketWrapper {
                                packet_type: PacketType::CONNECTION.into(),
                                email: "system".to_string(),
                                data: message.as_bytes().to_vec(),
                                ..Default::default()
                            };

                            match packet.write_to_bytes() {
                                Ok(packet_bytes) => {
                                    //let subject = format!("room.{}", room_id_clone);
                                    let subject =
                                        format!("room.{}.system", room_id_clone.replace(' ', "_"));
                                    match nc.publish(subject.clone(), packet_bytes.into()).await {
                                        Ok(_) => {
                                            info!(
                                                "Successfully published MEETING_ENDED to {}",
                                                subject
                                            );
                                        }
                                        Err(e) => {
                                            error!(
                                                "Failed to publish MEETING_ENDED to {}: {}",
                                                subject, e
                                            );
                                        }
                                    }
                                }
                                Err(e) => {
                                    error!("Failed to serialize MEETING_ENDED packet: {}", e);
                                }
                            }
                        } else if remaining_count == 0 {
                            info!("Last participant left room {}", room_id_clone);
                            match manager.end_meeting(&room_id_clone).await {
                                Ok(_) => info!("Meeting ended for room {}", room_id_clone),
                                Err(e) => {
                                    error!("Error ending meeting for room {}: {}", room_id_clone, e)
                                }
                            }
                        }
                    });
                } else if *count == 0 {
                    self.room_participants.remove(room_id);
                    let room_id_clone = room_id.to_string();
                    let manager = self.meeting_manager.clone();
                    info!("Last participant left room {}", room_id_clone);
                    tokio::spawn(async move {
                        match manager.end_meeting(&room_id_clone).await {
                            Ok(_) => info!("Meeting ended for room {}", room_id_clone),
                            Err(e) => {
                                error!("Error ending meeting for room {}: {}", room_id_clone, e)
                            }
                        }
                    });
                } else {
                    info!(
                        "No participant count found for room {} (session: {})",
                        room_id, session_id
                    );
                }
            }
        }
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
        Disconnect { session }: Disconnect,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        // Find the room this session is in
        let room = self.active_subs.iter().find_map(|(s, _)| {
            if s == &session {
                // Extract room from the subscription key if needed
                // For now, we'll just return None since we can't determine the room
                None
            } else {
                None
            }
        });

        //self.leave_rooms(&session, room);
        self.leave_rooms(&session, room, None);
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

        let count = self.room_participants.entry(room.clone()).or_insert(0);
        let is_first_participant = *count == 0;
        *count += 1;

        let manager = self.meeting_manager.clone();
        let room_clone = room.clone();
        let user_id_clone = user_id.clone();
        let nc = self.nats_connection.clone();

        let send_info_task = tokio::spawn(async move {
            if is_first_participant {
                match manager.start_meeting(&room_clone, &user_id_clone).await {
                    Ok(start_time) => {
                        info!(
                            "Meeting started for room {} at {} by creator {}",
                            room_clone, start_time, user_id_clone
                        );

                        send_meeting_info(&nc, &room_clone, start_time).await;
                    }
                    Err(e) => {
                        error!("Error starting meeting for room {}: {}", room_clone, e);
                    }
                }
            } else {
                match manager.get_meeting_start_time(&room_clone).await {
                    Ok(Some(start_time)) => {
                        info!("Meeting info for room {}", room_clone);

                        send_meeting_info(&nc, &room_clone, start_time as u64).await;
                    }
                    Ok(None) => {
                        error!("Meeting {} exists but has no start time", room_clone);
                    }
                    Err(e) => {
                        error!(
                            "Error getting meeting start time for room {}: {}",
                            room_clone, e
                        );
                    }
                }
            }
        });

        let (subject, queue) = build_subject_and_queue(&room, session.as_str());
        let session_recipient = match self.sessions.get(&session) {
            Some(addr) => addr.clone(),
            None => {
                if let Some(count) = self.room_participants.get_mut(&room) {
                    *count -= 1;
                    if *count == 0 {
                        self.room_participants.remove(&room);
                    }
                }

                return MessageResult(Err("Session not found".into()));
            }
        };

        let nc = self.nats_connection.clone();
        let session_clone = session.clone();
        let room_clone = room.clone();

        let handle = tokio::spawn(async move {
            match nc.queue_subscribe(subject, queue).await {
                Ok(mut sub) => {
                    drop(send_info_task);
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
    let message = format!("MEETING_INFO:{}", start_time_ms);

    let packet = PacketWrapper {
        packet_type: PacketType::CONNECTION.into(),
        email: "system".to_string(),
        data: message.as_bytes().to_vec(),
        ..Default::default()
    };

    match packet.write_to_bytes() {
        Ok(packet_byte) => {
            let subject = format!("room.{}.system", room.replace(' ', "_"));
            match nc.publish(subject.clone(), packet_byte.into()).await {
                Ok(_) => info!(
                    "Sent meeting start time {} to {}",
                    start_time_ms, subject
                ),
                Err(e) => error!("Failed to send meeting info to room {}: {}", room, e),
            }
        }
        Err(e) => error!("Failed to serialize packet: {}", e),
    }
}

// fn handle_subscription_error(e: impl std::fmt::Display, subject: &str) -> String {
//     let err = format!("error subscribing to subject {subject}: {e}");
//     error!("{err}");
//     err
// }

fn handle_msg(
    session_recipient: Recipient<Message>, // Assuming Recipient is a type
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
