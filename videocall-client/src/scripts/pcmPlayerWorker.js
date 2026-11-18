/**
 * PCM player AudioWorklet, paced pull from the peer's NetEq worker.
 *
 * The worklet owns playback timing. It keeps a small prebuffer and, as the
 * audio device consumes samples, asks the NetEq worker for more over a
 * MessagePort that connects the two directly (no main-thread hop). So NetEq's
 * GetAudio runs at the sound card's rate, which is what NetEq's delay
 * estimate assumes (one GetAudio per 10 ms of played audio).
 *
 * Protocol over the attached port:
 *   worklet -> worker: "send n more 10 ms frames", either the number n or a
 *                      Float32Array holding n in element 0. The array is a
 *                      buffer the worker sent before, transferred back so
 *                      the worker can reuse it.
 *   worker -> worklet: a Float32Array of n frames (interleaved, its buffer
 *                      transferred), or the number n meaning n frames of
 *                      silence
 * The worker answers every request with exactly the frames asked for, so
 * `pendingFrames` (asked, not yet answered) stays exact.
 *
 * Requests go out when buffered plus requested audio drops below
 * TARGET_FRAMES (30 ms, the worker's headroom to answer), and ask for at
 * least BATCH_FRAMES, so there is one round trip per 20 ms of audio rather
 * than per 10 ms. The cost is about 5 ms more average buffering.
 *
 * Returning the reply buffers means steady playback allocates no sample
 * memory. SharedArrayBuffer would avoid the messages entirely but needs
 * cross-origin isolation, which the app does not enable.
 *
 * Messages on this node's own port (from the main thread):
 *   { command: 'configure', channels }
 *   { command: 'attach' } with one transferred MessagePort (to the worker)
 *   { command: 'flush' }  drop buffered audio
 *   { command: 'close' }  stop processing; the node can be garbage collected
 *
 * process() allocates nothing: the ring buffer is preallocated, copies are
 * plain loops (no subarray views), and request buffers are recycled.
 */

// 10 ms at 48 kHz.
const FRAME_SAMPLES = 480;
// Low-water mark: audio buffered plus requested. Gives the worker about 25 ms
// to answer before the device runs dry, enough for a busy low-end phone.
const TARGET_FRAMES = 3;
// Least frames per request. Two halves the message rate; the ring then holds
// 30 to 50 ms instead of 30 to 40 ms.
const BATCH_FRAMES = 2;
// Ring capacity: 200 ms. Only reachable if the worker answers in bursts.
const CAPACITY_SAMPLES = 9600;
// If a request goes unanswered for this many render quanta (~500 ms), assume
// the worker restarted and forget what was in flight.
const PENDING_TIMEOUT_QUANTA = 188;
// Returned reply buffers kept for the next requests. Steady playback cycles
// one or two.
const MAX_SPARE_BUFFERS = 4;

class PcmRing {
    constructor(capacity, channels) {
        this.capacity = capacity;
        this.channels = channels;
        // Planar storage: channel c lives at [c * capacity, (c + 1) * capacity).
        this.data = new Float32Array(capacity * channels);
        this.readPos = 0;
        this.writePos = 0;
        this.available = 0;
    }

    reset() {
        this.readPos = 0;
        this.writePos = 0;
        this.available = 0;
    }

    dropOldest(count) {
        const drop = Math.min(count, this.available);
        this.readPos = (this.readPos + drop) % this.capacity;
        this.available -= drop;
    }

    // Append `frames` per channel from interleaved `pcm`, or zeros when `pcm`
    // is null. Drops the oldest audio if the ring would overflow.
    push(pcm, frames) {
        const overflow = this.available + frames - this.capacity;
        if (overflow > 0) this.dropOldest(overflow);
        const channels = this.channels;
        if (channels === 1) {
            // Plain loops: subarray() would allocate a view per call.
            const data = this.data;
            const first = Math.min(frames, this.capacity - this.writePos);
            const writePos = this.writePos;
            if (pcm) {
                for (let i = 0; i < first; i++) data[writePos + i] = pcm[i];
                for (let i = first; i < frames; i++) data[i - first] = pcm[i];
            } else {
                data.fill(0, writePos, writePos + first);
                data.fill(0, 0, frames - first);
            }
        } else {
            for (let i = 0; i < frames; i++) {
                const index = (this.writePos + i) % this.capacity;
                for (let c = 0; c < channels; c++) {
                    this.data[c * this.capacity + index] = pcm ? pcm[i * channels + c] : 0;
                }
            }
        }
        this.writePos = (this.writePos + frames) % this.capacity;
        this.available += frames;
    }

