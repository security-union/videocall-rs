/**
 * Polyfill for MediaStreamTrackProcessor for browsers that don't support it natively.
 * 
 * This implementation uses the older MediaStream Recording API as a fallback
 * and creates a similar interface to the MediaStreamTrackProcessor API.
 */

// Only apply polyfill if MediaStreamTrackProcessor is not supported
if (typeof MediaStreamTrackProcessor === 'undefined') {
  class ReadableStreamDefaultReader {
    constructor(stream) {
      this.stream = stream;
      this.frameQueue = [];
      this.resolveQueue = [];
      this.closed = false;
    }

    enqueueFrame(frame) {
      if (this.closed) return;
      
      if (this.resolveQueue.length > 0) {
        const resolve = this.resolveQueue.shift();
        resolve({ value: frame, done: false });
      } else {
        this.frameQueue.push(frame);
      }
    }

    read() {
      if (this.closed) {
        return Promise.resolve({ value: undefined, done: true });
      }

      if (this.frameQueue.length > 0) {
        const frame = this.frameQueue.shift();
        return Promise.resolve({ value: frame, done: false });
      }

      return new Promise(resolve => {
        this.resolveQueue.push(resolve);
      });
    }

    cancel() {
      this.closed = true;
      this.frameQueue = [];
      this.resolveQueue.forEach(resolve => resolve({ value: undefined, done: true }));
      this.resolveQueue = [];
    }
  }

  class ReadableStream {
    constructor(reader) {
      this.reader = reader;
    }

    getReader() {
      return this.reader;
    }
  }

  class MediaStreamTrackProcessorPolyfill {
    constructor(init) {
      this.track = init.track;
      this.reader = new ReadableStreamDefaultReader();
      this.readableStream = new ReadableStream(this.reader);
      this.setupTrackProcessor();
    }

    setupTrackProcessor() {
      if (!this.track) {
        console.error("MediaStreamTrackProcessor polyfill: No track provided");
        return;
      }

      // Create a MediaStream with just this track
      const stream = new MediaStream([this.track]);
      
      if (this.track.kind === 'video') {
        // For video tracks, use a video element and canvas to capture frames
        const video = document.createElement('video');
        video.srcObject = stream;
        video.autoplay = true;
        video.muted = true;
        video.style.display = 'none';
        document.body.appendChild(video);
        
        const canvas = document.createElement('canvas');
        const ctx = canvas.getContext('2d');
        
        // Set up canvas dimensions when video loads
        video.onloadedmetadata = () => {
          canvas.width = video.videoWidth;
          canvas.height = video.videoHeight;
        };
        
        // Process frames at regular intervals
        this.frameInterval = setInterval(() => {
          if (video.readyState < 2) return; // Not enough data
          
          ctx.drawImage(video, 0, 0);
          
          // Create a VideoFrame-like object
          // Since actual VideoFrame may not be available in browsers without MediaStreamTrackProcessor
          const imageData = ctx.getImageData(0, 0, canvas.width, canvas.height);
          const frameData = {
            // Mimic the VideoFrame interface enough for basic usage
            close: () => {},
            clone: () => frameData,
            timestamp: performance.now(),
            codedWidth: canvas.width,
            codedHeight: canvas.height,
            format: 'RGBA', // Approximate format
            _imageData: imageData // Store for internal use
          };
          
          this.reader.enqueueFrame(frameData);
        }, 1000/30); // Capture at ~30fps
        
        // Clean up when track ends
        this.track.onended = () => {
          clearInterval(this.frameInterval);
          video.remove();
          this.reader.cancel();
        };
      }
      else if (this.track.kind === 'audio') {
        // For audio tracks, use AudioContext to process audio
        const audioContext = new (window.AudioContext || window.webkitAudioContext)();
        const source = audioContext.createMediaStreamSource(stream);
        const processor = audioContext.createScriptProcessor(1024, 1, 1);
        
        processor.onaudioprocess = (e) => {
          const inputBuffer = e.inputBuffer;
          
          // Create an AudioData-like object
          const audioData = {
            // Mimic the AudioData interface enough for basic usage
            close: () => {},
            clone: () => audioData,
            timestamp: performance.now(),
            numberOfFrames: inputBuffer.length,
            numberOfChannels: inputBuffer.numberOfChannels,
            sampleRate: inputBuffer.sampleRate,
            format: 'f32-planar', // Approximate format
            _audioBuffer: inputBuffer // Store for internal use
          };
          
          this.reader.enqueueFrame(audioData);
        };
        
        source.connect(processor);
        processor.connect(audioContext.destination);
        
        // Clean up when track ends
        this.track.onended = () => {
          source.disconnect();
          processor.disconnect();
          this.reader.cancel();
        };
      }
    }

    readable() {
      return this.readableStream;
    }
  }

  // Add the polyfill to the global scope
  window.MediaStreamTrackProcessor = MediaStreamTrackProcessorPolyfill;
  window.MediaStreamTrackProcessorInit = function(track) {
    this.track = track;
  };
} 