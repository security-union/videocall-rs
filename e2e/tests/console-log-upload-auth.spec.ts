/**
 * API-level tests for the console-log upload endpoint's authentication and
 * authorization contract:
 *
 *   POST /api/v1/meetings/{meeting_id}/console-logs
 *
 * The route authenticates via the meeting **room_token** — the HS256 room-access
 * JWT minted by meeting-api at join time — sent as `Authorization: Bearer
 * <room_token>` (NOT a session cookie). This replaced the prior hard `AuthUser`
 * extractor so uploads work on PKCE deployments that have no auth cookie.
 *
 * Server contract under test (see `meeting-api/src/routes/console_logs.rs` and
 * the `RoomMember` extractor in `meeting-api/src/auth.rs`):
 *
 *   - Missing `Authorization: Bearer` header                       -> 401
 *   - Valid room_token whose `room` claim != the path meeting_id   -> 403
 *   - Valid room_token for the meeting, by a current participant   -> 200
 *
 * Why API-level (not a browser flow):
 *   The browser collector uploads console logs on a background timer / on
 *   page-close via `sendBeacon`. Asserting those opportunistic background
 *   uploads from a real page is inherently racy. The auth/authz contract this
 *   change introduced lives entirely at the HTTP boundary, so a direct request
 *   against the endpoint is both deterministic and a tighter fit for the
 *   protection being verified.
 *
 * room_token provenance:
 *   Tokens are NOT hardcoded or self-signed here — that would re-implement the
 *   server's signing and silently rot if the signing contract changes. Instead
 *   each token is obtained through the real `POST .../join` flow, which returns
 *   a freshly minted `room_token` in `ParticipantStatusResponse.room_token`
 *   (exactly what the production client uses). A host joining a
 *   `waiting_room_enabled=false` meeting is auto-admitted and gets the token
 *   immediately, and is recorded as a participant — satisfying the server's
 *   membership check.
 *
 * Feature flag:
 *   The endpoint is gated by `CONSOLE_LOG_UPLOAD_ENABLED=true` (off by default,
 *   returns 404). The e2e stack sets it to `true` for meeting-api
 *   (docker/docker-compose.e2e.yaml) so the positive (200) and cross-meeting
 *   (403) assertions are meaningful rather than passing vacuously on a 404.
 */

import { test, expect } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

const COOKIE_NAME = process.env.COOKIE_NAME || "session";
const API_URL = process.env.API_BASE_URL || "http://localhost:8081";

// ---------------------------------------------------------------------------
// Low-level API helpers (session-cookie auth, mirrors host-gone-join-check)
// ---------------------------------------------------------------------------

async function apiPost(
  path: string,
  email: string,
  name: string,
  body: unknown,
): Promise<Response> {
  const token = generateSessionToken(email, name);
  return fetch(`${API_URL}${path}`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Cookie: `${COOKIE_NAME}=${token}`,
    },
    body: JSON.stringify(body),
  });
}

/**
 * Join a meeting as the host and return the freshly minted `room_token`.
 *
 * The meeting is auto-created (the first joiner becomes host) with the joining
 * user as creator; a host of a `waiting_room_enabled=false` meeting is
 * auto-admitted, recorded as a participant, and receives a `room_token`
 * immediately. We assert the token is present so the test fails loudly if the
 * join contract ever stops returning one (rather than silently testing with an
 * undefined token).
 */
async function joinAsHostAndGetRoomToken(
  email: string,
  name: string,
  meetingId: string,
): Promise<string> {
  const res = await apiPost(`/api/v1/meetings/${meetingId}/join`, email, name, {
    display_name: name,
  });
  expect(res.ok, `host join for ${meetingId} should succeed`).toBe(true);
  const body = await res.json();
  const roomToken: string | undefined = body?.result?.room_token ?? body?.room_token;
  expect(roomToken, `join response for ${meetingId} must include a room_token`).toBeTruthy();
  return roomToken as string;
}

/**
 * POST a console-log chunk. `authHeader` is the full `Authorization` header
 * value (e.g. `Bearer <room_token>`), or `null` to omit it entirely.
 */
