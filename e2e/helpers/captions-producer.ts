import { resolve } from "node:path";
import { connect, NatsConnection } from "nats";
import protobuf from "protobufjs";

const dir = resolve(__dirname, "../../protobuf/types");
const protos = protobuf.loadSync([`${dir}/caption_packet.proto`, `${dir}/packet_wrapper.proto`]);
const Caption = protos.lookupType("CaptionPacket");
const Wrapper = protos.lookupType("PacketWrapper");
export const KIND = protos.lookupEnum("CaptionPacket.Kind").values;

/** Publishes what the captions service would on `room.{room}.captions`. */
export class CaptionsProducer {
  private constructor(
    readonly nc: NatsConnection,
    readonly room: string,
  ) {}

  static async start(room: string): Promise<CaptionsProducer> {
    const servers = `nats://127.0.0.1:${process.env.E2E_NATS_HOST_PORT || 14222}`;
    return new CaptionsProducer(await connect({ servers }), room);
  }

  async send(fields: Record<string, unknown>): Promise<void> {
    const data = Caption.encode(Caption.fromObject(fields)).finish();
    const wrapper = Wrapper.fromObject({
      packetType: protos.lookupEnum("PacketWrapper.PacketType").values.CAPTION,
      userId: Buffer.from("system-&^%$#@!"),
      data,
    });
    this.nc.publish(`room.${this.room}.captions`, Wrapper.encode(wrapper).finish());
    await this.nc.flush();
  }

  status(kind: number, startedBy = ""): Promise<void> {
    return this.send({ kind, startedBy: Buffer.from(startedBy) });
  }
}
