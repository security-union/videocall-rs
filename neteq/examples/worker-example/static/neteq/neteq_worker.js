let netEq;
let audioPort;

let framesReturned = 0;
let bufferSizeMs = 0;
let targetDelayMs = 0;

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
  const { wasmUrl, wasmJsUrl, port } = config;

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
  // Opus decodes inside the wasm (ropus); no JS decoder is needed.
  netEq = new WebNetEq(48000, 1);
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
