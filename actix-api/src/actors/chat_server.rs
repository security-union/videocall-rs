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
    constants::RECONNECT_GRACE_PERIOD,
    messages::{
        server::{ActivateConnection, ClientMessage, Connect, Disconnect, JoinRoom, Leave},
        session::Message,
    },
    models::build_subject_and_queue,
    session_manager::{SessionEndResult, SessionManager},
};

use actix::{
    Actor, AsyncContext, Context, Handler, Message as ActixMessage, MessageResult, Recipient,
    SpawnHandle,
};
use futures::StreamExt;
use protobuf::Message as ProtobufMessage;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::task::JoinHandle;
use tracing::{error, info, trace, warn};

use crate::metrics::{RELAY_NATS_PUBLISH_LATENCY_MS, RELAY_PACKET_DROPS_TOTAL};
use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
use videocall_types::protos::meeting_packet::MeetingPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::validation::validate_display_name;
use videocall_types::SYSTEM_USER_ID;

use super::session_logic::{ConnectionState, SessionId};

/// Internal message sent via `notify_later` after the reconnection grace period
/// expires. If the user has not reconnected by the time this message is handled,
/// the actual `leave_rooms()` + PARTICIPANT_LEFT flow executes.
#[derive(ActixMessage)]
#[rtype(result = "()")]
struct ExecutePendingDeparture {
    session: SessionId,
    room: String,
    user_id: String,
    display_name: String,
    is_host: bool,
    end_on_host_leave: bool,
}

/// NATS subject for cross-server stale session eviction.
const EVICT_INSTANCE_SUBJECT: &str = "internal.evict_instance";

/// Payload published to NATS for cross-server stale session eviction.
/// When a client reconnects (possibly to a different server), the new server
/// broadcasts this so the old server can clean up silently.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
struct EvictInstancePayload {
    instance_id: String,
    room: String,
    user_id: String,
    new_session_id: SessionId,
}

/// Internal actix message delivered when a NATS eviction message is received.
#[derive(ActixMessage)]
#[rtype(result = "()")]
struct EvictInstance(EvictInstancePayload);

/// Internal actix message to update a room member's display name.
/// Sent from the per-session NATS subscription loop when a
/// PARTICIPANT_DISPLAY_NAME_CHANGED event is received.
#[derive(ActixMessage)]
#[rtype(result = "()")]
struct UpdateMemberDisplayName {
    room_id: String,
    user_id: String,
    display_name: String,
}

/// State stored while a departure is pending (waiting for possible reconnection).
struct PendingDepartureState {
    /// Handle returned by `ctx.notify_later()`, used to cancel the delayed
    /// `ExecutePendingDeparture` message if the user reconnects in time.
    spawn_handle: SpawnHandle,
    /// The old session ID that disconnected — used for cleanup.
    old_session: SessionId,
    /// Whether the disconnecting session had been activated (Testing -> Active).
    /// Only Active sessions should trigger PARTICIPANT_LEFT, because only Active
    /// sessions had their PARTICIPANT_JOINED broadcast. Testing sessions (e.g.,
    /// the losing connection during RTT election) never announced themselves.
    was_active: bool,
    /// Whether the disconnecting session was the meeting host.
    is_host: bool,
    /// Whether the meeting should end when the host leaves.
    end_on_host_leave: bool,
}

/// Information about a room member tracked by the ChatServer.
#[derive(Clone, Debug)]
struct RoomMemberInfo {
    session: SessionId,
    user_id: String,
    display_name: String,
    is_host: bool,
    end_on_host_leave: bool,
}

pub struct ChatServer {
    nats_connection: async_nats::client::Client,
    sessions: HashMap<SessionId, Recipient<Message>>,
    active_subs: HashMap<SessionId, JoinHandle<()>>,
    session_manager: SessionManager,
    connection_states: HashMap<SessionId, ConnectionState>,
    /// Track which sessions are in which room, with their user_id, display_name,
    /// and host status. Used to send PARTICIPANT_JOINED for existing peers to
    /// new joiners and to determine host-leave behavior.
    room_members: HashMap<String, Vec<RoomMemberInfo>>,
    /// Pending departures keyed by `(room_id, user_id)`. When a session disconnects
    /// we defer the PARTICIPANT_LEFT broadcast by [`RECONNECT_GRACE_PERIOD`]. If the
    /// same user reconnects before the timer fires, the departure is cancelled
    /// silently — no PARTICIPANT_LEFT or PARTICIPANT_JOINED is sent.
    pending_departures: HashMap<(String, String), PendingDepartureState>,
    /// Sessions that should NOT have PARTICIPANT_JOINED broadcast at activation.
    /// This is used for reconnection sessions: the user never "left" from peers'
    /// perspective, so announcing a "join" would be misleading.
    suppress_join_broadcast: std::collections::HashSet<SessionId>,
    /// Maps `instance_id` → `SessionId` for the current active session of each
    /// client instance. Used to find and evict stale sessions on reconnection.
    instance_index: HashMap<String, SessionId>,
    /// Reverse map: `SessionId` → `instance_id`. Enables O(1) cleanup of
    /// `instance_index` when a session disconnects, instead of an O(n) retain scan.
    session_instance: HashMap<SessionId, String>,
    /// Per-session server-authoritative guest flag, sourced from the JWT
    /// `is_guest` claim captured at `JoinRoom`.
    session_is_guest: HashMap<SessionId, bool>,
}

impl ChatServer {
    pub async fn new(nats_connection: async_nats::client::Client) -> Self {
        ChatServer {
            nats_connection,
            active_subs: HashMap::new(),
            sessions: HashMap::new(),
            session_manager: SessionManager::new(),
            connection_states: HashMap::new(),
            room_members: HashMap::new(),
            pending_departures: HashMap::new(),
            suppress_join_broadcast: std::collections::HashSet::new(),
            instance_index: HashMap::new(),
            session_instance: HashMap::new(),
            session_is_guest: HashMap::new(),
        }
    }

    pub fn leave_rooms(
        &mut self,
        session_id: &SessionId,
        room: Option<&str>,
        user_id: Option<&str>,
        display_name: Option<&str>,
        observer: bool,
        is_host: bool,
        end_on_host_leave: bool,
    ) {
        // Remove the subscription task if it exists
        if let Some(task) = self.active_subs.remove(session_id) {
            task.abort();
        }

        // Clean up instance_index via reverse map: O(1) instead of O(n) retain.
        // If the entry was already replaced by a newer session (eviction), the
        // reverse map was already updated, so this is a no-op.
        if let Some(iid) = self.session_instance.remove(session_id) {
            // Only remove from instance_index if it still points to this session.
            if self.instance_index.get(&iid) == Some(session_id) {
                self.instance_index.remove(&iid);
            }
        }

        // Remove from room_members tracking
        if let Some(room_id) = room {
            if let Some(members) = self.room_members.get_mut(room_id) {
                members.retain(|m| m.session != *session_id);
                if members.is_empty() {
                    self.room_members.remove(room_id);
                }
            }
        }

        // End session using SessionManager
        if let (Some(room_id), Some(uid)) = (room, user_id) {
            let room_id = room_id.to_string();
            let user_id = uid.to_string();
            let display_name = display_name.unwrap_or(uid).to_string();
            let is_guest = self.session_is_guest.remove(session_id).unwrap_or(false);
            let session_manager = self.session_manager.clone();
            let nc = self.nats_connection.clone();
            let session_id_val = *session_id;

            // Observer sessions (waiting room) should not publish PARTICIPANT_LEFT
            // since they were never real participants in the meeting.
            if observer {
                info!(
                    "Observer session {} for {} leaving room {} - skipping PARTICIPANT_LEFT",
                    session_id_val, user_id, room_id
                );
                tokio::spawn(async move {
                    if let Err(e) = session_manager.end_session(&room_id, &user_id).await {
                        error!("Error ending observer session for room {}: {}", room_id, e);
                    }
                });
                return;
            }

            if let Some(state) = self.connection_states.get(session_id) {
                if *state != ConnectionState::Active {
                    info!(
                        "Skipping PARTICIPANT_LEFT for non-active session {}",
                        session_id
                    );
                    return;
                }
            }

            tokio::spawn(async move {
                // Check host-leave behavior first: if the host is leaving and
                // end_on_host_leave is set, end the meeting for all participants.
                if is_host && end_on_host_leave {
                    info!(
                        "Host {} left room {} - ending meeting for all",
                        user_id, room_id
                    );
                    let subject = format!("room.{}.system", room_id.replace(' ', "_"));
                    // First emit PARTICIPANT_LEFT so clients remove the host's video tile
                    // before the MEETING_ENDED overlay renders. Without this, the host's
                    // tile remains as a ghost until teardown ordering resolves it.
                    let left_bytes = SessionManager::build_peer_left_packet(
                        &room_id,
                        &user_id,
                        session_id_val,
                        &display_name,
                        is_guest,
                    );
                    if let Err(e) = nc.publish(subject.clone(), left_bytes.into()).await {
                        error!("Error publishing PARTICIPANT_LEFT for host: {}", e);
                    }
                    // Then end the meeting for all remaining participants.
                    let ended_bytes = SessionManager::build_meeting_ended_packet(
                        &room_id,
                        "The host has ended the meeting",
                    );
                    if let Err(e) = nc.publish(subject, ended_bytes.into()).await {
                        error!("Error publishing MEETING_ENDED: {}", e);
                    }
                    if let Err(e) = session_manager.end_session(&room_id, &user_id).await {
                        error!("Error ending host session for room {}: {}", room_id, e);
                    }
                } else {
                    // Normal participant departure
                    match session_manager.end_session(&room_id, &user_id).await {
                        Ok(SessionEndResult::HostEndedMeeting) => {
                            // SessionManager indicated host ended meeting
                            // (future-proofing for server-side tracking)
                            info!(
                                "Host {} left room {} - ending meeting for all (via SessionManager)",
                                user_id, room_id
                            );
                            let subject = format!("room.{}.system", room_id.replace(' ', "_"));
                            // Emit PARTICIPANT_LEFT first so clients clean up the host's tile.
                            let left_bytes = SessionManager::build_peer_left_packet(
                                &room_id,
                                &user_id,
                                session_id_val,
                                &display_name,
                                is_guest,
                            );
                            if let Err(e) = nc.publish(subject.clone(), left_bytes.into()).await {
                                error!("Error publishing PARTICIPANT_LEFT for host: {}", e);
                            }
                            let ended_bytes = SessionManager::build_meeting_ended_packet(
                                &room_id,
                                "The host has ended the meeting",
                            );
                            if let Err(e) = nc.publish(subject, ended_bytes.into()).await {
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
                            // Notify remaining peers about the departed session
                            let bytes = SessionManager::build_peer_left_packet(
                                &room_id,
                                &user_id,
                                session_id_val,
                                &display_name,
                                is_guest,
                            );
                            let subject = format!("room.{}.system", room_id.replace(' ', "_"));
                            if let Err(e) = nc.publish(subject, bytes.into()).await {
                                error!("Error publishing PARTICIPANT_LEFT: {}", e);
                            }
                        }
                        Err(e) => {
                            error!("Error ending session for room {}: {}", room_id, e);
                        }
                    }
                }
            });
        }
    }

    /// Get the session manager (for use by chat_session)
    pub fn session_manager(&self) -> &SessionManager {
        &self.session_manager
    }

    /// Evict a stale session for the given `instance_id`, if present locally.
    ///
    /// Returns `true` if a session was evicted.
    /// `skip_session_id` is the new session's ID — used as a self-delivery guard
    /// to prevent the publishing server from double-evicting.
    fn evict_stale_session(
        &mut self,
        instance_id: &str,
        room: &str,
        user_id: &str,
        skip_session_id: SessionId,
        ctx: &mut Context<Self>,
    ) -> bool {
        let prev_sid = match self.instance_index.get(instance_id) {
            Some(&sid) => sid,
            None => return false,
        };

        if prev_sid == skip_session_id {
            return false;
        }

        let user_matches = self
            .room_members
            .get(room)
            .map(|members| {
                members
                    .iter()
                    .any(|m| m.session == prev_sid && m.user_id == user_id)
            })
            .unwrap_or(false);

        if !user_matches {
            return false;
        }

        info!(
            "Evicting stale session {} for instance {} (user {} in room {}) \
             in favour of session {}",
            prev_sid, instance_id, user_id, room, skip_session_id
        );

        if let Some(members) = self.room_members.get_mut(room) {
            members.retain(|m| m.session != prev_sid);
        }

        if let Some(task) = self.active_subs.remove(&prev_sid) {
            task.abort();
        }

        let _ = self.sessions.remove(&prev_sid);
        let _ = self.connection_states.remove(&prev_sid);
        let _ = self.suppress_join_broadcast.remove(&prev_sid);

        let departure_key = (room.to_string(), user_id.to_string());
        if let Some(pending) = self.pending_departures.remove(&departure_key) {
            ctx.cancel_future(pending.spawn_handle);
        }

        self.instance_index.remove(instance_id);
        self.session_instance.remove(&prev_sid);

        true
    }
}

impl Actor for ChatServer {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        info!(
            "ChatServer started — subscribing to {}",
            EVICT_INSTANCE_SUBJECT
        );

