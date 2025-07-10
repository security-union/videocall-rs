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
    constructor(maxSamples = 8192, channels = 1) {
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
    }
    
    /**
     * Add PCM data to the buffer
     */
    push(channelData, frameLength) {
        if (this.availableSamples + frameLength > this.maxSamples) {
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
        
        // Audio buffer for incoming PCM data
        this.audioBuffer = new SimpleAudioBuffer(8192, 2); // 8192 samples, 2 channels max
        this.sampleRate = 48000; // Default, will be updated
        this.channels = 2; // Default, will be updated
        
        // Listen for PCM data from main thread
        this.port.onmessage = (event) => {
            const { command, pcm, sampleRate, channels } = event.data;
            
            switch (command) {
                case 'configure':
                    this.sampleRate = sampleRate || 48000;
                    this.channels = channels || 1;
                    this.audioBuffer = new SimpleAudioBuffer(8192, this.channels);
                    break;
                    
                case 'play':
                    if (pcm && pcm instanceof Float32Array) {
                        this.enqueuePCM(pcm);
                    }
                    break;
                    
                case 'flush':
                    this.audioBuffer.reset();
                    break;
            }
        };
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
        this.audioBuffer.push(channelData, samplesPerChannel);
    }
    
    /**
     * Process audio - called by Web Audio API
     */
    process(inputs, outputs, parameters) {
        const output = outputs[0];
        const frameLength = output[0].length;
        
        // Prepare output channel arrays
        const outputChannels = [];
        for (let ch = 0; ch < output.length; ch++) {
            outputChannels[ch] = output[ch];
        }
        
        // Try to pull audio from buffer
        if (this.audioBuffer.isFrameAvailable(frameLength)) {
            // Pull audio data from buffer
            this.audioBuffer.pull(outputChannels, frameLength);
        } else {
            // No audio available, output silence
            for (let ch = 0; ch < output.length; ch++) {
                output[ch].fill(0);
            }
        }
        
        // Keep processor alive
        return true;
    }
    
    static get parameterDescriptors() {
        return [];
    }
}

registerProcessor('pcm-player', PCMPlayerProcessor); 