    // Copy `out[c].length` samples per channel into `out`. Underruns fill the
    // rest with zeros.
    pull(out) {
        const length = out[0].length;
        const count = Math.min(length, this.available);
        const data = this.data;
        const first = Math.min(count, this.capacity - this.readPos);
        for (let c = 0; c < out.length; c++) {
            const channel = out[c];
            const offset = Math.min(c, this.channels - 1) * this.capacity;
            // Plain loops: subarray() would allocate a view per quantum.
            const start = offset + this.readPos;
            for (let i = 0; i < first; i++) channel[i] = data[start + i];
            for (let i = first; i < count; i++) channel[i] = data[offset + i - first];
            if (count < length) channel.fill(0, count);
        }
        this.readPos = (this.readPos + count) % this.capacity;
        this.available -= count;
        return count === length;
    }
}

class PCMPlayerProcessor extends AudioWorkletProcessor {
    constructor() {
        super();
        this.channels = 1;
        this.ring = new PcmRing(CAPACITY_SAMPLES, this.channels);
        this.neteqPort = null;
        this.pendingFrames = 0;
        this.quantaSinceAnswer = 0;
        // Reply buffers to hand back to the worker with the next requests.
        this.spare = [];
        // Reused one-element transfer list, so a request allocates no array.
        this.transfer = [null];
        this.closed = false;
        this.port.onmessage = (event) => this.handleMessage(event);
    }

    handleMessage(event) {
        const data = event.data || {};
        switch (data.command) {
            case 'configure': {
                const channels = data.channels === 2 ? 2 : 1;
                if (channels !== this.channels) {
                    this.channels = channels;
                    this.ring = new PcmRing(CAPACITY_SAMPLES, channels);
                }
                break;
            }
            case 'attach': {
                const port = event.ports && event.ports[0];
                if (!port) break;
                if (this.neteqPort) this.neteqPort.close();
                this.neteqPort = port;
                this.neteqPort.onmessage = (e) => this.handleFrames(e.data);
                this.pendingFrames = 0;
                this.quantaSinceAnswer = 0;
                this.ring.reset();
                this.requestFrames();
                break;
            }
            case 'flush':
                this.ring.reset();
                break;
            case 'close':
                this.closed = true;
                if (this.neteqPort) {
                    this.neteqPort.onmessage = null;
                    this.neteqPort.close();
                    this.neteqPort = null;
                }
                break;
        }
    }

    handleFrames(data) {
        const frameLength = FRAME_SAMPLES * this.channels;
        let frames;
        if (typeof data === 'number') {
            frames = data;
            this.ring.push(null, frames * FRAME_SAMPLES);
        } else if (data instanceof Float32Array) {
            frames = Math.floor(data.length / frameLength);
            this.ring.push(data, frames * FRAME_SAMPLES);
            if (data.length > 0 && this.spare.length < MAX_SPARE_BUFFERS) this.spare.push(data);
        } else {
            return;
        }
        this.pendingFrames = Math.max(0, this.pendingFrames - frames);
        this.quantaSinceAnswer = 0;
    }

    // Once buffered + requested drops below the target, ask for enough frames
    // to get back above it, and at least BATCH_FRAMES.
    requestFrames() {
        if (!this.neteqPort) return;
        const queued = this.ring.available + this.pendingFrames * FRAME_SAMPLES;
        const missing = TARGET_FRAMES * FRAME_SAMPLES - queued;
        if (missing <= 0) return;
        const frames = Math.max(BATCH_FRAMES, Math.ceil(missing / FRAME_SAMPLES));
        this.pendingFrames += frames;
        const buffer = this.spare.pop();
        if (buffer) {
            // Element 0 carries the count; the worker overwrites the rest.
            buffer[0] = frames;
            this.transfer[0] = buffer.buffer;
            this.neteqPort.postMessage(buffer, this.transfer);
            this.transfer[0] = null;
        } else {
            this.neteqPort.postMessage(frames);
        }
    }

    process(inputs, outputs) {
        if (this.closed) return false;
        const output = outputs[0];
        if (output && output.length > 0) {
            this.ring.pull(output);
        }
        if (this.pendingFrames > 0 && ++this.quantaSinceAnswer > PENDING_TIMEOUT_QUANTA) {
            this.pendingFrames = 0;
            this.quantaSinceAnswer = 0;
        }
        this.requestFrames();
        return true;
    }

    static get parameterDescriptors() {
        return [];
    }
}

registerProcessor('pcm-player', PCMPlayerProcessor);