        let nc = self.nats_connection.clone();
        let addr = ctx.address();

        tokio::spawn(async move {
            loop {
                match nc.subscribe(EVICT_INSTANCE_SUBJECT).await {
                    Ok(mut sub) => {
                        while let Some(msg) = sub.next().await {
                            match serde_json::from_slice::<EvictInstancePayload>(&msg.payload) {
                                Ok(payload) => {
                                    addr.do_send(EvictInstance(payload));
                                }
                                Err(e) => {
                                    warn!("Failed to deserialize evict_instance payload: {}", e);
                                }
                            }
                        }
                        warn!(
                            "{} subscription stream ended, re-subscribing in 1s",
                            EVICT_INSTANCE_SUBJECT
                        );
                    }
                    Err(e) => {
                        error!(
                            "Failed to subscribe to {}: {}, retrying in 1s",
                            EVICT_INSTANCE_SUBJECT, e
                        );
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        });
    }
}

impl Handler<Connect> for ChatServer {
    type Result = ();

    fn handle(&mut self, msg: Connect, _ctx: &mut Self::Context) -> Self::Result {
        let Connect { id, addr } = msg;
        self.sessions.insert(id, addr);
        self.connection_states.insert(id, ConnectionState::Testing);
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
            display_name,
            is_guest,
            observer,
            is_host,
            end_on_host_leave,
        }: Disconnect,
        ctx: &mut Self::Context,
    ) -> Self::Result {
        // Persist the authoritative guest flag so leave_rooms can populate
        // PARTICIPANT_LEFT consistently even if the original JoinRoom entry
        // was evicted. `is_guest` for a session never changes mid-lifetime.
        self.session_is_guest.entry(session).or_insert(is_guest);
        // If the session was already evicted (by a reconnecting instance_id),
        // its entries in sessions/connection_states were removed during eviction.
        // Ignore the stale Disconnect so it doesn't clobber a newer session's
        // pending departure for the same (room, user_id) key.
        let was_present = self.sessions.remove(&session).is_some();
        if !was_present {
            info!(
                "Disconnect for already-evicted session {} (user {} in room {}) — ignoring",
                session, user_id, room
            );
            return;
        }

        // Clean up session-level state immediately — the transport is gone.
        // Capture whether the session was Active before removing the state,
        // so we can store it in PendingDepartureState for the grace period.
        let was_active = self
            .connection_states
            .get(&session)
            .map(|s| *s == ConnectionState::Active)
            .unwrap_or(false);
        let _ = self.connection_states.remove(&session);
        let _ = self.suppress_join_broadcast.remove(&session);

        // Observers and non-active sessions bypass the grace period — they
        // never triggered PARTICIPANT_JOINED, so there is nothing to defer.
        if observer {
            self.leave_rooms(
                &session,
                Some(&room),
                Some(&user_id),
                Some(&display_name),
                true,
                false,
                true,
            );
            return;
        }

        // Remove the NATS subscription task immediately so the old session
        // stops receiving media. Keep room_members intact for now — they
        // will be cleaned up either on reconnection or when the grace
        // period expires.
        if let Some(task) = self.active_subs.remove(&session) {
            task.abort();
        }

        // If there is already a pending departure for this (room, user_id),
        // cancel the old timer and replace it. This handles the edge case of
        // rapid disconnect-reconnect-disconnect cycles.
        //
        // BUG FIX (introduced by 0844f062 / batch merge of PRs #793 et al.):
        // The original code cancelled the old timer but did NOT remove the
        // replaced session from room_members. During RTT election, N candidate
        // connections all call JoinRoom (adding N room_members entries for the
        // same user_id). When the N-1 losers disconnect in rapid succession,
        // each Disconnect replaces the previous pending departure — but only
        // the *last* replacement's session gets cleaned up when the grace
        // period expires. The earlier sessions become permanent orphans in
        // room_members, appearing as phantom peers that trigger PLI storms
        // and freeze real participants' video.
        let key = (room.clone(), user_id.clone());
        if let Some(old) = self.pending_departures.remove(&key) {
            ctx.cancel_future(old.spawn_handle);
            // Clean up the replaced session's room_members entry to prevent
            // orphaned phantom peers.
            if let Some(members) = self.room_members.get_mut(&room) {
                members.retain(|m| m.session != old.old_session);
            }
            info!(
                "Replaced existing pending departure for user {} in room {} (old session {})",
                user_id, room, old.old_session
            );
        }

        info!(
            "Deferring PARTICIPANT_LEFT for user {} (session {}) in room {} — \
             grace period {:?}",
            user_id, session, room, RECONNECT_GRACE_PERIOD
        );

        let handle = ctx.notify_later(
            ExecutePendingDeparture {
                session,
                room: room.clone(),
                user_id: user_id.clone(),
                display_name,
                is_host,
                end_on_host_leave,
            },
            RECONNECT_GRACE_PERIOD,
        );

        self.pending_departures.insert(
            key,
            PendingDepartureState {
                spawn_handle: handle,
                old_session: session,
                was_active,
                is_host,
                end_on_host_leave,
            },
        );
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
        // Cancel any pending departure for this (room, user_id) to avoid a
        // duplicate PARTICIPANT_LEFT when the grace-period timer fires later.
        // We don't need ctx.cancel_future() because ExecutePendingDeparture::handle
        // already checks whether the entry exists in pending_departures — once
        // removed, the timer becomes a no-op.
        let key = (room.clone(), user_id.clone());
        if self.pending_departures.remove(&key).is_some() {
            info!(
                "Cancelled pending departure for user {} in room {} — explicit Leave received",
                user_id, room
            );
        }

        // Look up is_host, end_on_host_leave, and display_name from room_members.
        // The Leave message carries no host info; we must resolve it from the
        // in-memory member table so the host-leave path in leave_rooms fires
        // correctly when the host explicitly leaves.
        let (is_host, end_on_host_leave, display_name) = self
            .room_members
            .get(&room)
            .and_then(|members| members.iter().find(|m| m.session == session))
            .map(|m| (m.is_host, m.end_on_host_leave, Some(m.display_name.clone())))
            .unwrap_or((false, true, None));

        // Leave is always a real participant, never an observer.
        self.leave_rooms(
            &session,
            Some(&room),
            Some(&user_id),
            display_name.as_deref(),
            false,
            is_host,
            end_on_host_leave,
        );
    }
}

impl Handler<ActivateConnection> for ChatServer {
    type Result = ();

    fn handle(&mut self, msg: ActivateConnection, ctx: &mut Self::Context) -> Self::Result {
        let ActivateConnection { session } = msg;
        let was_testing = if let Some(state) = self.connection_states.get_mut(&session) {
            if *state == ConnectionState::Testing {
                *state = ConnectionState::Active;
                info!("Session {} activated (Testing -> Active)", session);
                true
            } else {
                false
            }
        } else {
            self.connection_states
                .insert(session, ConnectionState::Active);
            info!(
                "Session {} activated (state was missing, created as Active)",
                session
            );
            // Treat missing state as a Testing -> Active transition so we
            // still broadcast PARTICIPANT_JOINED.
            true
        };

        // --- Cross-server eviction broadcast ---
        // Deferred from JoinRoom to here so that only the elected connection
        // (the winner of RTT election) publishes. Testing connections that
        // lose the election never trigger a NATS eviction message.
        if was_testing {
            if let Some(iid) = self.session_instance.get(&session).cloned() {
                // Look up room and user_id from room_members.
                let mut room_user: Option<(String, String)> = None;
                for (room_id, members) in &self.room_members {
                    for m in members {
                        if m.session == session {
                            room_user = Some((room_id.clone(), m.user_id.clone()));
                            break;
                        }
                    }
                    if room_user.is_some() {
                        break;
                    }
                }
                if let Some((room_id, user_id)) = room_user {
                    let payload = EvictInstancePayload {
                        instance_id: iid,
                        room: room_id,
                        user_id,
                        new_session_id: session,
                    };
                    match serde_json::to_vec(&payload) {
                        Ok(json) => {
                            let nc = self.nats_connection.clone();
                            let fut = async move {
                                if let Err(e) =
                                    nc.publish(EVICT_INSTANCE_SUBJECT, json.into()).await
                                {
                                    error!("Failed to publish eviction to NATS: {}", e);
                                }
                            };
                            let fut = actix::fut::wrap_future::<_, Self>(fut);
                            ctx.spawn(fut);
                        }
                        Err(e) => {
                            error!("Failed to serialize EvictInstancePayload: {}", e);
                        }
                    }
                }
            }
        }

        // Broadcast PARTICIPANT_JOINED now that this connection is confirmed
        // as the elected/active one. During JoinRoom, the broadcast was deferred
        // to avoid ghost join events from Testing connections (e.g., the losing
        // connection during RTT election).
        //
        // Skip the broadcast for sessions marked in suppress_join_broadcast
        // (reconnection sessions and observer sessions).
        let suppressed = self.suppress_join_broadcast.remove(&session);
        if was_testing && !suppressed {
            // Look up the session's room, user_id, and display_name from room_members.
            let mut found: Option<(String, String, String)> = None;
            for (room_id, members) in &self.room_members {
                for m in members {
                    if m.session == session {
                        found = Some((room_id.clone(), m.user_id.clone(), m.display_name.clone()));
                        break;
                    }
                }
                if found.is_some() {
                    break;
                }
            }

            if let Some((room_id, user_id, display_name)) = found {
                let is_guest = self
                    .session_is_guest
                    .get(&session)
                    .copied()
                    .unwrap_or(false);
                let bytes = SessionManager::build_peer_joined_packet(
                    &room_id,
                    &user_id,
                    session,
                    &display_name,
                    is_guest,
                );
                let subject = format!("room.{}.system", room_id.replace(' ', "_"));
                info!(
                    "Publishing deferred PARTICIPANT_JOINED for {} (display={}, session={}) to {}",
                    user_id, display_name, session, subject
                );
                let nc = self.nats_connection.clone();
                let fut = async move {
                    if let Err(e) = nc.publish(subject, bytes.into()).await {
                        error!("Error publishing deferred PARTICIPANT_JOINED: {}", e);
                    }
                };
                let fut = actix::fut::wrap_future::<_, Self>(fut);
                ctx.spawn(fut);
            } else {
                // This can happen for observer sessions (not tracked in room_members)
                // or if the session was cleaned up before activation. Not an error.
                info!(
                    "Session {} activated but not found in room_members — \
                     skipping PARTICIPANT_JOINED (likely observer or already cleaned up)",
                    session
                );
            }
        }
    }
}

/// Handle in-memory display-name updates triggered by NATS
/// PARTICIPANT_DISPLAY_NAME_CHANGED events.
impl Handler<UpdateMemberDisplayName> for ChatServer {
    type Result = ();

    fn handle(&mut self, msg: UpdateMemberDisplayName, _ctx: &mut Self::Context) -> Self::Result {
        let validated_name = match validate_display_name(&msg.display_name) {
            Ok(name) => name,
            Err(e) => {
                warn!(
                    "UpdateMemberDisplayName: rejecting invalid display name from NATS for user {} in room {}: {}",
                    msg.user_id, msg.room_id, e
                );
                return;
            }
        };
        if let Some(members) = self.room_members.get_mut(&msg.room_id) {
            for member in members.iter_mut() {
                if member.user_id == msg.user_id {
                    member.display_name = validated_name.clone();
                }
            }
        }
    }
}

/// Handle cross-server eviction requests received via NATS.
impl Handler<EvictInstance> for ChatServer {
    type Result = ();

