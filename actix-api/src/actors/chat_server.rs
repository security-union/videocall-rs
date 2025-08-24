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

use crate::messages::{
    server::{ClientMessage, Connect, Disconnect, JoinRoom, Leave},
    session::Message,
};
use crate::models::build_subject_and_queue;

use actix::{Actor, AsyncContext, Context, Handler, MessageResult, Recipient};
use futures::StreamExt;
use std::collections::HashMap;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, trace};

use super::chat_session::SessionId;

pub struct ChatServer {
    nats_connection: async_nats::client::Client,
    sessions: HashMap<SessionId, Recipient<Message>>,
    active_subs: HashMap<SessionId, JoinHandle<()>>,
}

impl ChatServer {
    pub async fn new(nats_connection: async_nats::client::Client) -> Self {
        ChatServer {
            nats_connection,
            active_subs: HashMap::new(),
            sessions: HashMap::new(),
        }
    }

    pub fn leave_rooms(&mut self, session_id: &SessionId) {
        if let Some(task) = self.active_subs.remove(session_id) {
            task.abort();
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
        self.leave_rooms(&session);
        let _ = self.sessions.remove(&session);
    }
}

impl Handler<Leave> for ChatServer {
    type Result = ();

    fn handle(&mut self, Leave { session }: Leave, _ctx: &mut Self::Context) -> Self::Result {
        self.leave_rooms(&session);
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
        self.leave_rooms(&session);

        let (subject, queue) = build_subject_and_queue(&room, &session);
        let session_recipient = match self.sessions.get(&session) {
            Some(recipient) => recipient.clone(),
            None => {
                let err = format!("session {session} is not connected");
                error!("{err}");
                return MessageResult(Err(err));
            }
        };

        let nc = self.nats_connection.clone();
        let session_2 = session.clone();
        let task = actix::spawn(async move {
            match nc
                .queue_subscribe(subject.clone(), queue.clone())
                .await
                .map_err(|e| handle_subscription_error(e, &subject))
            {
                Ok(mut sub) => {
                    debug!("Subscribed to subject {subject} with queue {queue}");
                    info!(
                        "someone connected to room {room} with session {}",
                        session_2.trim(),
                    );
                    while let Some(msg) = sub.next().await {
                        if let Err(e) =
                            handle_msg(session_recipient.clone(), room.clone(), session_2.clone())(
                                msg,
                            )
                        {
                            error!("{}", e);
                        }
                    }
                }
                Err(e) => {
                    error!("{}", e);
                }
            }
        });

        self.active_subs.insert(session, task);

        MessageResult(Ok(()))
    }
}

fn handle_subscription_error(e: impl std::fmt::Display, subject: &str) -> String {
    let err = format!("error subscribing to subject {subject}: {e}");
    error!("{err}");
    err
}

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
