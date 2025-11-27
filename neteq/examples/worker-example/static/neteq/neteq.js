const workerUrl = '/neteq/neteq_worker.js';
const processorUrl = '/neteq/neteq_processor.js';

class NetEq {
  wasmUrl;
  wasmJsUrl;
  onStatsCb;
  worker;

  constructor({ wasmUrl, wasmJsUrl, additionalDelayMs }) {
    this.wasmUrl = wasmUrl;
    this.wasmJsUrl = wasmJsUrl;
    this.additionalDelayMs = additionalDelayMs || additionalDelayMs === 0 ? additionalDelayMs : 80;
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
        additionalDelayMs: this.additionalDelayMs,
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