    fn handle(&mut self, msg: EvictInstance, ctx: &mut Self::Context) -> Self::Result {
        let EvictInstancePayload {
            instance_id,
            room,
            user_id,
            new_session_id,
        } = msg.0;

        // Validate inbound fields — the JoinRoom path sanitizes instance_id to
        // max 64 chars, but NATS messages come from untrusted peers.
        if instance_id.is_empty()
            || instance_id.len() > 64
            || room.len() > 256
            || user_id.len() > 256
        {
            warn!(
                "Ignoring eviction with invalid field lengths (instance_id={}, room={}, user_id={})",
                instance_id.len(),
                room.len(),
                user_id.len()
            );
            return;
        }

        if self.evict_stale_session(&instance_id, &room, &user_id, new_session_id, ctx) {
            info!(
                "Cross-server eviction completed: instance {} (user {} in room {}) — \
                 new session {} is on another server",
                instance_id, user_id, room, new_session_id
            );
        }
    }
}

/// Handler for deferred departure execution.
/// Runs after [`RECONNECT_GRACE_PERIOD`] unless cancelled by a reconnection.
impl Handler<ExecutePendingDeparture> for ChatServer {
    type Result = ();

    fn handle(
        &mut self,
        ExecutePendingDeparture {
            session,
            room,
            user_id,
            display_name,
            is_host,
            end_on_host_leave,
        }: ExecutePendingDeparture,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        let key = (room.clone(), user_id.clone());

        // Only execute if this departure is still pending. It may have been
        // cancelled by a reconnection or replaced by a newer disconnect.
        if let Some(pending) = self.pending_departures.remove(&key) {
            if pending.old_session != session {
                // A newer disconnect replaced this one — do nothing, the newer
                // timer will handle it.
                info!(
                    "Stale pending departure for user {} in room {} (session {} != {}), skipping",
                    user_id, room, session, pending.old_session
                );
                // Re-insert the newer pending state.
                self.pending_departures.insert(key, pending);
                return;
            }

            // Only broadcast PARTICIPANT_LEFT if the session was Active when it
            // disconnected. Testing sessions (e.g., the losing connection during
            // RTT election) never had PARTICIPANT_JOINED broadcast, so emitting
            // PARTICIPANT_LEFT would cause ghost leave events for other participants.
            if !pending.was_active {
                info!(
                    "Grace period expired for user {} (session {}) in room {} — \
                     skipping PARTICIPANT_LEFT (session was never activated)",
                    user_id, session, room
                );
                // Still clean up room_members and instance_index for the old session.
                if let Some(members) = self.room_members.get_mut(&room) {
                    members.retain(|m| m.session != session);
                    if members.is_empty() {
                        self.room_members.remove(&room);
                    }
                }
                if let Some(iid) = self.session_instance.remove(&session) {
                    if self.instance_index.get(&iid).copied() == Some(session) {
                        self.instance_index.remove(&iid);
                    }
                }
                return;
            }

            info!(
                "Grace period expired for user {} (session {}) in room {} — \
                 executing PARTICIPANT_LEFT",
                user_id, session, room
            );
            // Observer sessions bypass the grace period entirely (handled
            // directly in Disconnect), so this path is always non-observer.
            self.leave_rooms(
                &session,
                Some(&room),
                Some(&user_id),
                Some(&display_name),
                false,
                is_host,
                end_on_host_leave,
            );
        } else {
            info!(
                "Pending departure for user {} in room {} already cancelled (reconnected)",
                user_id, room
            );
        }
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

        // Check connection state - only publish to NATS if Active
        let connection_state = self
            .connection_states
            .get(&session)
            .copied()
            .unwrap_or(ConnectionState::Testing);

        if connection_state != ConnectionState::Active {
            trace!(
                "Skipping NATS publish for session {} in Testing state",
                session
            );
            return; // Don't publish during Testing state
        }

        let nc = self.nats_connection.clone();
        let subject = format!("room.{room}.{session}");
        let subject = subject.replace(' ', "_");

        let packet_bytes =
            if let Ok(mut packet_wrapper) = PacketWrapper::parse_from_bytes(&msg.data) {
                if packet_wrapper.session_id == 0 {
                    packet_wrapper.session_id = session;
                }
                match packet_wrapper.write_to_bytes() {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        error!("Failed to serialize PacketWrapper with session_id: {}", e);
                        msg.data.to_vec()
                    }
                }
            } else {
                msg.data.to_vec()
            };

        let b = bytes::Bytes::from(packet_bytes);
        let fut = async move {
            let start = std::time::Instant::now();
            match nc.publish(subject.clone(), b).await {
                Ok(_) => {
                    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
                    RELAY_NATS_PUBLISH_LATENCY_MS.observe(elapsed_ms);
                    trace!("published message to {subject}");
                }
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
            display_name,
            is_guest,
            observer,
            instance_id,
            is_host,
            end_on_host_leave,
        }: JoinRoom,
        ctx: &mut Self::Context,
    ) -> Self::Result {
        // Validate user_id synchronously BEFORE spawning async task.
        // This ensures we return an error to the client if validation fails,
        // rather than returning Ok and silently failing in the spawned task.
        if user_id == SYSTEM_USER_ID {
            return MessageResult(Err("Cannot use reserved system user ID".into()));
        }

        // Persist the server-authoritative guest flag so downstream
        // handlers (ActivateConnection, Disconnect, leave_rooms, and the
        // "existing members" broadcast in this handler) can retrieve it
        // by session_id without widening the `room_members` tuple.
        self.session_is_guest.insert(session, is_guest);

        if self.active_subs.contains_key(&session) {
            return MessageResult(Ok(()));
        }

        // Sanitize instance_id: reject oversized values to prevent memory abuse.
        let instance_id = instance_id.filter(|iid| !iid.is_empty() && iid.len() <= 64);

        // --- Live session eviction by instance_id ---
        // If the client provides an instance_id (stable UUID per tab/meeting),
        // look up the instance_index to find the previous session for this
        // client instance. If found (and it belongs to the same user), evict
        // the stale session silently so peers don't see a spurious leave/join.
        // This handles the common case where the client reconnects before the
        // server's heartbeat timeout detects the old session is dead.
        let mut evicted_old_session = false;
        if let Some(ref iid) = instance_id {
            evicted_old_session = self.evict_stale_session(iid, &room, &user_id, session, ctx);

            // Register/update this instance_id → session mapping (both directions).
            self.instance_index.insert(iid.clone(), session);
            self.session_instance.insert(session, iid.clone());

            // Cross-server eviction broadcast is deferred to ActivateConnection.
            // During RTT election, multiple connections (WS + WT) fire JoinRoom,
            // but only the winner activates. Publishing here would send 2-4
            // unnecessary NATS messages per connect.
        }

        // --- Reconnection grace period: cancel pending departure ---
        // If the same user_id is reconnecting to the same room within
        // the grace window, suppress both PARTICIPANT_LEFT (already deferred)
        // and the PARTICIPANT_JOINED that would normally follow.
        let departure_key = (room.clone(), user_id.clone());
        let is_reconnection = if let Some(pending) = self.pending_departures.remove(&departure_key)
        {
            ctx.cancel_future(pending.spawn_handle);

            // Clean up stale room_members entry from the old session
            if let Some(members) = self.room_members.get_mut(&room) {
                members.retain(|m| m.session != pending.old_session);
            }

            info!(
                "Reconnection detected for user {} in room {} — cancelled pending \
                 PARTICIPANT_LEFT (old session {}, new session {})",
                user_id, room, pending.old_session, session
            );
            true
        } else {
            false
        };

        // Mark reconnection and observer sessions so ActivateConnection does not
        // broadcast PARTICIPANT_JOINED for them. Reconnection sessions never
        // "left" from peers' perspective; observers are never announced.
        // Also suppress for instance_id-based evictions (same client instance).
        if is_reconnection || evicted_old_session || observer {
            self.suppress_join_broadcast.insert(session);
        }

        let room_clone = room.clone();
        let user_id_clone = user_id.clone();
        let display_name_clone = display_name.clone();
        let session_id = session;
        let nc = self.nats_connection.clone();

        let session_str = session.to_string();
        let (subject, queue) = build_subject_and_queue(&room, &session_str);
        let session_recipient = match self.sessions.get(&session) {
            Some(addr) => addr.clone(),
            None => {
                return MessageResult(Err("Session not found".into()));
            }
        };

        // Collect existing non-observer room members for notifying the new joiner.
        // On reconnection, we still send the existing member list so the
        // reconnecting client knows who is in the room.
        //
        // Snapshot `is_guest` per existing session here (inside the handler,
        // where `self` is in scope) so the spawned task can build accurate
        // PARTICIPANT_JOINED packets without needing to re-enter the actor.
        // The tuple preserves the full RoomMemberInfo (needed for host-leave
        // tracking) alongside the server-authoritative guest flag.
        let existing_members: Vec<(RoomMemberInfo, bool)> = if !observer {
            self.room_members
                .get(&room)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|m| {
                    let is_guest = self
                        .session_is_guest
                        .get(&m.session)
                        .copied()
                        .unwrap_or(false);
                    (m, is_guest)
                })
                .collect()
        } else {
            Vec::new()
        };

        // True when the room had no non-observer participants before this join.
        // Used to gate the NATS MEETING_STARTED broadcast (the transport actors
        // already send MEETING_STARTED directly to every connecting client).
        let is_first_in_room = existing_members.is_empty() && !observer;

        // Track this session in room_members (only for non-observers)
        if !observer {
            self.room_members
                .entry(room.clone())
                .or_default()
                .push(RoomMemberInfo {
                    session,
                    user_id: user_id.clone(),
                    display_name: display_name.clone(),
                    is_host,
                    end_on_host_leave,
                });
        }

        // Clone the recipient so we can send existing member info directly to the new joiner
        let new_joiner_recipient = session_recipient.clone();

        let nc2 = self.nats_connection.clone();
        let session_clone = session;
        let server_addr = ctx.address();

