/**
 * Helpers for seeding meeting state via the meeting-api REST API.
 *
 * These helpers are used by E2E tests that need to set up a known database
 * state without driving the full UI flow. They sign session JWTs the same way
 * `helpers/auth.ts` does, then make HTTP calls against the meeting-api
 * service exposed by the Docker compose stack on port 8081.
 *
 * Notes:
 * - All requests authenticate via the `session` cookie (HS256 JWT signed with
 *   `JWT_SECRET`). The cookie is only set on the request, not on any browser
 *   context — the API routes accept it directly.
 * - The helpers intentionally throw on non-2xx responses so test failures
 *   surface as clear error messages tied to the seeding step rather than
 *   later DOM-assertion timeouts.
 * - "Joined" semantics: the home-page "Previously Joined" list backs onto
 *   the `/api/v1/meetings/joined` endpoint, which only returns rows where
 *   `meeting_participants.admitted_at IS NOT NULL`. The simplest way to seed
 *   such a row is to call `/api/v1/meetings/{id}/join` — for an owner this
 *   upserts a host participant row (auto-admitted); for a non-owner this
 *   either auto-admits when `waiting_room_enabled=false`, or lands the user
 *   in the waiting room (which is NOT counted as "joined" until admitted).
 */

import { generateSessionToken } from "./auth";

const COOKIE_NAME = process.env.COOKIE_NAME || "session";
const API_URL = process.env.API_BASE_URL || "http://localhost:8081";

/** Build the cookie header for an authenticated meeting-api request. */
function authCookie(email: string, name: string): string {
  const token = generateSessionToken(email, name);
  return `${COOKIE_NAME}=${token}`;
}

interface CreateMeetingOpts {
  /** Custom meeting id. When omitted the server generates one. */
  meetingId?: string;
  /** Defaults to `false` so non-host joins auto-admit immediately. */
  waitingRoomEnabled?: boolean;
  /** Defaults to `false`. */
  allowGuests?: boolean;
  /** Defaults to `true`. */
  endOnHostLeave?: boolean;
}

/**
 * Create a meeting via `POST /api/v1/meetings`.
 *
 * The caller becomes the meeting owner. Returns the resulting `meeting_id`.
 * Tolerates a 409 (meeting already exists) so callers can use deterministic
 * ids across test re-runs without hitting "already exists" errors.
 */
export async function createMeeting(
  email: string,
  name: string,
  opts: CreateMeetingOpts = {},
): Promise<string> {
  const body: Record<string, unknown> = {
    attendees: [],
    waiting_room_enabled: opts.waitingRoomEnabled ?? false,
    allow_guests: opts.allowGuests ?? false,
  };
  if (opts.meetingId !== undefined) body.meeting_id = opts.meetingId;
  if (opts.endOnHostLeave !== undefined) body.end_on_host_leave = opts.endOnHostLeave;

  const res = await fetch(`${API_URL}/api/v1/meetings`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Cookie: authCookie(email, name),
    },
    body: JSON.stringify(body),
  });

  if (res.status === 409 && opts.meetingId !== undefined) {
    return opts.meetingId;
  }
  if (!res.ok) {
    const txt = await res.text();
    throw new Error(`POST /api/v1/meetings failed (${res.status}): ${txt}`);
  }
  const json = (await res.json()) as { result: { meeting_id: string } };
  return json.result.meeting_id;
}

/**
 * Have the given user "join" the meeting via `POST /api/v1/meetings/{id}/join`.
 *
 * Hosts are auto-admitted (and the meeting transitions to `active`).
 * Non-host joiners are auto-admitted only when the meeting was created with
 * `waiting_room_enabled=false`; otherwise they land in the waiting room and
 * the call returns a `waiting`/`waiting_for_meeting` status. The returned
 * status string lets callers verify the seed actually produced an
 * "admitted" row before proceeding to UI assertions.
 */
export async function joinMeeting(
  email: string,
  name: string,
  meetingId: string,
  displayName?: string,
): Promise<{ status: string }> {
  const body: Record<string, unknown> = {};
  if (displayName !== undefined) body.display_name = displayName;

  const res = await fetch(`${API_URL}/api/v1/meetings/${meetingId}/join`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Cookie: authCookie(email, name),
    },
    body: JSON.stringify(body),
  });
  if (!res.ok) {
    const txt = await res.text();
    throw new Error(`POST /api/v1/meetings/${meetingId}/join failed (${res.status}): ${txt}`);
  }
  const json = (await res.json()) as { result: { status: string } };
  return { status: json.result.status };
}

/**
 * End a meeting via `POST /api/v1/meetings/{id}/end`. Only the owner can
 * end a meeting; the call is idempotent on a meeting that is already ended.
 */
export async function endMeeting(
  ownerEmail: string,
  ownerName: string,
  meetingId: string,
): Promise<void> {
  const res = await fetch(`${API_URL}/api/v1/meetings/${meetingId}/end`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Cookie: authCookie(ownerEmail, ownerName),
    },
  });
  if (!res.ok) {
    const txt = await res.text();
    throw new Error(`POST /api/v1/meetings/${meetingId}/end failed (${res.status}): ${txt}`);
  }
}

