/**
 * Ultra-Fast PCM Audio Player Worklet
 * 
 * Highly optimized JavaScript implementation designed for low-end Android devices.
 * Uses advanced optimizations to minimize GC pressure and maximize performance.
 * 
 * This worklet receives PCM audio data via postMessage and plays it directly
 * through the AudioContext.destination() without using MediaStream APIs.
 * This approach is Safari-compatible and avoids the problematic MediaStreamTrackGenerator
 * and related APIs that cause issues in WebKit.
 * 
 * Usage:
 * 1. Load this worklet in an AudioContext
 * 2. Send PCM data via port.postMessage({ command: 'play', pcm: Float32Array })
 * 3. The worklet buffers and plays the audio directly

 */

// Ultra-fast circular buffer implementation  
class UltraFastPCMBuffer {
    constructor(maxSamples, channels) {
        this.maxSamples = maxSamples;
        this.channels = channels;
        this.totalSize = maxSamples * channels;
        
        // Pre-allocate all buffers to avoid GC during playback
        this.buffer = new Float32Array(this.totalSize);
        this.readPos = 0;
        this.writePos = 0;
        this.availableSamples = 0;
        
        console.log(`Ultra-fast PCM buffer: ${maxSamples} samples (~${(maxSamples/48000*1000).toFixed(1)}ms), ${channels} channels`);
    }
    
    // Drop the oldest samples to make room (advance read pointer).
    // Only used as a last resort when the buffer is completely full.
    dropOldestSamples(samplesToDrop) {
        if (samplesToDrop <= 0) return 0;
        const drop = Math.min(samplesToDrop, this.availableSamples);
        this.readPos = (this.readPos + drop) % this.maxSamples;
        this.availableSamples -= drop;
        return drop;
    }

    // Ensure there is physical room for an incoming frame.
    // Only drops samples when the buffer is literally full — the gradual
    // speedup in process() keeps the level from reaching this point under
    // normal conditions.
    ensureCapacityFor(frameLength) {
        const overflow = (this.availableSamples + frameLength) - this.maxSamples;
        if (overflow > 0) {
            return this.dropOldestSamples(overflow);
        }
        return 0;
    }

    // Read `count` samples from the circular buffer using linear
    // interpolation at the given playback rate.  Returns the number of
    // source samples actually consumed (may differ from `count` when
    // rate != 1.0).
    pullResampledMono(out, count, rate) {
        // How many source samples we need (fractional)
        const srcNeeded = Math.ceil(count * rate) + 1;
        if (this.availableSamples < srcNeeded) {
            out.fill(0);
            return 0;
        }

        let srcPos = 0.0; // fractional position into source
        for (let i = 0; i < count; i++) {
            const idx = Math.floor(srcPos);
            const frac = srcPos - idx;
            const a = this.buffer[(this.readPos + idx) % this.maxSamples];
            const b = this.buffer[(this.readPos + idx + 1) % this.maxSamples];
            out[i] = a + (b - a) * frac;
            srcPos += rate;
        }

        const consumed = Math.floor(srcPos);
        this.readPos = (this.readPos + consumed) % this.maxSamples;
        this.availableSamples -= consumed;
        return consumed;
    }

    // Optimized push with minimal branching
    pushInterleaved(data) {
        const frameLength = data.length / this.channels;
        
        if (this.availableSamples + frameLength > this.maxSamples) {
            return false; // Caller should ensure capacity first
        }
        
        if (this.channels === 1) {
            // Mono: direct copy using fastest possible method
            const writeStart = this.writePos;
            if (writeStart + frameLength <= this.maxSamples) {
                // Fast path: no wraparound
                this.buffer.set(data, writeStart);
            } else {
                // Wraparound: split copy
                const firstChunk = this.maxSamples - writeStart;
                this.buffer.set(data.subarray(0, firstChunk), writeStart);
                this.buffer.set(data.subarray(firstChunk), 0);
            }
        } else {
            // Stereo: deinterleave to planar format for faster processing
            const channelOffset = this.maxSamples;
            for (let i = 0; i < frameLength; i++) {
                const writeIndex = (this.writePos + i) % this.maxSamples;
                this.buffer[writeIndex] = data[i * 2];         // Left
                this.buffer[channelOffset + writeIndex] = data[i * 2 + 1]; // Right
            }
        }
        
        this.writePos = (this.writePos + frameLength) % this.maxSamples;
        this.availableSamples += frameLength;
        return true;
    }
    
