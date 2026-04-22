/**
 * Tests for the "host-gone join check" — the in-transaction guard that returns
 * JOINING_NOT_ALLOWED when the host has left and the meeting is configured with
 * both `admitted_can_admit=false` and `end_on_host_leave=false`.
 *
 * Why these settings matter:
 *   - `end_on_host_leave=false`: the meeting stays alive after the host leaves,
 *     so its status is still "active" and the meeting record is not deleted.
 *   - `admitted_can_admit=false`: no other participant has admission authority,
 *     so a new joiner would enter with nobody able to grant access.
 *
 * The check is folded into the `join_attendee` DB transaction to close the
 * TOCTOU window where a concurrent read of host status could be stale.
 */

import { test, expect } from "@playwright/test";
import { generateSessionToken } from "../helpers/auth";
import { waitForServices } from "../helpers/wait-for-services";

const COOKIE_NAME = process.env.COOKIE_NAME || "session";
const API_URL = process.env.API_BASE_URL || "http://localhost:8081";

// ---------------------------------------------------------------------------
// Low-level API helpers
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

async function apiPatch(
  path: string,
  email: string,
  name: string,
  body: unknown,
): Promise<Response> {
  const token = generateSessionToken(email, name);
  return fetch(`${API_URL}${path}`, {
    method: "PATCH",
    headers: {
      "Content-Type": "application/json",
      Cookie: `${COOKIE_NAME}=${token}`,
    },
    body: JSON.stringify(body),
  });
}

/**
 * Create a meeting via the API and return its meeting_id.
 * The meeting is created with `waiting_room_enabled=false` so that the host
 * join does not block in the waiting room, and with `end_on_host_leave=false`
 * so the meeting persists after the host leaves.
 */
async function createMeeting(
  hostEmail: string,
  hostName: string,
  meetingId: string,
): Promise<string> {
  const res = await apiPost("/api/v1/meetings", hostEmail, hostName, {
    meeting_id: meetingId,
    attendees: [],
    allow_guests: false,
    waiting_room_enabled: false,
  });
  if (!res.ok) {
    const body = await res.text();
    throw new Error(`POST /api/v1/meetings failed (${res.status}): ${body}`);
  }
  const json = await res.json();
  return json.result.meeting_id as string;
}

/**
 * PATCH the meeting settings. Used to set `end_on_host_leave` and
 * `admitted_can_admit` after creation.
 */
async function updateMeeting(
  hostEmail: string,
  hostName: string,
  meetingId: string,
  settings: {
    end_on_host_leave?: boolean;
    admitted_can_admit?: boolean;
    waiting_room_enabled?: boolean;
  },
): Promise<void> {
  const res = await apiPatch(
    `/api/v1/meetings/${meetingId}`,
    hostEmail,
    hostName,
    settings,
  );
  if (!res.ok) {
    const body = await res.text();
    throw new Error(`PATCH /api/v1/meetings/${meetingId} failed (${res.status}): ${body}`);
  }
}

/**
 * POST /join for an authenticated user. Returns the raw Response so callers
 * can inspect both success and error paths.
 */
async function joinMeetingRaw(
  email: string,
  name: string,
  meetingId: string,
): Promise<Response> {
  return apiPost(`/api/v1/meetings/${meetingId}/join`, email, name, {
    display_name: name,
  });
}

/**
 * POST /leave for an authenticated user.
 */