        let handle = tokio::spawn(async move {
            // start_session is called by the transport actors (ws_chat_session /
            // wt_chat_session) in their started() method, which blocks with
            // ctx.wait() before this JoinRoom handler runs. We do NOT call it
            // again here to avoid double-counting if SessionManager ever
            // acquires stateful tracking (room capacity, DB records, etc.).
            //
            // The reserved-user-ID check is performed synchronously at the top
            // of this handler, so we can proceed directly to NATS setup.

            let start_time_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);

            info!(
                "JoinRoom task running for user {} in room {} (session {})",
                user_id_clone, room_clone, session_id,
            );

            // SESSION_ASSIGNED is sent by ws_chat_session / wt_chat_session
            // in their started() method before this JoinRoom handler runs.

            // Only broadcast MEETING_STARTED via NATS for the first
            // participant. The transport actors already send it directly
            // to every connecting client, so subsequent joins would just
            // produce redundant events for existing participants.
            if is_first_in_room {
                send_meeting_info(&nc, &room_clone, start_time_ms, &user_id_clone).await;
            }

            // PARTICIPANT_JOINED broadcast is deferred until
            // ActivateConnection is received. This prevents ghost join
            // events from Testing connections during RTT election — only
            // the elected (activated) connection announces itself.
            //
            // Reconnection joins also skip the broadcast (the user never
            // "left" from peers' perspective), and observer joins are
            // never broadcast either.
            if is_reconnection || evicted_old_session {
                info!(
                    "Suppressing PARTICIPANT_JOINED for reconnecting user {} in room {} \
                     (deferred broadcast also skipped)",
                    user_id_clone, room_clone
                );
            } else if observer {
                info!(
                    "Skipping PARTICIPANT_JOINED for observer {} in room {}",
                    user_id_clone, room_clone
                );
            } else {
                info!(
                    "Deferring PARTICIPANT_JOINED for {} (display={}) in room {} \
                     until ActivateConnection (session {})",
                    user_id_clone, display_name_clone, room_clone, session_id
                );
            }

            // Send PARTICIPANT_JOINED for each existing member directly to the new joiner.
            // This ensures the new joiner learns about all participants already in the room.
            for (member, is_guest) in &existing_members {
                let existing_bytes = SessionManager::build_peer_joined_packet(
                    &room_clone,
                    &member.user_id,
                    member.session,
                    &member.display_name,
                    *is_guest,
                );
                info!(
                    "Sending existing PARTICIPANT_JOINED for {} (display={}) to new joiner {}",
                    member.user_id, member.display_name, user_id_clone
                );
                if let Err(e) = new_joiner_recipient.try_send(Message {
                    msg: existing_bytes,
                    session: member.session,
                }) {
                    warn!(
                        "Failed to send existing PARTICIPANT_JOINED for {} to new joiner {}: {}",
                        member.user_id, user_id_clone, e
                    );
                }
            }

            match nc2.queue_subscribe(subject, queue).await {
                Ok(mut sub) => {
                    while let Some(msg) = sub.next().await {
                        // Detect PARTICIPANT_DISPLAY_NAME_CHANGED and update
                        // in-memory room_members via the actor before forwarding.
                        // Only system messages (room.{room}.system) carry display-name
                        // change events — skip the protobuf parse entirely for the
                        // high-frequency per-session media subjects.
                        if msg.subject.ends_with(".system") {
                            if let Ok(wrapper) = PacketWrapper::parse_from_bytes(&msg.payload) {
                                if wrapper.packet_type == PacketType::MEETING.into() {
                                    if let Ok(inner) =
                                        MeetingPacket::parse_from_bytes(&wrapper.data)
                                    {
                                        if inner.event_type
                                            == MeetingEventType::PARTICIPANT_DISPLAY_NAME_CHANGED
                                                .into()
                                        {
                                            let target = match String::from_utf8(
                                                inner.target_user_id.to_vec(),
                                            ) {
                                                Ok(s) => s,
                                                Err(_) => {
                                                    warn!("UpdateMemberDisplayName: non-UTF-8 target_user_id in NATS packet, dropping");
                                                    continue;
                                                }
                                            };
                                            let new_name = match String::from_utf8(
                                                inner.display_name.to_vec(),
                                            ) {
                                                Ok(s) => s,
                                                Err(_) => {
                                                    warn!("UpdateMemberDisplayName: non-UTF-8 display_name in NATS packet, dropping");
                                                    continue;
                                                }
                                            };
                                            if !target.is_empty() && !new_name.is_empty() {
                                                let room_mismatch = !inner.room_id.is_empty()
                                                    && inner.room_id != room_clone;
                                                if room_mismatch {
                                                    warn!(
                                                        "UpdateMemberDisplayName: protobuf room_id '{}' differs from subscription room '{}', sanitizing before forwarding",
                                                        inner.room_id, room_clone
                                                    );
                                                }
                                                server_addr.do_send(UpdateMemberDisplayName {
                                                    room_id: room_clone.clone(),
                                                    user_id: target,
                                                    display_name: new_name,
                                                });
                                                if room_mismatch {
                                                    // Rewrite room_id so clients never see the mismatched value.
                                                    let mut patched = inner;
                                                    patched.room_id = room_clone.clone();
                                                    let forwarded =
                                                        patched.write_to_bytes().and_then(|ib| {
                                                            let mut pw = wrapper;
                                                            pw.data = ib;
                                                            pw.write_to_bytes()
                                                        });
                                                    match forwarded {
                                                        Ok(sanitized) => {
                                                            let message = Message {
                                                                msg: sanitized,
                                                                session: session_clone,
                                                            };
                                                            if let Err(e) =
                                                                session_recipient.try_send(message)
                                                            {
                                                                RELAY_PACKET_DROPS_TOTAL
                                                                    .with_label_values(&[
                                                                        &room_clone,
                                                                        "nats_delivery",
                                                                        "mailbox_full",
                                                                    ])
                                                                    .inc();
                                                                warn!(
                                                                    "Dropping sanitized PARTICIPANT_DISPLAY_NAME_CHANGED for session {}: {}",
                                                                    session_clone, e
                                                                );
                                                            }
                                                        }
                                                        Err(e) => {
                                                            warn!(
                                                                "Failed to re-serialize sanitized PARTICIPANT_DISPLAY_NAME_CHANGED, dropping forward: {}",
                                                                e
                                                            );
                                                        }
                                                    }
                                                    continue;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        if let Err(e) = handle_msg(
                            session_recipient.clone(),
                            room_clone.clone(),
                            session_clone,
                            observer,
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

async fn send_meeting_info(
    nc: &async_nats::client::Client,
    room: &str,
    start_time_ms: u64,
    creator_id: &str,
) {
    let packet_bytes =
        SessionManager::build_meeting_started_packet(room, start_time_ms, creator_id);

    let subject = format!("room.{}.system", room.replace(' ', "_"));
    match nc.publish(subject.clone(), packet_bytes.into()).await {
        Ok(_) => info!("Sent meeting start time {} to {}", start_time_ms, subject),
        Err(e) => error!("Failed to send meeting info to room {}: {}", room, e),
    }
}

fn handle_msg(
    session_recipient: Recipient<Message>,
    room: String,
    session: SessionId,
    observer: bool,
) -> impl Fn(async_nats::Message) -> Result<(), std::io::Error> {
    move |msg| {
        if msg.subject == format!("room.{room}.{session}").replace(' ', "_").into() {
            // Self-skip prevents echo of our own broadcasts. However,
            // CONGESTION signals published on our subject by a congested
            // receiver must still be delivered — they are not echo.
            let is_congestion = PacketWrapper::parse_from_bytes(&msg.payload)
                .map(|pw| pw.packet_type == PacketType::CONGESTION.into())
                .unwrap_or(false);
            if !is_congestion {
                return Ok(());
            }
        }

        // Observer sessions (waiting room) only need meeting-control packets.
        // The client-side `decode_media: false` check is bypassable (WASM
        // patching, raw WebSocket capture, custom client), so the server
        // enforces this as the authoritative filter.
        //
        // Allowlist: only MEETING and SESSION_ASSIGNED are forwarded.
        // Everything else — including any future packet types — is dropped.
        // This is fail-closed by default: new PacketTypes must be explicitly
        // added here to reach observer sessions.
        if observer {
            let allowed = PacketWrapper::parse_from_bytes(&msg.payload)
                .map(|pw| {
                    matches!(
                        pw.packet_type.enum_value(),
                        Ok(PacketType::MEETING) | Ok(PacketType::SESSION_ASSIGNED)
                    )
                })
                .unwrap_or(false); // unparseable → drop (fail-closed)

            if !allowed {
                trace!(
                    "Dropping non-allowed packet for observer session {} in room {}",
                    session,
                    room
                );
                return Ok(());
            }
        }

        let message = Message {
            msg: msg.payload.to_vec(),
            session,
        };

        if let Err(e) = session_recipient.try_send(message) {
            RELAY_PACKET_DROPS_TOTAL
                .with_label_values(&[&room, "nats_delivery", "mailbox_full"])
                .inc();
            warn!(
                "Dropping inbound message for session {}: {} (mailbox full — subscription continues)",
                session, e
            );
        }
        Ok(())
    }
}

// ==========================================================================
// Unit Tests for ChatServer
// ==========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use actix::Actor;
    use serial_test::serial;

    /// Test helper: create a database pool for integration tests.
    /// Kept for future JWT flow testing (create meeting -> get JWT -> connect via WS/WT).
    #[allow(dead_code)]
    async fn get_test_pool() -> sqlx::PgPool {
        let database_url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
        sqlx::PgPool::connect(&database_url)
            .await
            .expect("Failed to connect to test database")
    }

    // ==========================================================================
    // TEST: JoinRoom rejects reserved system user ID synchronously
    // ==========================================================================
    // This test verifies the fix for the race condition where JoinRoom would
    // spawn an async task and immediately return Ok(()), even if validation
    // would fail inside the task. Now validation happens synchronously.
    #[actix_rt::test]
    #[serial]
    async fn test_join_room_rejects_system_user_id_synchronously() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        // Start the ChatServer actor
        let chat_server = ChatServer::new(nats_client).await.start();

        // Create a mock session recipient
        // We need a real actor to receive messages, so we use a simple dummy
        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 1001u64;

        // Register the session first
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        // Attempt to join with the reserved system user ID
        // This should return an error SYNCHRONOUSLY (not Ok then fail async)
        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: "test-room".to_string(),
                user_id: SYSTEM_USER_ID.to_string(),
                display_name: SYSTEM_USER_ID.to_string(),
                is_guest: false,
                observer: false,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
            })
            .await
            .expect("Message delivery should succeed");

        // The key assertion: JoinRoom should return Err immediately
        assert!(
            result.is_err(),
            "JoinRoom with system user ID should return Err, not Ok"
        );

        let error_msg = result.unwrap_err();
        assert!(
            error_msg.contains("reserved system user ID"),
            "Error should mention reserved system user ID, got: {error_msg}"
        );
    }

    // ==========================================================================
    // TEST: JoinRoom succeeds with valid user_id
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_join_room_succeeds_with_valid_user() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 1002u64;

        // Register the session
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        // Join with a valid user_id - should succeed
        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: "test-room-valid".to_string(),
                user_id: "valid-user@example.com".to_string(),
                display_name: "valid-user@example.com".to_string(),
                is_guest: false,
                observer: false,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
            })
            .await
            .expect("Message delivery should succeed");

        assert!(
            result.is_ok(),
            "JoinRoom with valid user should return Ok, got: {result:?}"
        );
    }

    // ==========================================================================
    // TEST: JoinRoom fails if session not registered
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_join_room_fails_without_session() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        // Try to join WITHOUT registering the session first
        let result = chat_server
            .send(JoinRoom {
                session: 9999u64,
                room: "test-room".to_string(),
                user_id: "valid-user@example.com".to_string(),
                display_name: "valid-user@example.com".to_string(),
                is_guest: false,
                observer: false,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
            })
            .await
            .expect("Message delivery should succeed");

        assert!(
            result.is_err(),
            "JoinRoom without registered session should return Err"
        );
        assert!(
            result.unwrap_err().contains("Session not found"),
            "Error should mention session not found"
        );
    }

    // ==========================================================================
    // TEST: Duplicate join with same session returns Ok
    // ==========================================================================
    // Verifies that a second JoinRoom for the same session_id returns Ok
    // immediately because the session is already tracked in active_subs.
    #[actix_rt::test]
    #[serial]
    async fn test_duplicate_join_same_session_returns_ok() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 1003u64;

        // Register the session
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        // First join attempt - should succeed (returns Ok immediately,
        // spawns async task which will also succeed with valid user)
        let result1 = chat_server
            .send(JoinRoom {
                session: session_id,
                room: "test-room-cleanup".to_string(),
                user_id: "valid-user@example.com".to_string(),
                display_name: "valid-user@example.com".to_string(),
                is_guest: false,
                observer: false,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
            })
            .await
            .expect("Message delivery should succeed");

        assert!(result1.is_ok(), "First join should succeed");

        // Second join attempt with same session - should return Ok
        // immediately because session is already in active_subs
        let result2 = chat_server
            .send(JoinRoom {
                session: session_id,
                room: "test-room-cleanup".to_string(),
                user_id: "valid-user@example.com".to_string(),
                display_name: "valid-user@example.com".to_string(),
                is_guest: false,
                observer: false,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
            })
            .await
            .expect("Message delivery should succeed");

        assert!(
            result2.is_ok(),
            "Second join with same session should return Ok (already active)"
        );
    }

    // ==========================================================================
    // TEST: Two clients with same user_id get unique session_id values
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_same_user_id_unique_session_ids() {
        use crate::actors::session_logic::SessionLogic;
        use crate::server_diagnostics::{TrackerMessage, TrackerSender};
        use crate::session_manager::SessionManager;
        use tokio::sync::mpsc;

        let _pool = get_test_pool().await;
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();

        let (tx, _rx) = mpsc::unbounded_channel::<TrackerMessage>();
        let tracker_sender: TrackerSender = tx;
        let session_manager = SessionManager::new();

        // Create two sessions with the same user_id
        let user_id = "same-user@example.com".to_string();
        let room = "test-room-unique".to_string();

        let session1 = SessionLogic::new(
            chat_server.clone(),
            room.clone(),
            user_id.clone(),
            user_id.clone(), // display_name fallback
            false,           // is_guest
            nats_client.clone(),
            tracker_sender.clone(),
            session_manager.clone(),
            false,
            None, // no instance_id
            "websocket",
        );

        let session2 = SessionLogic::new(
            chat_server.clone(),
            room.clone(),
            user_id.clone(),
            user_id.clone(), // display_name fallback
            false,           // is_guest
            nats_client.clone(),
            tracker_sender.clone(),
            session_manager.clone(),
            false,
            None, // no instance_id
            "websocket",
        );

        // Verify they have different session IDs
        assert_ne!(
            session1.id, session2.id,
            "Two sessions with same user_id should have different session_id values"
        );
        assert!(session1.id != 0, "Session ID should not be zero");
        assert!(session2.id != 0, "Session ID should not be zero");
    }

    // ==========================================================================
    // TEST: ConnectionState transitions - Testing does not publish to NATS
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_connection_state_testing_does_not_publish() {
        use crate::messages::server::Packet;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use tokio::time::{sleep, Duration};

        let _pool = get_test_pool().await;
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 1004u64;
        let room = "test-room-state".to_string();

        // Register session - starts in Testing state
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        let subject = format!("room.{room}.{session_id}").replace(' ', "_");
        let published = Arc::new(AtomicBool::new(false));
        let published_clone = published.clone();
        let mut sub = nats_client
            .subscribe(subject.clone())
            .await
            .expect("Failed to subscribe");

        tokio::spawn(async move {
            if let Ok(Some(_msg)) =
                tokio::time::timeout(Duration::from_millis(500), sub.next()).await
            {
                published_clone.store(true, Ordering::Relaxed);
            }
        });

        // Send message while in Testing state - should NOT publish
        chat_server
            .send(ClientMessage {
                session: session_id,
                room: room.clone(),
                msg: Packet {
                    data: Arc::new(b"test data".to_vec()),
                },
                user: "test@example.com".to_string(),
            })
            .await
            .expect("Message delivery should succeed");

        // Wait a bit to ensure no publish happened
        sleep(Duration::from_millis(600)).await;

        assert!(
            !published.load(Ordering::Relaxed),
            "Message should NOT be published while in Testing state"
        );
    }

    // ==========================================================================
    // TEST: ConnectionState transitions - Active publishes to NATS
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_connection_state_active_publishes() {
        use crate::messages::server::Packet;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use tokio::time::{sleep, Duration};

        let _pool = get_test_pool().await;
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 1005u64;
        let room = "test-room-active".to_string();

        // Register session - starts in Testing state
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        // Activate the connection
        chat_server
            .send(ActivateConnection {
                session: session_id,
            })
            .await
            .expect("ActivateConnection should succeed");

        let subject = format!("room.{room}.{session_id}").replace(' ', "_");
        let published = Arc::new(AtomicBool::new(false));
        let published_clone = published.clone();
        let mut sub = nats_client
            .subscribe(subject.clone())
            .await
            .expect("Failed to subscribe");

        tokio::spawn(async move {
            if let Ok(Some(_msg)) =
                tokio::time::timeout(Duration::from_millis(500), sub.next()).await
            {
                published_clone.store(true, Ordering::Relaxed);
            }
        });

        // Send message while in Active state - should publish
        chat_server
            .send(ClientMessage {
                session: session_id,
                room: room.clone(),
                msg: Packet {
                    data: Arc::new(b"test data".to_vec()),
                },
                user: "test@example.com".to_string(),
            })
            .await
            .expect("Message delivery should succeed");

        // Wait for publish
        sleep(Duration::from_millis(600)).await;

        assert!(
            published.load(Ordering::Relaxed),
            "Message should be published while in Active state"
        );
    }

    // ==========================================================================
    // TEST: ActivateConnection handler is idempotent
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_activate_connection_idempotent() {
        let _pool = get_test_pool().await;
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 1006u64;

        // Register session - starts in Testing state
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        // First activation - should transition Testing -> Active
        chat_server
            .send(ActivateConnection {
                session: session_id,
            })
            .await
            .expect("ActivateConnection should succeed");

        // Verify state is Active
        let state1 = chat_server
            .send(GetConnectionState {
                session: session_id,
            })
            .await
            .expect("GetConnectionState should succeed")
            .expect("GetConnectionState should return Ok");
        assert_eq!(
            state1,
            ConnectionState::Active,
            "State should be Active after first activation"
        );

        // Second activation - should remain Active (idempotent)
        chat_server
            .send(ActivateConnection {
                session: session_id,
            })
            .await
            .expect("ActivateConnection should succeed");

        // Verify state is still Active
        let state2 = chat_server
            .send(GetConnectionState {
                session: session_id,
            })
            .await
            .expect("GetConnectionState should succeed")
            .expect("GetConnectionState should return Ok");
        assert_eq!(
            state2,
            ConnectionState::Active,
            "State should remain Active after second activation (idempotent)"
        );
    }

    // ==========================================================================
    // TEST: JoinRoom broadcasts MEETING_STARTED via NATS (no session_id)
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_join_room_broadcasts_meeting_started() {
        use std::sync::{Arc, Mutex};
        use tokio::time::{sleep, Duration};
        use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        let received: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));

        struct CapturingSession {
            received: Arc<Mutex<Vec<Vec<u8>>>>,
        }
        impl Actor for CapturingSession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for CapturingSession {
            type Result = ();
            fn handle(&mut self, msg: Message, _ctx: &mut Self::Context) {
                self.received.lock().unwrap().push(msg.msg);
            }
        }

        let capturing = CapturingSession {
            received: received.clone(),
        }
        .start();
        let session_id = 1007u64;

        chat_server
            .send(Connect {
                id: session_id,
                addr: capturing.recipient(),
            })
            .await
            .expect("Connect should succeed");

        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: "test-room-broadcast".to_string(),
                user_id: "alice@example.com".to_string(),
                display_name: "alice@example.com".to_string(),
                is_guest: false,
                observer: false,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
            })
            .await
            .expect("Message delivery should succeed");

        assert!(result.is_ok(), "JoinRoom should succeed");

        // Wait for the spawned async task to complete and NATS subscription to deliver
        sleep(Duration::from_millis(500)).await;

        let msgs = received.lock().unwrap();
        // The session should NOT receive SESSION_ASSIGNED from ChatServer
        // (that's the transport layer's job). It may receive MEETING_STARTED
        // via NATS if the subscription was set up in time.
        for msg_bytes in msgs.iter() {
            if let Ok(wrapper) = <PacketWrapper as ProtobufMessage>::parse_from_bytes(msg_bytes) {
                assert_ne!(
                    wrapper.packet_type,
                    PacketType::SESSION_ASSIGNED.into(),
                    "ChatServer JoinRoom should NOT send SESSION_ASSIGNED directly"
                );
                if wrapper.packet_type == PacketType::MEETING.into() {
                    assert_eq!(
                        wrapper.session_id, 0,
                        "MEETING_STARTED must not carry session_id"
                    );
                }
            }
        }
    }

