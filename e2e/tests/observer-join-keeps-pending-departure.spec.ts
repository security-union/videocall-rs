import { test, expect } from "@playwright/test";
import jwt from "jsonwebtoken";
import { waitForServices } from "../helpers/wait-for-services";
import { createMeeting, fetchMeetingState } from "../helpers/meeting-api";
import { generateSessionToken } from "../helpers/auth";
import { relayReconnectGraceMs } from "../helpers/relay-constants";

// #3182: an observer JoinRoom must not consume a departed participant's pending
// departure (#1311 grace period), even when it presents that participant's instance_id.
// Driven with raw relay sockets because the real client always draws a random instance_id.

const OWNER_EMAIL = "observer-pending-owner@videocall.rs";
const OWNER_NAME = "ObserverPendingOwner";
const API_URL = process.env.API_BASE_URL || "http://localhost:8081";
const RELAY_WS_URL = process.env.RELAY_WS_URL || "ws://localhost:8080";
const JWT_SECRET = process.env.JWT_SECRET || "dev-jwt-secret-change-me";
const COOKIE_NAME = process.env.COOKIE_NAME || "session";

async function joinForRoomToken(meetingId: string): Promise<string> {
  const res = await fetch(`${API_URL}/api/v1/meetings/${meetingId}/join`, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Cookie: `${COOKIE_NAME}=${generateSessionToken(OWNER_EMAIL, OWNER_NAME)}`,
    },
    body: JSON.stringify({ display_name: OWNER_NAME }),
  });
  expect(res.ok, `join failed: ${res.status}`).toBe(true);
  const body = (await res.json()) as { result: { room_token?: string } };
  const token = body.result.room_token;
  expect(token, "the join must return a room token").toBeTruthy();
  return token as string;
}

function observerTokenFor(roomToken: string): string {
  const claims = jwt.decode(roomToken) as { sub: string; room: string; display_name: string };
  const now = Math.floor(Date.now() / 1000);
  return jwt.sign(
    {
      sub: claims.sub,
      room: claims.room,
      room_join: false,
      is_host: false,
      is_guest: false,
      display_name: claims.display_name,
      observer: true,
      end_on_host_leave: true,
      exp: now + 600,
      iat: now,
      iss: "videocall-meeting-backend",
      typ: "room_access",
    },
    JWT_SECRET,
    { algorithm: "HS256" },
  );
}

function openRelaySocket(token: string, instanceId: string): Promise<WebSocket> {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(
      `${RELAY_WS_URL}/lobby?token=${encodeURIComponent(token)}&instance_id=${instanceId}`,
    );
    ws.binaryType = "arraybuffer";
    ws.onopen = () => resolve(ws);
    ws.onerror = () => reject(new Error("relay socket failed to open"));
  });
}

test.describe("Observer JoinRoom and the pending departure (#3182)", () => {
  test.beforeAll(async () => {
    await waitForServices();
  });

  test("an observer presenting a departed participant's instance_id does not cancel its departure", async () => {
    test.setTimeout(90_000);
    const meetingId = `e2e_3182_${Date.now()}`;
    const instanceId = `3182-${Date.now()}-predecessor`;
    await createMeeting(OWNER_EMAIL, OWNER_NAME, { meetingId, endOnHostLeave: false });

    const roomToken = await joinForRoomToken(meetingId);
    const sockets: WebSocket[] = [];
    try {
      const participant = await openRelaySocket(roomToken, instanceId);
      sockets.push(participant);
      await expect
        .poll(() => fetchMeetingState(OWNER_EMAIL, OWNER_NAME, meetingId), { timeout: 15_000 })
        .toBe("active");
      // The relay only activates a session on its first non-RTT packet.
      await new Promise((r) => setTimeout(r, 500));
      // PacketWrapper { packet_type: MEDIA, media_kind: VIDEO }.
      participant.send(new Uint8Array([0x08, 0x03, 0x30, 0x01]));
      await new Promise((r) => setTimeout(r, 1_000));

      const closed = new Promise((r) => (participant.onclose = r));
      const closeStartedAt = Date.now();
      participant.close();
      await closed;
      const observer = await openRelaySocket(observerTokenFor(roomToken), instanceId);
      sockets.push(observer);
      expect(
        Date.now() - closeStartedAt,
        "the observer must join well inside the grace period, or unfixed code also reaches idle",
      ).toBeLessThan(relayReconnectGraceMs() / 2);

      await expect
        .poll(() => fetchMeetingState(OWNER_EMAIL, OWNER_NAME, meetingId), {
          timeout: 30_000,
          intervals: [1_000],
          message:
            "the grace period must still run out and drain the room while the observer stays connected",
        })
        .toBe("idle");
      expect(observer.readyState, "the observer must have stayed connected throughout").toBe(
        WebSocket.OPEN,
      );
    } finally {
      sockets.forEach((ws) => ws.close());
    }
  });
});
