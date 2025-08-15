/**
 * Ultra-Fast PCM Audio Player Worklet
 * 
 * Highly optimized JavaScript implementation designed for low-end Android devices.
 * Uses advanced optimizations to minimize GC pressure and maximize performance.
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
    
    // Optimized push with minimal branching
    pushInterleaved(data) {
        const frameLength = data.length / this.channels;
        
        if (this.availableSamples + frameLength > this.maxSamples) {
            return false; // Buffer full
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
        this.buffer = new UltraFastPCMBuffer(4096 * 2, 2); // 85ms at 48kHz
        this.sampleRate = 48000;
        this.channels = 2;
        
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
                this.buffer = new UltraFastPCMBuffer(4096 * 2, this.channels);
                break;
                
            case 'play':
                if (pcm && pcm instanceof Float32Array) {
                    const success = this.buffer.pushInterleaved(pcm);
                    // Silent failure handling - no console spam on low-end devices
                    if (!success) {
                        console.warn('Failed to enqueue PCM data');
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