    // Ultra-fast pull with SIMD-friendly operations
    pullToChannels(leftOut, rightOut) {
        const frameLength = leftOut.length;
        
        if (this.availableSamples < frameLength) {
            leftOut.fill(0);
            if (this.channels > 1) rightOut.fill(0);
            return false;
        }
        
        if (this.channels === 1) {
            // Mono: copy to left, duplicate to right if needed
            if (this.readPos + frameLength <= this.maxSamples) {
                leftOut.set(this.buffer.subarray(this.readPos, this.readPos + frameLength));
            } else {
                const firstChunk = this.maxSamples - this.readPos;
                leftOut.set(this.buffer.subarray(this.readPos, this.maxSamples));
                leftOut.set(this.buffer.subarray(0, frameLength - firstChunk), firstChunk);
            }
            if (rightOut !== leftOut) rightOut.set(leftOut);
        } else {
            // Stereo: extract from planar format
            const channelOffset = this.maxSamples;
            if (this.readPos + frameLength <= this.maxSamples) {
                leftOut.set(this.buffer.subarray(this.readPos, this.readPos + frameLength));
                rightOut.set(this.buffer.subarray(channelOffset + this.readPos, channelOffset + this.readPos + frameLength));
            } else {
                const firstChunk = this.maxSamples - this.readPos;
                leftOut.set(this.buffer.subarray(this.readPos, this.maxSamples));
                leftOut.set(this.buffer.subarray(0, frameLength - firstChunk), firstChunk);
                rightOut.set(this.buffer.subarray(channelOffset + this.readPos, channelOffset + this.maxSamples));
                rightOut.set(this.buffer.subarray(channelOffset, channelOffset + frameLength - firstChunk), firstChunk);
            }
        }
        
        this.readPos = (this.readPos + frameLength) % this.maxSamples;
        this.availableSamples -= frameLength;
        return true;
    }
    
    reset() {
        this.readPos = 0;
        this.writePos = 0;
        this.availableSamples = 0;
    }
}

class PCMPlayerProcessor extends AudioWorkletProcessor {
    constructor() {
        super();

        // Initialize with optimized settings for low-end devices
        this.buffer = new UltraFastPCMBuffer(4096 * 3, 1); // ~256ms at 48kHz
        this.sampleRate = 48000;
        this.channels = 1;

        // Gradual-speedup configuration (replaces burst-drop policy).
        // When the buffer fill level exceeds targetLevel, playback rate
        // increases linearly up to maxRate.  This drains the excess
        // smoothly without audible gaps.
        this.targetLevel = 0.40;  // 40% full = ~102ms — ideal operating point
        this.speedupStart = 0.55; // start speeding up above 55%
        this.maxRate = 1.08;      // never exceed 8% speedup (inaudible)
        this.lastDropWarnTime = 0;
        this.dropWarnCooldownMs = 1000;

        console.log('PCM Player Worklet initialized (gradual-speedup mode)');

        // Setup message handler
        this.port.onmessage = (event) => this.handleMessage(event.data);
    }
    
    handleMessage(data) {
        const { command, pcm, sampleRate, channels } = data;
        
        switch (command) {
            case 'configure':
                this.sampleRate = sampleRate || 48000;
                this.channels = channels || 2;
                // Recreate buffer with new configuration
                this.buffer = new UltraFastPCMBuffer(4096 * 2, this.channels);
                break;
                
            case 'play':
                if (pcm && pcm instanceof Float32Array) {
                    const frameLength = pcm.length / this.channels;
                    // Only drop when physically full — the gradual speedup in
                    // process() normally prevents us from reaching this point.
                    const dropped = this.buffer.ensureCapacityFor(frameLength);

                    if (dropped > 0) {
                        const now = Date.now();
                        if (now - this.lastDropWarnTime >= this.dropWarnCooldownMs) {
                            console.warn(`PCM buffer overflow: dropped ${dropped} samples (avail=${this.buffer.availableSamples}/${this.buffer.maxSamples})`);
                            this.lastDropWarnTime = now;
                        }
                    }

                    this.buffer.pushInterleaved(pcm);
                }
                break;
                
            case 'flush':
                this.buffer.reset();
                break;
        }
    }
    
    /**
     * Compute the playback rate based on current buffer fullness.
     * Returns 1.0 when the buffer is at or below the target level, and
     * ramps linearly up to this.maxRate as it approaches 100%.
     */
    playbackRate() {
        const fill = this.buffer.availableSamples / this.buffer.maxSamples;
        if (fill <= this.speedupStart) return 1.0;
        // Linear ramp: speedupStart → 1.0 fill  maps to  1.0 → maxRate
        const t = (fill - this.speedupStart) / (1.0 - this.speedupStart);
        return 1.0 + t * (this.maxRate - 1.0);
    }

    /**
     * Process audio — called by Web Audio API at ~375Hz (128 samples/call).
     * When the buffer is growing, we consume slightly more source samples
     * per call (up to 8% faster) via linear interpolation, draining the
     * excess smoothly instead of dropping chunks.
     */
    process(inputs, outputs, parameters) {
        const output = outputs[0];
        const left = output[0];
        const count = left.length; // typically 128

        if (this.channels === 1 || output.length < 2) {
            const rate = this.playbackRate();
            this.buffer.pullResampledMono(left, count, rate);
            if (output.length >= 2) output[1].set(left);
        } else {
            // Stereo: fall back to original 1:1 pull (speedup not yet
            // implemented for stereo — rare path for this project).
            this.buffer.pullToChannels(left, output[1]);
        }

        return true;
    }
    
    static get parameterDescriptors() {
        return [];
    }
}

registerProcessor('pcm-player', PCMPlayerProcessor);