async function leaveMeeting(email: string, name: string, meetingId: string): Promise<void> {
  const res = await apiPost(`/api/v1/meetings/${meetingId}/leave`, email, name, {});
  if (!res.ok) {
    const body = await res.text();
    throw new Error(`POST /leave failed (${res.status}): ${body}`);
  }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

test.describe("Host-gone join check (JOINING_NOT_ALLOWED)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  /**
   * Core scenario:
   *   admitted_can_admit=OFF + end_on_host_leave=OFF
   *   → host joins → host leaves → new participant tries to join
   *   → API must return 403 JOINING_NOT_ALLOWED
   */
  test("new participant receives JOINING_NOT_ALLOWED after host leaves when admitted_can_admit=OFF and end_on_host_leave=OFF", async () => {
    const meetingId = `e2e_hgjc_${Date.now()}`;
    const hostEmail = "hgjc-host@videocall.rs";
    const hostName = "HGJCHost";
    const lateEmail = "hgjc-late@videocall.rs";
    const lateName = "HGJCLateJoiner";

    // 1. Create meeting with default settings (waiting_room_enabled=false for simplicity).
    await createMeeting(hostEmail, hostName, meetingId);

    // 2. Ensure admitted_can_admit=false and end_on_host_leave=false.
    //    These are the two conditions that activate the host-gone check.
    await updateMeeting(hostEmail, hostName, meetingId, {
      admitted_can_admit: false,
      end_on_host_leave: false,
      waiting_room_enabled: false,
    });

    // 3. Host joins to make the meeting "active".
    const hostJoinRes = await joinMeetingRaw(hostEmail, hostName, meetingId);
    expect(hostJoinRes.ok, "host join should succeed").toBe(true);

    // 4. Host leaves.
    await leaveMeeting(hostEmail, hostName, meetingId);

    // 5. A late participant tries to join.  The in-transaction host-gone check
    //    should detect that the creator is absent and return 403.
    const lateJoinRes = await joinMeetingRaw(lateEmail, lateName, meetingId);
    expect(lateJoinRes.status).toBe(403);

    const body = await lateJoinRes.json();
    expect(body?.error?.code ?? body?.code).toBe("JOINING_NOT_ALLOWED");
  });

  /**
   * Control scenario A:
   *   admitted_can_admit=ON + end_on_host_leave=OFF
   *   → the host-gone check is skipped because admitted participants can admit
   *     others, so joining should still succeed even after the host leaves.
   */
  test("new participant can join when admitted_can_admit=ON even after host leaves (end_on_host_leave=OFF)", async () => {
    const meetingId = `e2e_hgjc_aca_${Date.now()}`;
    const hostEmail = "hgjc-aca-host@videocall.rs";
    const hostName = "HGJCACAHost";
    const lateEmail = "hgjc-aca-late@videocall.rs";
    const lateName = "HGJCACALate";

    await createMeeting(hostEmail, hostName, meetingId);
    // waiting_room_enabled=true + admitted_can_admit=true → host-gone check disabled
    await updateMeeting(hostEmail, hostName, meetingId, {
      admitted_can_admit: true,
      end_on_host_leave: false,
      waiting_room_enabled: false,
    });

    const hostJoinRes = await joinMeetingRaw(hostEmail, hostName, meetingId);
    expect(hostJoinRes.ok, "host join should succeed").toBe(true);

    await leaveMeeting(hostEmail, hostName, meetingId);

    // With admitted_can_admit=true, the guard is not enforced; the join may
    // succeed (auto-admitted) or be placed in the waiting room, but must not
    // return JOINING_NOT_ALLOWED.
    const lateJoinRes = await joinMeetingRaw(lateEmail, lateName, meetingId);
    expect(lateJoinRes.status).not.toBe(403);
    if (!lateJoinRes.ok) {
      const text = await lateJoinRes.text();
      expect(text).not.toContain("JOINING_NOT_ALLOWED");
    }
  });

  /**
   * Control scenario B:
   *   admitted_can_admit=OFF + end_on_host_leave=ON
   *   → the meeting is ended when the host leaves, so any subsequent join
   *     attempt should get a non-200 response (meeting not active / not found),
   *     but NOT the JOINING_NOT_ALLOWED error specifically.
   *
   * This validates that the correct code path is exercised in each branch.
   */
  test("join after host leaves with end_on_host_leave=ON does not return JOINING_NOT_ALLOWED", async () => {
    const meetingId = `e2e_hgjc_eohl_${Date.now()}`;
    const hostEmail = "hgjc-eohl-host@videocall.rs";
    const hostName = "HGJCEOHLHost";
    const lateEmail = "hgjc-eohl-late@videocall.rs";
    const lateName = "HGJCEOHLLate";

    await createMeeting(hostEmail, hostName, meetingId);
    await updateMeeting(hostEmail, hostName, meetingId, {
      admitted_can_admit: false,
      end_on_host_leave: true,
      waiting_room_enabled: false,
    });

    const hostJoinRes = await joinMeetingRaw(hostEmail, hostName, meetingId);
    expect(hostJoinRes.ok, "host join should succeed").toBe(true);

    // Host leaves → meeting should be ended by the server.
    await leaveMeeting(hostEmail, hostName, meetingId);

    // The late joiner should not see JOINING_NOT_ALLOWED; the meeting is ended.
    const lateJoinRes = await joinMeetingRaw(lateEmail, lateName, meetingId);
    expect(lateJoinRes.ok).toBe(false);
    const text = await lateJoinRes.text();
    expect(text).not.toContain("JOINING_NOT_ALLOWED");
  });
});
