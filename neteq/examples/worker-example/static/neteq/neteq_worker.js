import * as opus from 'https://cdn.jsdelivr.net/npm/opus-decoder@0.7.11/+esm';

let netEq;
let audioPort;

let framesReturned = 0;
let bufferSizeMs = 0;
let targetDelayMs = 0;

// The wasm will look for the opus decoder here
self['opus-decoder'] = opus;

function sendFrame({ willPlayInMs, maxPlaybackDelayMs }) {
  if (!netEq) {
    return;
  }

  const result = netEq.get_audio();
  audioPort.postMessage(result);

  framesReturned++;
  if (framesReturned % 10 === 0) {
    const stats = netEq.getStatistics();
    // re-calculate delay every 100ms
    bufferSizeMs = stats.current_buffer_size_ms + willPlayInMs;
    targetDelayMs = stats.target_delay_ms + maxPlaybackDelayMs;
  } else {
    bufferSizeMs -= 10;
  }

  self.postMessage({ type: 'stats', bufferSizeMs, targetDelayMs });
}

async function init(config) {
  const { wasmUrl, wasmJsUrl, port, additionalDelayMs } = config;

  audioPort = port;
  audioPort.onmessage = (e) => {
    if (e.data.type == 'request-frame') {
      sendFrame(e.data);
    }
  };

  const { initNetEq, initSync, WebNetEq } = await import(wasmJsUrl);

  const response = await fetch(wasmUrl);
  const bytes = await response.arrayBuffer();

  initSync(bytes);
  initNetEq();
  const ne = new WebNetEq(48000, 1, additionalDelayMs);
  await ne.init();
  netEq = ne;
}

self.onmessage = (event) => {
  switch (event.data?.type) {
    case 'init':
      init(event.data);

      break;
    case 'insert':
      if (!netEq) {
        return;
      }

      netEq.insert_packet(event.data.seq, event.data.timestamp, event.data.payload);
      bufferSizeMs += event.data.durationMs;

      break;
    case 'shutdown':
      if (netEq) {
        netEq.free();
        netEq = null;
      }
      if (audioPort) {
        audioPort.close();
        audioPort = null;
      }
      self.close();
  }
};
