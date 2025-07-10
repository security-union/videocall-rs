/**
 * Simple PCM Audio Player Worklet
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

class SimpleAudioBuffer {
    constructor(maxSamples = 2048, channels = 1) { // Reduce from 8192 to 2048 (~43ms at 48kHz)
        this.maxSamples = maxSamples;
        this.channels = channels;
        this.buffers = [];
        this.readPos = 0;
        this.writePos = 0;
        this.availableSamples = 0;
        
        // Initialize channel buffers
        for (let ch = 0; ch < channels; ch++) {
            this.buffers[ch] = new Float32Array(maxSamples);
        }
        
        console.log(`Safari PCM buffer initialized: ${maxSamples} samples (~${(maxSamples/48000*1000).toFixed(1)}ms at 48kHz)`);
    }
    
    /**
     * Add PCM data to the buffer
     */
    push(channelData, frameLength) {
        if (this.availableSamples + frameLength > this.maxSamples) {
            console.warn(`Safari PCM buffer full! Available: ${this.availableSamples}, trying to add: ${frameLength}, max: ${this.maxSamples}`);
            return false; // Buffer full
        }
        
        for (let ch = 0; ch < this.channels && ch < channelData.length; ch++) {
            const buffer = this.buffers[ch];
            const data = channelData[ch];
            
            for (let i = 0; i < frameLength; i++) {
                const writeIndex = (this.writePos + i) % this.maxSamples;
                buffer[writeIndex] = data[i] || 0;
            }
        }
        
        this.writePos = (this.writePos + frameLength) % this.maxSamples;
        this.availableSamples += frameLength;
        return true;
    }
    
    /**
     * Pull PCM data from the buffer
     */
    pull(outputChannels, frameLength) {
        if (this.availableSamples < frameLength) {
            // Not enough data, fill with silence
            for (let ch = 0; ch < outputChannels.length; ch++) {
                outputChannels[ch].fill(0);
            }
            return false;
        }
        
        for (let ch = 0; ch < outputChannels.length && ch < this.channels; ch++) {
            const buffer = this.buffers[ch];
            const output = outputChannels[ch];
            
            for (let i = 0; i < frameLength; i++) {
                const readIndex = (this.readPos + i) % this.maxSamples;
                output[i] = buffer[readIndex];
            }
        }
        
        this.readPos = (this.readPos + frameLength) % this.maxSamples;
        this.availableSamples -= frameLength;
        return true;
    }
    
    /**
     * Check if enough samples are available
     */
    isFrameAvailable(frameLength) {
        return this.availableSamples >= frameLength;
    }
    
    /**
     * Get buffer utilization percentage
     */
    getUtilization() {
        return (this.availableSamples / this.maxSamples) * 100;
    }
    
    /**
     * Clear the buffer
     */
    reset() {
        this.readPos = 0;
        this.writePos = 0;
        this.availableSamples = 0;
        
        for (let ch = 0; ch < this.channels; ch++) {
            this.buffers[ch].fill(0);
        }
    }
}

class PCMPlayerProcessor extends AudioWorkletProcessor {
    constructor() {
        super();
        
        // Audio buffer for incoming PCM data (smaller buffer for faster consumption)
        this.audioBuffer = new SimpleAudioBuffer(2048, 2); // Reduced from 8192 to 2048
        this.sampleRate = 48000; // Default, will be updated
        this.channels = 2; // Default, will be updated
        
        // Debug counters
        this.processCallCount = 0;
        this.lastLogTime = Date.now();
        this.samplesConsumed = 0;
        this.silenceFrames = 0;
        
        // Listen for PCM data from main thread
        this.port.onmessage = (event) => {
            const { command, pcm, sampleRate, channels } = event.data;
            
            switch (command) {
                case 'configure':
                    this.sampleRate = sampleRate || 48000;
                    this.channels = channels || 1;
                    this.audioBuffer = new SimpleAudioBuffer(2048, this.channels); // Keep small buffer
                    console.log(`Safari PCM worklet configured: ${this.sampleRate}Hz, ${this.channels} channels`);
                    break;
                    
                case 'play':
                    if (pcm && pcm instanceof Float32Array) {
                        const success = this.enqueuePCM(pcm);
                        if (!success) {
                            console.warn(`Safari PCM: Failed to enqueue ${pcm.length} samples - buffer full`);
                        }
                    }
                    break;
                    
                case 'flush':
                    this.audioBuffer.reset();
                    console.log('Safari PCM buffer flushed');
                    break;
            }
        };
        
        console.log('Safari PCMPlayerProcessor initialized');
    }
    
    /**
     * Enqueue PCM data for playback
     */
    enqueuePCM(pcmData) {
        const samplesPerChannel = pcmData.length / this.channels;
        
        // Convert interleaved PCM to channel arrays
        const channelData = [];
        for (let ch = 0; ch < this.channels; ch++) {
            channelData[ch] = new Float32Array(samplesPerChannel);
            for (let i = 0; i < samplesPerChannel; i++) {
                channelData[ch][i] = pcmData[i * this.channels + ch];
            }
        }
        
        // Push to buffer
        return this.audioBuffer.push(channelData, samplesPerChannel);
    }
    
    /**
     * Process audio - called by Web Audio API
     */
    process(inputs, outputs, parameters) {
        const output = outputs[0];
        const frameLength = output[0].length;
        
        this.processCallCount++;
        
        // Prepare output channel arrays
        const outputChannels = [];
        for (let ch = 0; ch < output.length; ch++) {
            outputChannels[ch] = output[ch];
        }
        
        // Try to pull audio from buffer
        if (this.audioBuffer.isFrameAvailable(frameLength)) {
            // Pull audio data from buffer
            this.audioBuffer.pull(outputChannels, frameLength);
            this.samplesConsumed += frameLength;
        } else {
            // No audio available, output silence
            for (let ch = 0; ch < output.length; ch++) {
                output[ch].fill(0);
            }
            this.silenceFrames++;
        }
        
        // Log statistics every 5 seconds
        const now = Date.now();
        if (now - this.lastLogTime > 5000) {
            const utilization = this.audioBuffer.getUtilization();
            const processRate = this.processCallCount / 5; // calls per second
            const consumptionRate = (this.samplesConsumed / this.sampleRate) / 5; // seconds of audio per second
            
            console.log(`Safari PCM stats: process=${processRate.toFixed(1)}/s, consumption=${consumptionRate.toFixed(2)}x realtime, buffer=${utilization.toFixed(1)}%, silence=${this.silenceFrames} frames`);
            
            // Reset counters
            this.processCallCount = 0;
            this.samplesConsumed = 0;
            this.silenceFrames = 0;
            this.lastLogTime = now;
        }
        
        // Keep processor alive
        return true;
    }
    
    static get parameterDescriptors() {
        return [];
    }
}

registerProcessor('pcm-player', PCMPlayerProcessor); 