async function uploadConsoleLogs(
  meetingId: string,
  authHeader: string | null,
  payload = "e2e console log chunk\n",
): Promise<Response> {
  const headers: Record<string, string> = {
    "Content-Type": "text/plain",
    "X-Session-Timestamp": String(Date.now()),
  };
  if (authHeader !== null) {
    headers["Authorization"] = authHeader;
  }
  return fetch(`${API_URL}/api/v1/meetings/${meetingId}/console-logs`, {
    method: "POST",
    headers,
    body: payload,
  });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

test.describe("Console-log upload auth (room_token)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  /**
   * Positive path: a valid room_token for THIS meeting, held by a current
   * participant, is accepted (200).
   *
   * Fails if removed/broken: if the feature flag is not enabled the endpoint
   * 404s here (caught by the explicit 200 assertion), and if the happy-path
   * write breaks (e.g. participant check rejects a real participant) this turns
   * non-200. This is the baseline that proves the negative tests below are
   * rejecting for the RIGHT reason, not because the endpoint is simply down.
   */
  test("accepts a valid room_token for the same meeting from a participant (200)", async () => {
    const meetingId = `e2e_cl_ok_${Date.now()}`;
    const email = "cl-ok@videocall.rs";
    const name = "CLOkHost";

    const roomToken = await joinAsHostAndGetRoomToken(email, name, meetingId);

    const res = await uploadConsoleLogs(meetingId, `Bearer ${roomToken}`);
    expect(res.status, "valid room_token + participant should be accepted").toBe(200);
  });

  /**
   * Auth required: no `Authorization` header at all -> 401.
   *
   * This protection is enforced by the `RoomMember` extractor BEFORE the
   * handler body runs, so it fires regardless of the feature flag.
   *
   * Mutation that makes this fail: drop the `Authorization: Bearer` requirement
   * in `RoomMember::from_request_parts` (e.g. revert the route to an extractor
   * that accepts unauthenticated requests). The endpoint would then return a
   * non-401 status (404/200) and this assertion would fail.
   */
  test("rejects an upload with no Authorization header (401)", async () => {
    const meetingId = `e2e_cl_noauth_${Date.now()}`;

    const res = await uploadConsoleLogs(meetingId, null);
    expect(res.status, "missing Bearer token must be unauthorized").toBe(401);
  });

  /**
   * Cross-meeting binding: a valid room_token minted for meeting A must NOT be
   * usable to upload under meeting B's path -> 403.
   *
   * ADVERSARIAL DESIGN (CLAUDE.md rule 2): the SAME user is host of BOTH
   * meetings, so they are a legitimate participant of meeting B as well. With
   * the `claims.room == meeting_id` check in place, uploading A's token to B's
   * path is rejected at the cross-meeting gate (403). If that check were
   * removed, the request would fall through to the participant-membership
   * check — which this user PASSES for meeting B — and the upload would succeed
   * (200). Thus this test fails iff the cross-meeting binding is removed. Were
   * the uploader instead a non-participant of B, the test would still pass via
   * the membership 403 even with the binding gone — a fake pass. Using a
   * dual-meeting host is what makes the binding the sole thing under test.
   *
   * Mutation that makes this fail: delete the
   * `if token_meeting_id != meeting_id { return 403 }` block in
   * `upload_console_logs` — the response becomes 200.
   */
  test("rejects a room_token from a different meeting (403), even for a participant of both", async () => {
    const stamp = Date.now();
    const meetingA = `e2e_cl_xmeet_a_${stamp}`;
    const meetingB = `e2e_cl_xmeet_b_${stamp}`;
    // One identity hosts both meetings -> participant of both.
    const email = "cl-xmeet@videocall.rs";
    const name = "CLXMeetHost";

    const tokenForA = await joinAsHostAndGetRoomToken(email, name, meetingA);
    // Establish the same user as host/participant of meeting B too, so the only
    // thing standing between A's token and a successful upload to B is the
    // cross-meeting binding check.
    await joinAsHostAndGetRoomToken(email, name, meetingB);

    // Sanity: A's token is genuinely accepted for A (rules out a token that is
    // simply invalid, which would also 4xx and mask a removed binding check).
    const okForA = await uploadConsoleLogs(meetingA, `Bearer ${tokenForA}`);
    expect(okForA.status, "control: A's token must work for meeting A").toBe(200);

    // The actual assertion: A's token against B's path is forbidden.
    const res = await uploadConsoleLogs(meetingB, `Bearer ${tokenForA}`);
    expect(res.status, "room_token for meeting A must not authorize meeting B").toBe(403);
  });
});