/**
 * Transfer host from the calling (current host) user to `targetUserId` via
 * `POST /api/v1/meetings/{id}/transfer-host`.
 *
 * The endpoint's `require_host` gate authorizes the CALLER, then atomically
 * promotes the target and demotes the caller in one DB transaction (the only
 * sanctioned self-demotion), publishing `HOST_GRANTED(target)` then
 * `HOST_REVOKED(caller)` over NATS (meeting-api/src/routes/host.rs).
 *
 * The target `user_id` is the participant's JWT `sub`, which for the e2e
 * session tokens is the user's EMAIL (see `generateSessionToken` in
 * `helpers/auth.ts`) — pass the target's e2e email here.
 *
 * This helper exists so a test can move host WHILE the caller's media
 * transport is severed: the browser never receives the `HOST_REVOKED` packet
 * (it is delivered over the media session the relay drops when the transport is
 * down), so the caller's host state drifts until it is reconciled on reconnect.
 */
export async function transferHost(
  callerEmail: string,
  callerName: string,
  meetingId: string,
  targetUserId: string,
): Promise<void> {
  const res = await fetch(`${API_URL}/api/v1/meetings/${meetingId}/transfer-host`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Cookie: authCookie(callerEmail, callerName),
    },
    body: JSON.stringify({ user_id: targetUserId }),
  });
  if (!res.ok) {
    const txt = await res.text();
    throw new Error(
      `POST /api/v1/meetings/${meetingId}/transfer-host failed (${res.status}): ${txt}`,
    );
  }
}

/**
 * Fetch the authenticated user's home feed via `GET /api/v1/meetings/feed`
 * and return the `state` ("idle" | "active" | "ended") of the meeting with the
 * given `meetingId`, or `null` when that meeting is not present in the feed.
 *
 * The feed is the deterministic, server-side source of truth for meeting state
 * (the same payload that backs the home-page meetings list). It is the
 * canonical observation point for the presence-driven idle/active transitions:
 * a created-but-unjoined or fully-drained meeting reports `idle`, a meeting
 * with live presence reports `active`, and an ended meeting reports `ended`.
 *
 * Returns the raw `state` string (not an enum) so callers can `expect.poll`
 * against it directly while the async NATS round-trip settles. `null` (meeting
 * absent) is distinguished from a present-but-unexpected state so a poll can
 * tell "not in my feed yet" apart from "wrong state".
 */
export async function fetchMeetingState(
  email: string,
  name: string,
  meetingId: string,
): Promise<string | null> {
  const res = await fetch(`${API_URL}/api/v1/meetings/feed?limit=200`, {
    method: "GET",
    headers: { Cookie: authCookie(email, name) },
  });
  if (!res.ok) {
    const txt = await res.text();
    throw new Error(`GET /api/v1/meetings/feed failed (${res.status}): ${txt}`);
  }
  const json = (await res.json()) as {
    result: { meetings: Array<{ meeting_id: string; state: string }> };
  };
  const row = json.result.meetings.find((m) => m.meeting_id === meetingId);
  return row ? row.state : null;
}

/**
 * Delete every meeting the authenticated user owns. Useful in `beforeEach`
 * hooks that need a clean baseline for assertions about list content. Does
 * NOT touch participant rows in meetings owned by other users — joined
 * non-owned rows can only be cleaned up by the meeting's owner deleting the
 * meeting, which is intentional (no per-user "leave history" delete).
 *
 * Iterates the user's full owned-meeting list with `?limit=100` per page;
 * stops when the server reports no more meetings. Tolerates per-row delete
 * failures so a single transient error doesn't leave the suite in a bad
 * state — the next test's seed step will pave over any leftovers.
 */
export async function deleteAllOwnedMeetings(email: string, name: string): Promise<void> {
  const cookie = authCookie(email, name);
  const undeletable = new Set<string>();

  while (true) {
    const res = await fetch(`${API_URL}/api/v1/meetings?limit=100&offset=0`, {
      method: "GET",
      headers: { Cookie: cookie },
    });
    if (!res.ok) {
      const txt = await res.text();
      throw new Error(`GET /api/v1/meetings failed (${res.status}): ${txt}`);
    }
    const json = (await res.json()) as {
      result: { meetings: Array<{ meeting_id: string }> };
    };
    const rows = json.result.meetings.filter((r) => !undeletable.has(r.meeting_id));
    if (rows.length === 0) return;

    for (const row of rows) {
      const del = await fetch(`${API_URL}/api/v1/meetings/${row.meeting_id}`, {
        method: "DELETE",
        headers: { Cookie: cookie },
      });
      if (del.ok || del.status === 404) continue;
      // 403 = not the owner; skip on future iterations to avoid infinite loop.
      if (del.status === 403) {
        undeletable.add(row.meeting_id);
      } else {
        console.warn(
          `DELETE /api/v1/meetings/${row.meeting_id} returned ${del.status}; continuing`,
        );
      }
    }
  }
}
