import { connect } from "nats";

export const EVICT_INSTANCE_SUBJECT = "internal.evict_instance";

export interface EvictInstance {
  instanceId: string;
  room: string;
  userId: string;
  newSessionId: bigint;
  originRelay: bigint;
  newConnSeq: bigint;
}

/** The `room` and `sub` claims of the room token on a relay connect URL, and whether it joins the room (`room_join`). */
export function roomTokenClaims(url: string): { room: string; sub: string; joined: boolean } {
  const token = new URL(url).searchParams.get("token");
  if (!token) {
    throw new Error(`no token on connect URL ${url}`);
  }
  const claims = JSON.parse(Buffer.from(token.split(".")[1], "base64url").toString("utf8"));
  return { room: claims.room, sub: claims.sub, joined: claims.room_join === true };
}

/** A random non-zero u64 below 2^53, so it also survives a JS number round-trip. */
export function randomU64(): bigint {
  return BigInt(Math.floor(Math.random() * (Number.MAX_SAFE_INTEGER - 1)) + 1);
}

/** Publishes what another relay would on `internal.evict_instance`. */
export async function publishEvictInstance(evict: EvictInstance): Promise<void> {
  const servers = `nats://127.0.0.1:${process.env.E2E_NATS_HOST_PORT || 14222}`;
  const nc = await connect({ servers });
  try {
    const json =
      `{"instance_id":${JSON.stringify(evict.instanceId)},` +
      `"room":${JSON.stringify(evict.room)},` +
      `"user_id":${JSON.stringify(evict.userId)},` +
      `"new_session_id":${evict.newSessionId},` +
      `"origin_relay":${evict.originRelay},` +
      `"new_conn_seq":${evict.newConnSeq}}`;
    nc.publish(EVICT_INSTANCE_SUBJECT, new TextEncoder().encode(json));
    await nc.flush();
  } finally {
    await nc.close();
  }
}