    // ==========================================================================
    // TEST: Observer JoinRoom does NOT publish PARTICIPANT_JOINED
    // ==========================================================================
    // When an observer (waiting room user) joins a room, the server should NOT
    // publish a PARTICIPANT_JOINED event to NATS. Only real participants trigger
    // this notification.
    #[actix_rt::test]
    #[serial]
    async fn test_observer_join_does_not_publish_participant_joined() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 2001u64;
        let room = "test-room-observer-join";

        // Subscribe to the system subject for this room BEFORE join
        let system_subject = format!("room.{}.system", room.replace(' ', "_"));
        let participant_joined_received = Arc::new(AtomicBool::new(false));
        let flag = participant_joined_received.clone();
        let mut sub = nats_client
            .subscribe(system_subject)
            .await
            .expect("Failed to subscribe to system subject");

        tokio::spawn(async move {
            use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
            use videocall_types::protos::meeting_packet::MeetingPacket;

            while let Ok(Some(msg)) =
                tokio::time::timeout(Duration::from_millis(1500), sub.next()).await
            {
                if let Ok(wrapper) =
                    <PacketWrapper as ProtobufMessage>::parse_from_bytes(&msg.payload)
                {
                    if let Ok(inner) = MeetingPacket::parse_from_bytes(&wrapper.data) {
                        if inner.event_type == MeetingEventType::PARTICIPANT_JOINED.into() {
                            flag.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
        });

        // Register session
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        // Join as observer - should NOT publish PARTICIPANT_JOINED
        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: room.to_string(),
                user_id: "observer-user@example.com".to_string(),
                display_name: "observer-user@example.com".to_string(),
                is_guest: false,
                observer: true,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
            })
            .await
            .expect("Message delivery should succeed");

        assert!(result.is_ok(), "Observer JoinRoom should succeed");

        // Wait long enough for any NATS publish to arrive
        sleep(Duration::from_millis(1000)).await;

        assert!(
            !participant_joined_received.load(Ordering::Relaxed),
            "Observer join should NOT publish PARTICIPANT_JOINED to NATS"
        );
    }

    // ==========================================================================
    // TEST: Non-observer JoinRoom + ActivateConnection publishes PARTICIPANT_JOINED
    // ==========================================================================
    // When a real participant joins a room and their connection is activated,
    // the server should publish a PARTICIPANT_JOINED event to NATS so other
    // peers are notified. The broadcast is deferred from JoinRoom to
    // ActivateConnection to avoid ghost join events during RTT election.
    #[actix_rt::test]
    #[serial]
    async fn test_non_observer_join_publishes_participant_joined() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 2002u64;
        let room = "test-room-non-observer-join";

        // Subscribe to the system subject for this room BEFORE join
        let system_subject = format!("room.{}.system", room.replace(' ', "_"));
        let participant_joined_received = Arc::new(AtomicBool::new(false));
        let flag = participant_joined_received.clone();
        let mut sub = nats_client
            .subscribe(system_subject)
            .await
            .expect("Failed to subscribe to system subject");

        tokio::spawn(async move {
            use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
            use videocall_types::protos::meeting_packet::MeetingPacket;

            while let Ok(Some(msg)) =
                tokio::time::timeout(Duration::from_millis(1500), sub.next()).await
            {
                if let Ok(wrapper) =
                    <PacketWrapper as ProtobufMessage>::parse_from_bytes(&msg.payload)
                {
                    if let Ok(inner) = MeetingPacket::parse_from_bytes(&wrapper.data) {
                        if inner.event_type == MeetingEventType::PARTICIPANT_JOINED.into() {
                            flag.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
        });

        // Register session
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        // Join as non-observer
        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: room.to_string(),
                user_id: "real-user@example.com".to_string(),
                display_name: "real-user@example.com".to_string(),
                is_guest: false,
                observer: false,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
            })
            .await
            .expect("Message delivery should succeed");

        assert!(result.is_ok(), "Non-observer JoinRoom should succeed");

        // Activate the connection — this triggers the deferred PARTICIPANT_JOINED
        chat_server
            .send(ActivateConnection {
                session: session_id,
            })
            .await
            .expect("ActivateConnection should succeed");

        // Wait for the async task to publish PARTICIPANT_JOINED
        sleep(Duration::from_millis(1000)).await;

        assert!(
            participant_joined_received.load(Ordering::Relaxed),
            "Non-observer join + activate SHOULD publish PARTICIPANT_JOINED to NATS"
        );
    }

    // ==========================================================================
    // TEST: JoinRoom without ActivateConnection does NOT publish PARTICIPANT_JOINED
    // ==========================================================================
    // When a connection joins but is never activated (e.g., the losing connection
    // during RTT election), PARTICIPANT_JOINED should NOT be broadcast.
    #[actix_rt::test]
    #[serial]
    async fn test_join_without_activate_does_not_publish_participant_joined() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 3001u64;
        let room = "test-room-no-activate";

        // Subscribe to the system subject for this room BEFORE join
        let system_subject = format!("room.{}.system", room.replace(' ', "_"));
        let participant_joined_received = Arc::new(AtomicBool::new(false));
        let flag = participant_joined_received.clone();
        let mut sub = nats_client
            .subscribe(system_subject)
            .await
            .expect("Failed to subscribe to system subject");

        tokio::spawn(async move {
            use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
            use videocall_types::protos::meeting_packet::MeetingPacket;

            while let Ok(Some(msg)) =
                tokio::time::timeout(Duration::from_millis(1500), sub.next()).await
            {
                if let Ok(wrapper) =
                    <PacketWrapper as ProtobufMessage>::parse_from_bytes(&msg.payload)
                {
                    if let Ok(inner) = MeetingPacket::parse_from_bytes(&wrapper.data) {
                        if inner.event_type == MeetingEventType::PARTICIPANT_JOINED.into() {
                            flag.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
        });

        // Register session
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        // Join as non-observer but do NOT activate
        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: room.to_string(),
                user_id: "testing-user@example.com".to_string(),
                display_name: "testing-user@example.com".to_string(),
                is_guest: false,
                observer: false,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
            })
            .await
            .expect("Message delivery should succeed");

        assert!(result.is_ok(), "JoinRoom should succeed");

        // Wait — no ActivateConnection sent
        sleep(Duration::from_millis(1500)).await;

        assert!(
            !participant_joined_received.load(Ordering::Relaxed),
            "JoinRoom without ActivateConnection should NOT publish PARTICIPANT_JOINED"
        );
    }

    // ==========================================================================
    // TEST: Testing session disconnect does NOT publish PARTICIPANT_LEFT
    // ==========================================================================
    // When a Testing session disconnects (e.g., the losing connection during
    // RTT election), PARTICIPANT_LEFT should NOT be broadcast because
    // PARTICIPANT_JOINED was never broadcast for it.
    #[actix_rt::test]
    #[serial]
    async fn test_testing_session_disconnect_does_not_publish_participant_left() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 3002u64;
        let room = "test-room-testing-dc";

        // Register and join (Testing state, never activated)
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: room.to_string(),
                user_id: "testing-dc@example.com".to_string(),
                display_name: "testing-dc@example.com".to_string(),
                is_guest: false,
                observer: false,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
            })
            .await
            .expect("Message delivery should succeed");
        assert!(result.is_ok(), "JoinRoom should succeed");

        // Wait for session setup
        sleep(Duration::from_millis(300)).await;

        // Subscribe to system subject to watch for PARTICIPANT_LEFT
        let system_subject = format!("room.{}.system", room.replace(' ', "_"));
        let participant_left_received = Arc::new(AtomicBool::new(false));
        let flag = participant_left_received.clone();
        let mut sub = nats_client
            .subscribe(system_subject)
            .await
            .expect("Failed to subscribe to system subject");

        tokio::spawn(async move {
            use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
            use videocall_types::protos::meeting_packet::MeetingPacket;

            while let Ok(Some(msg)) = tokio::time::timeout(Duration::from_secs(6), sub.next()).await
            {
                if let Ok(wrapper) =
                    <PacketWrapper as ProtobufMessage>::parse_from_bytes(&msg.payload)
                {
                    if let Ok(inner) = MeetingPacket::parse_from_bytes(&wrapper.data) {
                        if inner.event_type == MeetingEventType::PARTICIPANT_LEFT.into() {
                            flag.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
        });

        // Disconnect while still in Testing state (never activated)
        chat_server
            .send(Disconnect {
                session: session_id,
                room: room.to_string(),
                user_id: "testing-dc@example.com".to_string(),
                display_name: "testing-dc@example.com".to_string(),
                is_guest: false,
                observer: false,
                is_host: false,
                end_on_host_leave: true,
            })
            .await
            .expect("Disconnect should succeed");

        // Wait for grace period to expire plus buffer
        sleep(Duration::from_secs(4)).await;

        assert!(
            !participant_left_received.load(Ordering::Relaxed),
            "Testing session disconnect should NOT publish PARTICIPANT_LEFT \
             (was_active=false prevents ghost leave event)"
        );
    }

    // ==========================================================================
    // TEST: Observer Disconnect does NOT publish PARTICIPANT_LEFT
    // ==========================================================================
    // When an observer session disconnects (e.g., waiting room user admitted),
    // the server should NOT publish a PARTICIPANT_LEFT event. The user was never
    // a real participant in the meeting.
    #[actix_rt::test]
    #[serial]
    async fn test_observer_disconnect_does_not_publish_participant_left() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 2003u64;
        let room = "test-room-observer-disconnect";

        // Register and join as observer first
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: room.to_string(),
                user_id: "observer-dc@example.com".to_string(),
                display_name: "observer-dc@example.com".to_string(),
                is_guest: false,
                observer: true,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
            })
            .await
            .expect("Message delivery should succeed");
        assert!(result.is_ok(), "Observer JoinRoom should succeed");

        // Wait for session to be fully set up
        sleep(Duration::from_millis(300)).await;

        // Now subscribe to system subject to watch for PARTICIPANT_LEFT
        let system_subject = format!("room.{}.system", room.replace(' ', "_"));
        let participant_left_received = Arc::new(AtomicBool::new(false));
        let flag = participant_left_received.clone();
        let mut sub = nats_client
            .subscribe(system_subject)
            .await
            .expect("Failed to subscribe to system subject");

        tokio::spawn(async move {
            use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
            use videocall_types::protos::meeting_packet::MeetingPacket;

            while let Ok(Some(msg)) =
                tokio::time::timeout(Duration::from_millis(1500), sub.next()).await
            {
                if let Ok(wrapper) =
                    <PacketWrapper as ProtobufMessage>::parse_from_bytes(&msg.payload)
                {
                    if let Ok(inner) = MeetingPacket::parse_from_bytes(&wrapper.data) {
                        if inner.event_type == MeetingEventType::PARTICIPANT_LEFT.into() {
                            flag.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
        });

        // Disconnect as observer - should NOT publish PARTICIPANT_LEFT
        chat_server
            .send(Disconnect {
                session: session_id,
                room: room.to_string(),
                user_id: "observer-dc@example.com".to_string(),
                display_name: "observer-dc@example.com".to_string(),
                is_guest: false,
                observer: true,
                is_host: false,
                end_on_host_leave: true,
            })
            .await
            .expect("Disconnect should succeed");

        // Wait long enough for any NATS publish to arrive
        sleep(Duration::from_millis(1000)).await;

        assert!(
            !participant_left_received.load(Ordering::Relaxed),
            "Observer disconnect should NOT publish PARTICIPANT_LEFT to NATS"
        );
    }

    // ==========================================================================
    // TEST: Non-observer Disconnect publishes PARTICIPANT_LEFT after grace period
    // ==========================================================================
    // When a real participant disconnects, the server defers the PARTICIPANT_LEFT
    // broadcast by RECONNECT_GRACE_PERIOD. If no reconnection occurs, the event
    // is published after the grace period expires. This test uses
    // ExecutePendingDeparture directly to avoid waiting for the full grace period.
    #[actix_rt::test]
    #[serial]
    async fn test_non_observer_disconnect_publishes_event() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use tokio::time::{sleep, Duration};

        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client.clone()).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 2004u64;
        let room = "test-room-non-observer-disconnect";

        // Register and join as real participant
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: room.to_string(),
                user_id: "real-dc@example.com".to_string(),
                display_name: "real-dc@example.com".to_string(),
                is_guest: false,
                observer: false,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
            })
            .await
            .expect("Message delivery should succeed");
        assert!(result.is_ok(), "Non-observer JoinRoom should succeed");

        // Activate the connection so the state is Active before disconnect
        chat_server
            .send(ActivateConnection {
                session: session_id,
            })
            .await
            .expect("ActivateConnection should succeed");

        // Wait for session setup
        sleep(Duration::from_millis(300)).await;

        // Subscribe to system subject to watch for any meeting events
        let system_subject = format!("room.{}.system", room.replace(' ', "_"));
        let meeting_event_received = Arc::new(AtomicBool::new(false));
        let flag = meeting_event_received.clone();
        let mut sub = nats_client
            .subscribe(system_subject)
            .await
            .expect("Failed to subscribe to system subject");

        // Use a longer timeout to accommodate the reconnect grace period.
        // The NATS subscriber waits up to 6s (grace period is 2s + buffer).
        tokio::spawn(async move {
            use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
            use videocall_types::protos::meeting_packet::MeetingPacket;

            while let Ok(Some(msg)) = tokio::time::timeout(Duration::from_secs(6), sub.next()).await
            {
                if let Ok(wrapper) =
                    <PacketWrapper as ProtobufMessage>::parse_from_bytes(&msg.payload)
                {
                    if let Ok(inner) = MeetingPacket::parse_from_bytes(&wrapper.data) {
                        // Accept any meeting lifecycle event (PARTICIPANT_LEFT or MEETING_ENDED)
                        // depending on how end_session categorizes this session
                        if inner.event_type == MeetingEventType::PARTICIPANT_LEFT.into()
                            || inner.event_type == MeetingEventType::MEETING_ENDED.into()
                        {
                            flag.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
        });

        // Disconnect as non-observer — the departure is deferred by
        // RECONNECT_GRACE_PERIOD (3s). The PARTICIPANT_LEFT event will
        // not be published until the grace period expires.
        chat_server
            .send(Disconnect {
                session: session_id,
                room: room.to_string(),
                user_id: "real-dc@example.com".to_string(),
                display_name: "real-dc@example.com".to_string(),
                is_guest: false,
                observer: false,
                is_host: false,
                end_on_host_leave: true,
            })
            .await
            .expect("Disconnect should succeed");

        // Wait for the grace period to expire plus some buffer.
        // RECONNECT_GRACE_PERIOD is 3s, we wait 4s to give the deferred
        // execution and NATS publish time to complete.
        sleep(Duration::from_secs(4)).await;

        // The non-observer path should have attempted to publish via the full
        // end_session flow after the grace period expired.
        assert!(
            meeting_event_received.load(Ordering::Relaxed),
            "Non-observer disconnect should publish a meeting event after grace period \
             (PARTICIPANT_LEFT or MEETING_ENDED)"
        );
    }

    // ==========================================================================
    // TEST: Observer JoinRoom succeeds and session is tracked
    // ==========================================================================
    // Verify that observer sessions are accepted and registered just like normal
    // sessions - the only difference is in event publishing behavior.
    #[actix_rt::test]
    #[serial]
    async fn test_observer_join_room_succeeds() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 2005u64;

        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        // Join as observer - should succeed (same as non-observer)
        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: "test-room-observer-ok".to_string(),
                user_id: "observer@example.com".to_string(),
                display_name: "observer@example.com".to_string(),
                is_guest: false,
                observer: true,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
            })
            .await
            .expect("Message delivery should succeed");

        assert!(
            result.is_ok(),
            "Observer JoinRoom should succeed, got: {result:?}"
        );

        // Joining again with same session should return Ok (already in active_subs)
        let result2 = chat_server
            .send(JoinRoom {
                session: session_id,
                room: "test-room-observer-ok".to_string(),
                user_id: "observer@example.com".to_string(),
                display_name: "observer@example.com".to_string(),
                is_guest: false,
                observer: true,
                instance_id: None,
                is_host: false,
                end_on_host_leave: true,
            })
            .await
            .expect("Message delivery should succeed");

        assert!(
            result2.is_ok(),
            "Second observer JoinRoom should return Ok (already active)"
        );
    }

    // ======================================================================
    // Test helper messages for inspecting ChatServer internal state
    // ======================================================================

    #[derive(ActixMessage)]
    #[rtype(result = "Result<ConnectionState, ()>")]
    struct GetConnectionState {
        session: SessionId,
    }

    impl Handler<GetConnectionState> for ChatServer {
        type Result = Result<ConnectionState, ()>;

        fn handle(&mut self, msg: GetConnectionState, _ctx: &mut Self::Context) -> Self::Result {
            Ok(self
                .connection_states
                .get(&msg.session)
                .copied()
                .unwrap_or(ConnectionState::Testing))
        }
    }

    // ==========================================================================
    // Unit tests for `handle_msg` observer filtering
    // ==========================================================================
    //
    // These tests exercise the closure returned by `handle_msg` in isolation.
    // They do NOT require a NATS connection or `#[serial]` — they only need an
    // actix runtime to start the RecordingSession actor.

    /// Actor that records how many `Message`s it receives.
    struct RecordingSession {
        count: Arc<AtomicUsize>,
    }

    impl Actor for RecordingSession {
        type Context = actix::Context<Self>;
    }

    impl Handler<Message> for RecordingSession {
        type Result = ();
        fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {
            self.count.fetch_add(1, Ordering::Relaxed);
        }
    }

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Build a minimal `async_nats::Message` with the given subject and payload.
    fn make_nats_message(subject: &str, payload: Vec<u8>) -> async_nats::Message {
        async_nats::Message {
            subject: subject.into(),
            payload: payload.into(),
            reply: None,
            headers: None,
            status: None,
            description: None,
            length: 0,
        }
    }

    /// Serialize a `PacketWrapper` with the given `PacketType`.
    fn make_packet_bytes(packet_type: PacketType) -> Vec<u8> {
        let mut pw = PacketWrapper::new();
        pw.packet_type = packet_type.into();
        pw.user_id = b"test-user".to_vec();
        pw.write_to_bytes()
            .expect("PacketWrapper serialization should succeed")
    }

    #[actix_rt::test]
    async fn test_handle_msg_observer_drops_media_packet() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "room1".to_string(),
            9001,
            true, // observer
        );

        let nats_msg = make_nats_message(
            "room.room1.other_session",
            make_packet_bytes(PacketType::MEDIA),
        );
        handler(nats_msg).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "Observer must NOT receive MEDIA packets"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_observer_drops_aes_key_packet() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "room2".to_string(),
            9002,
            true, // observer
        );

