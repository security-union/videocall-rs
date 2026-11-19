/**
 * Microphone capture AudioWorklet: forwards mono PCM to the encoder worker.
 *
 * No encoding happens here. The audio thread has a hard deadline every
 * render quantum, so the worklet only copies samples into a batch and posts
 * it over a MessagePort that leads straight to the audio_encoder_worker
 * (no main-thread hop). The worker resamples to 48 kHz, cuts 20 ms frames
 * and encodes Opus.
 *
 * The node is created with channelCount 1 and channelCountMode 'explicit',
 * so the browser has already downmixed the input to mono.
 *
 * Protocol over the attached port:
 *   worklet -> worker: a Float32Array of BATCH_FRAMES samples at the
 *                      context's sample rate, its buffer transferred
 *   worklet -> worker: a number, the samples dropped since the last batch
 *                      (sent just before the next batch), so the worker's
 *                      frame index keeps following real time
 *   worker -> worklet: the same Float32Array, transferred back for reuse
 * Recycling the batches means steady capture allocates no sample memory.
 * If the worker falls behind (MAX_OUTSTANDING batches unreturned, about
 * 85 ms) and the pool is empty, new audio is dropped instead of allocating
 * more buffers for a queue nobody drains. The main thread attaches the port
 * only once the worker is ready, so this is a stall guard, not a startup
 * cost.
 *
 * Messages on this node's own port (from the main thread):
 *   { command: 'attach' } with one transferred MessagePort (to the worker)
 *   { command: 'close' }  stop processing; the node can be garbage collected
 */

// 256 frames is 5.3 ms at 48 kHz: two render quanta per message, so the
// worker sees about 190 messages a second, and batching adds at most 5 ms
// to the 20 ms an Opus frame waits for anyway.
const BATCH_FRAMES = 256;
const MAX_POOLED = 8;
const MAX_OUTSTANDING = 16;

class MicCaptureProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this.sink = null;
    this.closed = false;
    this.pool = [];
    this.batch = null;
    this.filled = 0;
    this.outstanding = 0;
    this.dropped = 0;
    this.port.onmessage = (event) => {
      const data = event.data;
      if (!data) return;
      if (data.command === 'attach' && event.ports.length > 0) {
        this.sink = event.ports[0];
        this.sink.onmessage = (reply) => {
          const buffer = reply.data;
          if (this.outstanding > 0) this.outstanding--;
          if (buffer instanceof Float32Array &&
              buffer.length === BATCH_FRAMES &&
              this.pool.length < MAX_POOLED) {
            this.pool.push(buffer);
          }
        };
      } else if (data.command === 'close') {
        this.closed = true;
        if (this.sink) {
          this.sink.onmessage = null;
          this.sink.close();
          this.sink = null;
        }
      }
    };
  }

  process(inputs) {
    if (this.closed) return false;
    const input = inputs[0];
    // No sink yet, or the source is disconnected: nothing to forward.
    if (!this.sink || !input || input.length === 0) return true;
    const samples = input[0];
    let offset = 0;
    while (offset < samples.length) {
      if (!this.batch) {
        this.batch = this.pool.pop() || null;
        if (!this.batch) {
          // Worker stalled: drop the rest of this quantum rather than grow
          // the queue, and remember how much so the worker can account.
          if (this.outstanding >= MAX_OUTSTANDING) {
            this.dropped += samples.length - offset;
            return true;
          }
          this.batch = new Float32Array(BATCH_FRAMES);
        }
        this.filled = 0;
        if (this.dropped > 0) {
          // Ordered on the port: the gap lands before this batch's samples.
          this.sink.postMessage(this.dropped);
          this.dropped = 0;
        }
      }
      const count = Math.min(BATCH_FRAMES - this.filled, samples.length - offset);
      for (let i = 0; i < count; i++) {
        this.batch[this.filled + i] = samples[offset + i];
      }
      this.filled += count;
      offset += count;
      if (this.filled === BATCH_FRAMES) {
        this.sink.postMessage(this.batch, [this.batch.buffer]);
        this.outstanding++;
        this.batch = null;
      }
    }
    return true;
  }
}

registerProcessor('mic-capture', MicCaptureProcessor);
