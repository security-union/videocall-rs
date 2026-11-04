import { randomUUID } from "node:crypto";
import { resolve } from "node:path";
import protobuf from "protobufjs";
import { joinMeeting } from "./meeting-api";
import { HEARTBEAT_KEEPALIVE_INTERVAL_MS } from "./rust-mirrored-constants";

const RELAY_WS_URL = process.env.RELAY_WS_URL || "ws://localhost:8080";
const SESSION_ASSIGNED_TIMEOUT_MS = 15_000;

const dir = resolve(__dirname, "../../protobuf/types");
const protos = protobuf.loadSync([`${dir}/media_packet.proto`, `${dir}/packet_wrapper.proto`]);
const Media = protos.lookupType("MediaPacket");
const Wrapper = protos.lookupType("PacketWrapper");
const PACKET_TYPE = protos.lookupEnum("PacketWrapper.PacketType").values;
const MEDIA_TYPE = protos.lookupEnum("MediaPacket.MediaType").values;
const TRANSPORT_TYPE = protos.lookupEnum("TransportType").values;

export interface RelayPeer {
  name: string;
  leave(): void;
}

function heartbeatFrame(email: string): Uint8Array {
  const userId = Buffer.from(email);
  const media = Media.fromObject({
    mediaType: MEDIA_TYPE.HEARTBEAT,
    userId,
    timestamp: Date.now(),
    heartbeatMetadata: { transportType: TRANSPORT_TYPE.TRANSPORT_WEBSOCKET },
  });
  const wrapper = Wrapper.fromObject({
    packetType: PACKET_TYPE.MEDIA,
    userId,
    data: Media.encode(media).finish(),
  });
  return Wrapper.encode(wrapper).finish();
}

function isSessionAssigned(event: MessageEvent): boolean {
  const packet = Wrapper.toObject(Wrapper.decode(new Uint8Array(event.data as ArrayBuffer)), {
    enums: Number,
  });
  return packet.packetType === PACKET_TYPE.SESSION_ASSIGNED;
}

function openAssignedSocket(token: string, name: string): Promise<WebSocket> {
  return new Promise((ok, fail) => {
    const socket = new WebSocket(
      `${RELAY_WS_URL}/lobby?token=${encodeURIComponent(token)}&instance_id=${randomUUID()}`,
    );
    socket.binaryType = "arraybuffer";
    const timer = setTimeout(() => {
      socket.close();
      fail(new Error(`${name}: no SESSION_ASSIGNED in ${SESSION_ASSIGNED_TIMEOUT_MS}ms`));
    }, SESSION_ASSIGNED_TIMEOUT_MS);
    socket.onmessage = (event) => {
      if (isSessionAssigned(event)) {
        clearTimeout(timer);
        socket.onmessage = null;
        ok(socket);
      }
    };
    socket.onerror = () => {
      clearTimeout(timer);
      fail(new Error(`${name}: the relay socket failed to open`));
    };
  });
}

/** A camera-off participant: a real relay session sending a browser's heartbeats and no media. */
export async function joinRelayPeer(meetingId: string, name: string): Promise<RelayPeer> {
  const email = `${name.toLowerCase()}@videocall.rs`;
  const { status, roomToken } = await joinMeeting(email, name, meetingId, name);
  if (!roomToken) {
    throw new Error(`${name}: the join returned no room token (status ${status})`);
  }
  const socket = await openAssignedSocket(roomToken, name);
  const beat = () => {
    if (socket.readyState === WebSocket.OPEN) socket.send(heartbeatFrame(email));
  };
  beat();
  const timer = setInterval(beat, HEARTBEAT_KEEPALIVE_INTERVAL_MS);
  return {
    name,
    leave() {
      clearInterval(timer);
      socket.close();
    },
  };
}
