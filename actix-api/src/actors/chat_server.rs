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
};

use actix::{Actor, AsyncContext, Context, Handler, MessageResult, Recipient};
use futures::StreamExt;
use std::collections::HashMap;
use tokio::task::JoinHandle;
use tracing::{error, info, trace};

use super::chat_session::SessionId;

pub struct ChatServer {
    nats_connection: async_nats::client::Client,
    sessions: HashMap<SessionId, Recipient<Message>>,
    active_subs: HashMap<SessionId, JoinHandle<()>>,
    room_participants: HashMap<String, usize>, // Track number of participants per room
}

impl ChatServer {
    pub async fn new(
        nats_connection: async_nats::client::Client,
    ) -> Self {
        ChatServer {
            nats_connection,
            active_subs: HashMap::new(),
            sessions: HashMap::new(),
            room_participants: HashMap::new(),
        }
    }

    pub fn leave_rooms(&mut self, session_id: &SessionId, room: Option<&str>) {
        // Remove the subscription task
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

                // If no more participants, end the meeting
                if *count == 0 {
                    self.room_participants.remove(room_id);
                    info!("Last participant left room {}", room_id);
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

        self.leave_rooms(&session, room);
        let _ = self.sessions.remove(&session);
    }
}

impl Handler<Leave> for ChatServer {
    type Result = ();

    fn handle(&mut self, Leave { session, room }: Leave, _ctx: &mut Self::Context) -> Self::Result {
        self.leave_rooms(&session, Some(&room));
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
        JoinRoom { session, room }: JoinRoom,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        if self.active_subs.contains_key(&session) {
            return MessageResult(Ok(()));
        }

        *self.room_participants.entry(room.clone()).or_insert(0) += 1;

        let (subject, queue) = build_subject_and_queue(&room, &session);
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
        };

        session_recipient.try_send(message).map_err(|e| {
            error!("error sending message to session {}: {}", session, e);
            std::io::Error::other(e)
        })
    }
}