        let nats_msg = make_nats_message(
            "room.room2.other_session",
            make_packet_bytes(PacketType::AES_KEY),
        );
        handler(nats_msg).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "Observer must NOT receive AES_KEY packets"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_observer_drops_rsa_pub_key_packet() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "room3".to_string(),
            9003,
            true, // observer
        );

        let nats_msg = make_nats_message(
            "room.room3.other_session",
            make_packet_bytes(PacketType::RSA_PUB_KEY),
        );
        handler(nats_msg).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "Observer must NOT receive RSA_PUB_KEY packets"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_observer_allows_meeting_packet() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "room4".to_string(),
            9004,
            true, // observer
        );

        let nats_msg = make_nats_message(
            "room.room4.other_session",
            make_packet_bytes(PacketType::MEETING),
        );
        handler(nats_msg).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "Observer MUST receive MEETING packets"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_observer_drops_unparseable_packet() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "room5".to_string(),
            9005,
            true, // observer
        );

        // Send garbage bytes that cannot be parsed as a PacketWrapper.
        let nats_msg = make_nats_message(
            "room.room5.other_session",
            vec![0xFF, 0xFE, 0xFD, 0x00, 0x01],
        );
        handler(nats_msg).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "Observer must NOT receive unparseable packets (fail-closed)"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_observer_allows_session_assigned_packet() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "room7".to_string(),
            9007,
            true, // observer
        );

        let nats_msg = make_nats_message(
            "room.room7.other_session",
            make_packet_bytes(PacketType::SESSION_ASSIGNED),
        );
        handler(nats_msg).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "Observer MUST receive SESSION_ASSIGNED packets"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_observer_drops_connection_packet() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "room8".to_string(),
            9008,
            true, // observer
        );

        let nats_msg = make_nats_message(
            "room.room8.other_session",
            make_packet_bytes(PacketType::CONNECTION),
        );
        handler(nats_msg).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "Observer must NOT receive CONNECTION packets (not in allowlist)"
        );
    }

    #[actix_rt::test]
    async fn test_handle_msg_non_observer_forwards_media_packet() {
        let count = Arc::new(AtomicUsize::new(0));
        let actor = RecordingSession {
            count: count.clone(),
        }
        .start();

        let handler = handle_msg(
            actor.recipient(),
            "room6".to_string(),
            9006,
            false, // NOT an observer
        );

        let nats_msg = make_nats_message(
            "room.room6.other_session",
            make_packet_bytes(PacketType::MEDIA),
        );
        handler(nats_msg).expect("handler should not return Err");

        tokio::task::yield_now().await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "Non-observer MUST receive MEDIA packets"
        );
    }
    /// Returns all RoomMemberInfo entries for a given room.
    #[derive(ActixMessage)]
    #[rtype(result = "Vec<RoomMemberInfo>")]
    struct GetRoomMembers {
        room: String,
    }

    impl Handler<GetRoomMembers> for ChatServer {
        type Result = MessageResult<GetRoomMembers>;

        fn handle(&mut self, msg: GetRoomMembers, _ctx: &mut Self::Context) -> Self::Result {
            MessageResult(
                self.room_members
                    .get(&msg.room)
                    .cloned()
                    .unwrap_or_default(),
            )
        }
    }

    /// Check whether a session is in the suppress_join_broadcast set.
    #[derive(ActixMessage)]
    #[rtype(result = "bool")]
    struct IsSuppressedJoinBroadcast {
        session: SessionId,
    }

    impl Handler<IsSuppressedJoinBroadcast> for ChatServer {
        type Result = bool;

        fn handle(
            &mut self,
            msg: IsSuppressedJoinBroadcast,
            _ctx: &mut Self::Context,
        ) -> Self::Result {
            self.suppress_join_broadcast.contains(&msg.session)
        }
    }

    /// Check whether a session is registered in the sessions map.
    #[derive(ActixMessage)]
    #[rtype(result = "bool")]
    struct HasSession {
        session: SessionId,
    }

    impl Handler<HasSession> for ChatServer {
        type Result = bool;

        fn handle(&mut self, msg: HasSession, _ctx: &mut Self::Context) -> Self::Result {
            self.sessions.contains_key(&msg.session)
        }
    }

    /// Check whether a session has an active NATS subscription.
    #[derive(ActixMessage)]
    #[rtype(result = "bool")]
    struct HasActiveSub {
        session: SessionId,
    }

    impl Handler<HasActiveSub> for ChatServer {
        type Result = bool;

        fn handle(&mut self, msg: HasActiveSub, _ctx: &mut Self::Context) -> Self::Result {
            self.active_subs.contains_key(&msg.session)
        }
    }

    // ======================================================================
    // Helper: connect + join a session, returning Ok or panicking
    // ======================================================================
    async fn connect_and_join(
        chat_server: &actix::Addr<ChatServer>,
        session_id: SessionId,
        room: &str,
        user_id: &str,
        addr: actix::Recipient<Message>,
        instance_id: Option<String>,
    ) {
        chat_server
            .send(Connect {
                id: session_id,
                addr,
            })
            .await
            .expect("Connect should succeed");

        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: room.to_string(),
                user_id: user_id.to_string(),
                display_name: user_id.to_string(),
                is_guest: false,
                observer: false,
                instance_id,
                is_host: false,
                end_on_host_leave: true,
            })
            .await
            .expect("Message delivery should succeed");

        assert!(
            result.is_ok(),
            "JoinRoom should succeed for session {session_id}, got: {result:?}"
        );
    }

    // ======================================================================
    // Session eviction via instance_id — integration tests
    // ======================================================================

    // ------------------------------------------------------------------
    // TEST 1: Basic eviction — Session B evicts Session A (same instance_id)
    // ------------------------------------------------------------------
    // Session A joins a room with instance_id="inst-1". Session B joins the
    // same room with the same user_id and instance_id="inst-1". Verify:
    //   - Session A is removed from room_members
    //   - Session B is present in room_members
    //   - PARTICIPANT_JOINED is suppressed for Session B
    #[actix_rt::test]
    #[serial]
    async fn test_eviction_basic_same_user() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let room = "eviction-basic";
        let user_id = "alice@example.com";
        let instance_id = "inst-alice-1".to_string();
        let session_a: SessionId = 5001;
        let session_b: SessionId = 5002;

        // Session A joins with instance_id
        let dummy_a = DummySession.start();
        connect_and_join(
            &chat_server,
            session_a,
            room,
            user_id,
            dummy_a.recipient(),
            Some(instance_id.clone()),
        )
        .await;

        // Allow the async JoinRoom task to start the NATS subscription
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Verify Session A is in room_members
        let members = chat_server
            .send(GetRoomMembers {
                room: room.to_string(),
            })
            .await
            .expect("GetRoomMembers should succeed");
        assert_eq!(members.len(), 1, "Room should have exactly 1 member");
        assert_eq!(
            members[0].session, session_a,
            "Session A should be in the room"
        );

        // Session B joins with same instance_id (same user reconnecting)
        let dummy_b = DummySession.start();
        connect_and_join(
            &chat_server,
            session_b,
            room,
            user_id,
            dummy_b.recipient(),
            Some(instance_id),
        )
        .await;

        // Allow the async JoinRoom task to complete
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Verify Session A is evicted and Session B is in room_members
        let members = chat_server
            .send(GetRoomMembers {
                room: room.to_string(),
            })
            .await
            .expect("GetRoomMembers should succeed");
        assert_eq!(
            members.len(),
            1,
            "Room should have exactly 1 member after eviction"
        );
        assert_eq!(
            members[0].session, session_b,
            "Session B should be the sole member"
        );

        // Verify Session A's internal state was cleaned up
        let has_session_a = chat_server
            .send(HasSession { session: session_a })
            .await
            .expect("HasSession should succeed");
        assert!(
            !has_session_a,
            "Session A should be removed from sessions map"
        );

        let has_sub_a = chat_server
            .send(HasActiveSub { session: session_a })
            .await
            .expect("HasActiveSub should succeed");
        assert!(
            !has_sub_a,
            "Session A should have no active NATS subscription"
        );

        // Verify PARTICIPANT_JOINED is suppressed for Session B
        let suppressed = chat_server
            .send(IsSuppressedJoinBroadcast { session: session_b })
            .await
            .expect("IsSuppressedJoinBroadcast should succeed");
        assert!(
            suppressed,
            "Session B should have PARTICIPANT_JOINED suppressed (eviction reconnect)"
        );
    }

    // ------------------------------------------------------------------
    // TEST 2: Different user_id — no eviction
    // ------------------------------------------------------------------
    // Session A joins (user "alice") with instance_id="inst-1". Session B
    // joins (user "bob") with the same instance_id="inst-1". Session A must
    // NOT be evicted because the user_id does not match.
    #[actix_rt::test]
    #[serial]
    async fn test_eviction_different_user_no_eviction() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let room = "eviction-diff-user";
        let instance_id = "inst-shared".to_string();
        let session_a: SessionId = 6001;
        let session_b: SessionId = 6002;

        // Session A joins as "alice" with instance_id
        let dummy_a = DummySession.start();
        connect_and_join(
            &chat_server,
            session_a,
            room,
            "alice@example.com",
            dummy_a.recipient(),
            Some(instance_id.clone()),
        )
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Session B joins as "bob" with the same instance_id
        // (should NOT evict because user_id differs)
        let dummy_b = DummySession.start();
        connect_and_join(
            &chat_server,
            session_b,
            room,
            "bob@example.com",
            dummy_b.recipient(),
            Some(instance_id),
        )
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Both sessions should be in room_members — alice was NOT evicted
        let members = chat_server
            .send(GetRoomMembers {
                room: room.to_string(),
            })
            .await
            .expect("GetRoomMembers should succeed");
        assert_eq!(
            members.len(),
            2,
            "Room should have 2 members (no eviction across different user_ids)"
        );

        let session_ids: Vec<SessionId> = members.iter().map(|m| m.session).collect();
        assert!(
            session_ids.contains(&session_a),
            "Session A (alice) should still be in room_members"
        );
        assert!(
            session_ids.contains(&session_b),
            "Session B (bob) should be in room_members"
        );

        // Session A should still be registered (not evicted)
        let has_session_a = chat_server
            .send(HasSession { session: session_a })
            .await
            .expect("HasSession should succeed");
        assert!(
            has_session_a,
            "Session A should NOT be removed when user_id does not match"
        );

        // Session B should NOT have suppress_join_broadcast (normal join, not eviction)
        let suppressed = chat_server
            .send(IsSuppressedJoinBroadcast { session: session_b })
            .await
            .expect("IsSuppressedJoinBroadcast should succeed");
        assert!(
            !suppressed,
            "Session B (different user) should NOT suppress PARTICIPANT_JOINED"
        );
    }

    // ------------------------------------------------------------------
    // TEST 3: No instance_id — normal join flow
    // ------------------------------------------------------------------
    // Session joins with instance_id = None. Verify it follows
    // the normal PARTICIPANT_JOINED flow (not suppressed).
    #[actix_rt::test]
    #[serial]
    async fn test_no_instance_id_normal_join() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let room = "eviction-no-prev";
        let session_id: SessionId = 7001;

        let dummy = DummySession.start();
        connect_and_join(
            &chat_server,
            session_id,
            room,
            "carol@example.com",
            dummy.recipient(),
            None,
        )
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Session should be in room_members
        let members = chat_server
            .send(GetRoomMembers {
                room: room.to_string(),
            })
            .await
            .expect("GetRoomMembers should succeed");
        assert_eq!(members.len(), 1, "Room should have exactly 1 member");
        assert_eq!(
            members[0].session, session_id,
            "The session should be in room_members"
        );

        // PARTICIPANT_JOINED should NOT be suppressed (normal first join)
        let suppressed = chat_server
            .send(IsSuppressedJoinBroadcast {
                session: session_id,
            })
            .await
            .expect("IsSuppressedJoinBroadcast should succeed");
        assert!(
            !suppressed,
            "Normal join (no instance_id) should NOT suppress PARTICIPANT_JOINED"
        );
    }

    // ------------------------------------------------------------------
    // TEST 4: New instance_id (no prior session) — normal join
    // ------------------------------------------------------------------
    // Session joins with an instance_id that has no prior entry in the
    // instance_index. Verify no panic and the join proceeds normally.
    #[actix_rt::test]
    #[serial]
    async fn test_new_instance_id_normal_join() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let room = "eviction-new-instance";
        let session_id: SessionId = 8001;

        let dummy = DummySession.start();
        connect_and_join(
            &chat_server,
            session_id,
            room,
            "dave@example.com",
            dummy.recipient(),
            Some("inst-brand-new".to_string()), // instance_id with no prior session
        )
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Session should be in room_members (first-time join with this instance_id)
        let members = chat_server
            .send(GetRoomMembers {
                room: room.to_string(),
            })
            .await
            .expect("GetRoomMembers should succeed");
        assert_eq!(
            members.len(),
            1,
            "Room should have exactly 1 member after first join with new instance_id"
        );
        assert_eq!(
            members[0].session, session_id,
            "The new session should be in room_members"
        );

        // No eviction occurred, so PARTICIPANT_JOINED should NOT be suppressed
        let suppressed = chat_server
            .send(IsSuppressedJoinBroadcast {
                session: session_id,
            })
            .await
            .expect("IsSuppressedJoinBroadcast should succeed");
        assert!(
            !suppressed,
            "First join with new instance_id should NOT suppress PARTICIPANT_JOINED"
        );
    }

    // ------------------------------------------------------------------
    // TEST 5: Multi-device safe — same user, different instance_ids
    // ------------------------------------------------------------------
    // Session A joins (user "alice", instance_id="inst-tab1"). Session B
    // joins (user "alice", instance_id="inst-tab2"). Both should coexist
    // in room_members because different instance_ids mean different
    // browser tabs / devices.
    #[actix_rt::test]
    #[serial]
    async fn test_multi_device_safe_coexistence() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let room = "eviction-multidevice";
        let user_id = "alice@example.com";
        let session_a: SessionId = 9001;
        let session_b: SessionId = 9002;

        // Session A joins with instance_id for tab 1
        let dummy_a = DummySession.start();
        connect_and_join(
            &chat_server,
            session_a,
            room,
            user_id,
            dummy_a.recipient(),
            Some("inst-tab1".to_string()),
        )
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Session B joins with a different instance_id for tab 2
        // (multi-device scenario — different UUIDs = different KV keys)
        let dummy_b = DummySession.start();
        connect_and_join(
            &chat_server,
            session_b,
            room,
            user_id,
            dummy_b.recipient(),
            Some("inst-tab2".to_string()),
        )
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Both sessions should coexist in room_members
        let members = chat_server
            .send(GetRoomMembers {
                room: room.to_string(),
            })
            .await
            .expect("GetRoomMembers should succeed");
        assert_eq!(
            members.len(),
            2,
            "Room should have 2 members (different instance_ids, same user, no eviction)"
        );

        let session_ids: Vec<SessionId> = members.iter().map(|m| m.session).collect();
        assert!(
            session_ids.contains(&session_a),
            "Session A should still be in room_members"
        );
        assert!(
            session_ids.contains(&session_b),
            "Session B should be in room_members"
        );

        // Session A should still be registered
        let has_session_a = chat_server
            .send(HasSession { session: session_a })
            .await
            .expect("HasSession should succeed");
        assert!(
            has_session_a,
            "Session A should still be registered (multi-device, not evicted)"
        );

        // Neither session should have suppressed PARTICIPANT_JOINED
        // (both are fresh joins from different devices)
        let suppressed_a = chat_server
            .send(IsSuppressedJoinBroadcast { session: session_a })
            .await
            .expect("IsSuppressedJoinBroadcast should succeed");
        assert!(
            !suppressed_a,
            "Session A should NOT suppress PARTICIPANT_JOINED"
        );

        let suppressed_b = chat_server
            .send(IsSuppressedJoinBroadcast { session: session_b })
            .await
            .expect("IsSuppressedJoinBroadcast should succeed");
        assert!(
            !suppressed_b,
            "Session B (different instance_id) should NOT suppress PARTICIPANT_JOINED"
        );
    }

    // Test helper: inspect instance_index
    #[derive(ActixMessage)]
    #[rtype(result = "Option<SessionId>")]
    struct GetInstanceSession {
        instance_id: String,
    }

    impl Handler<GetInstanceSession> for ChatServer {
        type Result = Option<SessionId>;

        fn handle(&mut self, msg: GetInstanceSession, _ctx: &mut Self::Context) -> Self::Result {
            self.instance_index.get(&msg.instance_id).copied()
        }
    }

    // ==========================================================================
    // TEST: EvictInstance handler evicts the correct session
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_evict_instance_evicts_correct_session() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 10001u64;
        let instance_id = "cross-server-evict-test".to_string();
        let room = "test-room-cross-evict".to_string();
        let user_id = "alice@example.com".to_string();

        // Register and join with instance_id
        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        let result = chat_server
            .send(JoinRoom {
                session: session_id,
                room: room.clone(),
                user_id: user_id.clone(),
                display_name: user_id.clone(),
                is_guest: false,
                observer: false,
                instance_id: Some(instance_id.clone()),
            })
            .await
            .expect("Message delivery should succeed");
        assert!(result.is_ok(), "JoinRoom should succeed");

        // Verify session is tracked
        let stored = chat_server
            .send(GetInstanceSession {
                instance_id: instance_id.clone(),
            })
            .await
            .expect("Query should succeed");
        assert_eq!(stored, Some(session_id));

        // Send EvictInstance with a DIFFERENT new_session_id (simulating cross-server)
        chat_server
            .send(EvictInstance(EvictInstancePayload {
                instance_id: instance_id.clone(),
                room: room.clone(),
                user_id: user_id.clone(),
                new_session_id: 99999,
            }))
            .await
            .expect("EvictInstance should succeed");

        // Verify session was evicted from instance_index
        let stored = chat_server
            .send(GetInstanceSession {
                instance_id: instance_id.clone(),
            })
            .await
            .expect("Query should succeed");
        assert_eq!(stored, None, "Instance should be removed after eviction");

        // Verify session is removed from sessions map
        let has = chat_server
            .send(HasSession {
                session: session_id,
            })
            .await
            .expect("Query should succeed");
        assert!(!has, "Session should be removed from sessions map");
    }

    // ==========================================================================
    // TEST: EvictInstance is no-op for unknown instance_id
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_evict_instance_noop_for_unknown() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 10002u64;
        let instance_id = "known-inst".to_string();
        let room = "test-room-noop".to_string();
        let user_id = "bob@example.com".to_string();

        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        chat_server
            .send(JoinRoom {
                session: session_id,
                room: room.clone(),
                user_id: user_id.clone(),
                display_name: user_id.clone(),
                is_guest: false,
                observer: false,
                instance_id: Some(instance_id.clone()),
            })
            .await
            .expect("JoinRoom should succeed")
            .expect("JoinRoom should return Ok");

        // Send eviction for UNKNOWN instance_id
        chat_server
            .send(EvictInstance(EvictInstancePayload {
                instance_id: "unknown-inst".to_string(),
                room: room.clone(),
                user_id: user_id.clone(),
                new_session_id: 99999,
            }))
            .await
            .expect("EvictInstance should succeed");

        // Original session should still be present
        let stored = chat_server
            .send(GetInstanceSession {
                instance_id: instance_id.clone(),
            })
            .await
            .expect("Query should succeed");
        assert_eq!(
            stored,
            Some(session_id),
            "Known instance should be unaffected"
        );

        let has = chat_server
            .send(HasSession {
                session: session_id,
            })
            .await
            .expect("Query should succeed");
        assert!(has, "Session should still exist");
    }

    // ==========================================================================
    // TEST: Self-delivery guard prevents double eviction
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_evict_instance_self_delivery_guard() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 10003u64;
        let instance_id = "self-delivery-test".to_string();
        let room = "test-room-self".to_string();
        let user_id = "charlie@example.com".to_string();

        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        chat_server
            .send(JoinRoom {
                session: session_id,
                room: room.clone(),
                user_id: user_id.clone(),
                display_name: user_id.clone(),
                is_guest: false,
                observer: false,
                instance_id: Some(instance_id.clone()),
            })
            .await
            .expect("JoinRoom should succeed")
            .expect("JoinRoom should return Ok");

        // Send eviction with new_session_id == the SAME session (self-delivery)
        chat_server
            .send(EvictInstance(EvictInstancePayload {
                instance_id: instance_id.clone(),
                room: room.clone(),
                user_id: user_id.clone(),
                new_session_id: session_id, // SAME as stored — self-delivery
            }))
            .await
            .expect("EvictInstance should succeed");

        // Session should NOT be evicted (self-delivery guard)
        let stored = chat_server
            .send(GetInstanceSession {
                instance_id: instance_id.clone(),
            })
            .await
            .expect("Query should succeed");
        assert_eq!(
            stored,
            Some(session_id),
            "Self-delivery should not evict the session"
        );

        let has = chat_server
            .send(HasSession {
                session: session_id,
            })
            .await
            .expect("Query should succeed");
        assert!(has, "Session should still exist after self-delivery");
    }

    // ==========================================================================
    // TEST: EvictInstance verifies user_id ownership
    // ==========================================================================
    #[actix_rt::test]
    #[serial]
    async fn test_evict_instance_verifies_user_ownership() {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat_server = ChatServer::new(nats_client).await.start();

        struct DummySession;
        impl Actor for DummySession {
            type Context = actix::Context<Self>;
        }
        impl Handler<Message> for DummySession {
            type Result = ();
            fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
        }

        let dummy = DummySession.start();
        let session_id = 10004u64;
        let instance_id = "ownership-test".to_string();
        let room = "test-room-ownership".to_string();
        let user_id = "alice@example.com".to_string();

        chat_server
            .send(Connect {
                id: session_id,
                addr: dummy.recipient(),
            })
            .await
            .expect("Connect should succeed");

        chat_server
            .send(JoinRoom {
                session: session_id,
                room: room.clone(),
                user_id: user_id.clone(),
                display_name: user_id.clone(),
                is_guest: false,
                observer: false,
                instance_id: Some(instance_id.clone()),
            })
            .await
            .expect("JoinRoom should succeed")
            .expect("JoinRoom should return Ok");

        // Send eviction with DIFFERENT user_id (attacker scenario)
        chat_server
            .send(EvictInstance(EvictInstancePayload {
                instance_id: instance_id.clone(),
                room: room.clone(),
                user_id: "mallory@example.com".to_string(), // WRONG user
                new_session_id: 99999,
            }))
            .await
            .expect("EvictInstance should succeed");

        // Session should NOT be evicted (user_id mismatch)
        let stored = chat_server
            .send(GetInstanceSession {
                instance_id: instance_id.clone(),
            })
            .await
            .expect("Query should succeed");
        assert_eq!(
            stored,
            Some(session_id),
            "Session should not be evicted by different user"
        );

        let has = chat_server
            .send(HasSession {
                session: session_id,
            })
            .await
            .expect("Query should succeed");
        assert!(has, "Session should still exist (user_id mismatch)");
    }
}
