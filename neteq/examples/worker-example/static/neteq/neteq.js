const workerUrl = '/neteq/neteq_worker.js';
const processorUrl = '/neteq/neteq_processor.js';

class NetEq {
  wasmUrl;
  wasmJsUrl;
  onStatsCb;
  worker;

  constructor({ wasmUrl, wasmJsUrl }) {
    this.wasmUrl = wasmUrl;
    this.wasmJsUrl = wasmJsUrl;
  }

  async init(audioCtx) {
    await audioCtx.audioWorklet.addModule(processorUrl);
    const node = new AudioWorkletNode(audioCtx, 'neteq-processor', {
      processorOptions: { bufferLength: 2 },
    });
    node.connect(audioCtx.destination);

    this.worker = new Worker(workerUrl, { type: 'module' });
    this.worker.postMessage(
      {
        type: 'init',
        port: node.port,
        wasmUrl: this.wasmUrl,
        wasmJsUrl: this.wasmJsUrl,
      },
      [node.port],
    );

    this.worker.onmessage = (e) => {
      switch (e.data.type) {
        case 'stats':
          if (this.onStatsCb) {
            this.onStatsCb({
              bufferSizeMs: e.data.bufferSizeMs,
              targetDelayMs: e.data.targetDelayMs,
            });
          }
      }
    };
  }

  onStats(cb) {
    this.onStatsCb = cb;
  }

  insertPacket({ seq, timestamp, payload, durationMs }) {
    this.worker.postMessage({
      type: 'insert',
      seq,
      timestamp,
      payload,
      durationMs,
    });
  }
}

export { NetEq };
