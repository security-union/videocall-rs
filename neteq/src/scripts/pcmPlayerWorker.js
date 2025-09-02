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
    
    // Drop the oldest samples to make room (advance read pointer)
    dropOldestSamples(samplesToDrop) {
        if (samplesToDrop <= 0) return 0;
        const drop = Math.min(samplesToDrop, this.availableSamples);
        this.readPos = (this.readPos + drop) % this.maxSamples;
        this.availableSamples -= drop;
        return drop;
    }

    // Ensure capacity for a frame by applying a burst-drop policy (drop oldest)
    // Returns number of samples dropped
    ensureCapacityFor(frameLength, highWatermark, lowWatermark) {
        // If we're above the high watermark, proactively reduce to low watermark
        let dropped = 0;
        if (this.availableSamples >= highWatermark) {
            const target = Math.max(0, Math.min(lowWatermark, this.maxSamples));
            const toDrop = this.availableSamples - target;
            dropped += this.dropOldestSamples(toDrop);
        }

        // If still not enough room for the incoming frame, drop just enough
        const overflow = (this.availableSamples + frameLength) - this.maxSamples;
        if (overflow > 0) {
            dropped += this.dropOldestSamples(overflow);
        }
        return dropped;
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
        this.buffer = new UltraFastPCMBuffer(6144 * 6, 1); // 85ms at 48kHz
        this.sampleRate = 48000;
        this.channels = 1;
        // Burst-drop policy configuration
        this.highWatermarkRatio = 0.80; // start dropping when >= 80% full
        this.lowWatermarkRatio = 0.50;  // drop down to 20%
        this.lastDropWarnTime = 0;
        this.dropWarnCooldownMs = 1000; // rate-limit warnings
        
        console.log('Ultra-Fast PCM Player Worklet initialized - JavaScript optimized for maximum performance');
        
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
                this.buffer = new UltraFastPCMBuffer(6144 * 4, this.channels);
                break;
                
            case 'play':
                if (pcm && pcm instanceof Float32Array) {
                    // Apply burst-drop policy before enqueuing
                    const frameLength = pcm.length / this.channels;
                    const highWM = Math.floor(this.buffer.maxSamples * this.highWatermarkRatio);
                    const lowWM = Math.floor(this.buffer.maxSamples * this.lowWatermarkRatio);
                    const dropped = this.buffer.ensureCapacityFor(frameLength, highWM, lowWM);

                    if (dropped > 0) {
                        const now = Date.now();
                        if (now - this.lastDropWarnTime >= this.dropWarnCooldownMs) {
                            console.warn(`PCM buffer high-watermark: dropped ${dropped} samples to cap latency (avail=${this.buffer.availableSamples}/${this.buffer.maxSamples})`);
                            this.lastDropWarnTime = now;
                        }
                    }

                    // Enqueue after ensuring capacity. If this still fails, frame is too large.
                    const success = this.buffer.pushInterleaved(pcm);
                    if (!success) {
                        // Extremely rare: frame larger than buffer capacity
                        const now = Date.now();
                        if (now - this.lastDropWarnTime >= this.dropWarnCooldownMs) {
                            console.warn('Failed to enqueue PCM data: frame larger than buffer capacity');
                            this.lastDropWarnTime = now;
                        }
                    }
                }
                break;
                
            case 'flush':
                this.buffer.reset();
                break;
        }
    }
    
    /**
     * Process audio - called by Web Audio API at ~375Hz  
     * This is the critical hot path - must be ultra-fast
     */
    process(inputs, outputs, parameters) {
        const output = outputs[0];
        
        if (output.length >= 2) {
            // Stereo output - use direct channel access for maximum speed
            this.buffer.pullToChannels(output[0], output[1]);
        } else {
            // Mono output - pass the same channel twice
            this.buffer.pullToChannels(output[0], output[0]);
        }
        
        return true;
    }
    
    static get parameterDescriptors() {
        return [];
    }
}

registerProcessor('pcm-player', PCMPlayerProcessor);
