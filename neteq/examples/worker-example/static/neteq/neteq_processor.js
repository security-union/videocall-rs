// 48 kHz * 0.01 s = 480 samples per 10 ms frame
const FRAME_SIZE = 480;

class NetEqProcessor extends AudioWorkletProcessor {
  constructor(options) {
    super();

    // Number of frames that can be buffered (can be changed via options if needed)
    this.bufferSize = (options?.processorOptions?.bufferLength || 2) * FRAME_SIZE;

    // Internal ring buffer of Float32Array frames
    this.buffer = new Float32Array(this.bufferSize);
    // where to write the next frame
    this.writeI = 0;
    // where to read the next frame from
    this.readI = 0;
    this.pendingWriteI = 0;

    this.port.onmessage = (event) => {
      const samples = new Float32Array(event.data);

      this.buffer.set(samples, this.writeI % this.bufferSize);
      this.writeI += FRAME_SIZE;
    };
  }

  process(inputs, outputs, parameters) {
    const output = outputs[0];
    const out = output[0]; // mono

    for (let i = 0; i < out.length; i++) {
      if (this.readI < this.writeI) {
        out[i] = this.buffer[this.readI % this.bufferSize];
      } else {
        out[i] = 0;
      }
      this.readI++;
    }

    while (this.pendingWriteI <= this.readI + this.bufferSize - FRAME_SIZE) {
      this.port.postMessage({
        type: 'request-frame',
        willPlayInMs: (this.pendingWriteI - this.readI) / 48, // 48 frame per ms
        maxPlaybackDelayMs: (this.bufferSize - FRAME_SIZE) / 48,
      });
      this.pendingWriteI += FRAME_SIZE;
    }

    return true; // Keep processor alive
  }
}

registerProcessor('neteq-processor', NetEqProcessor);
