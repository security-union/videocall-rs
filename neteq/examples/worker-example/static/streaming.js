let sendPacketCb;

export class Streaming {
  socket;
  onPacketCb;

  constructor() {}

  onPacket(cb) {
    this.onPacketCb = cb;
  }

  async connect() {
    this.socket = new WebSocket((window.location.protocol === 'https:' ? 'wss://' : 'ws://') + window.location.host + '/stream');

    this.socket.addEventListener('open', () => {
      console.log('connected to server');
    });

    let seq = 0;
    this.socket.addEventListener('message', async (event) => {
      if (typeof event.data === 'string') {
        // Text frame → JSON
        console.log('JSON message:', event.data);
      } else {
        // Binary frame → ArrayBuffer or Blob
        const payload = new Uint8Array((await event.data.arrayBuffer?.()) || event.data);

        seq++;
        if (this.onPacketCb) {
          this.onPacketCb({
            seq,
            timestamp: seq * 960, // 48000Hz, 20ms: 960 sample per packet
            payload,
            durationMs: 20,
          });
        }
      }
    });

    this.socket.addEventListener('error', (error) => {
      console.error('WebSocket error:', error);
    });

    this.socket.addEventListener('close', () => {
      console.log('Connection closed');
    });
  }

  async startStream(url) {
    this.socket.send(JSON.stringify({ type: 'start', data: { url } }));
  }

  async stopStream() {
    this.socket.send(JSON.stringify({ type: 'stop' }));
  }

  async configure({ jitterMs, packetLoss }) {
    this.socket.send(JSON.stringify({ type: 'configure', data: { jitterMs, packetLoss } }));
  }

  async pausePackets({ pauseMs }) {
    console.log('pause', pauseMs);
    this.socket.send(JSON.stringify({ type: 'pause', data: { pauseMs } }));
  }
}
