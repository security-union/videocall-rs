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

  // Fetch up to 100 owned meetings at a time. The list endpoint clamps
  // limit to [1, 100] so larger values are silently capped.
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
    const rows = json.result.meetings;
    if (rows.length === 0) return;

    for (const row of rows) {
      const del = await fetch(`${API_URL}/api/v1/meetings/${row.meeting_id}`, {
        method: "DELETE",
        headers: { Cookie: cookie },
      });
      // 404 = already gone; treat as success. Other failures are swallowed
      // with a console warning so a single bad row doesn't abort the loop.
      if (!del.ok && del.status !== 404) {
        console.warn(
          `DELETE /api/v1/meetings/${row.meeting_id} returned ${del.status}; continuing`,
        );
      }
    }
  }
}
