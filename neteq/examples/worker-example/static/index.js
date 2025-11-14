import { NetEq } from './neteq/neteq.js';
import { Streaming } from './streaming.js';

const urlInput = document.getElementById('url');
const startButton = document.getElementById('start');

const jitterInput = document.getElementById('jitter');
const packetLossInput = document.getElementById('packet-loss');
const configureButton = document.getElementById('configure');

const pauseDurationInput = document.getElementById('pause-duration');
const pausePacketsButton = document.getElementById('pause-packets');

const targetDelayEl = document.getElementById('target-delay');
const bufferedAudioEl = document.getElementById('buffered-audio');

const audioCtx = new AudioContext();
audioCtx.suspend();

const streaming = new Streaming();

async function start() {
  const netEq = new NetEq({ wasmUrl: '/wasm/neteq_wasm_bg.wasm', wasmJsUrl: '/wasm/neteq_wasm.js', additionalDelayMs: 0 });
  await netEq.init(audioCtx);

  let lastStatUpdate = Date.now();
  netEq.onStats(({ bufferSizeMs, targetDelayMs }) => {
    if (Date.now() - lastStatUpdate < 500) {
      return;
    }

    lastStatUpdate = Date.now();

    targetDelayEl.innerText = `${Math.round(targetDelayMs)} ms`;
    bufferedAudioEl.innerText = `${Math.round(bufferSizeMs)} ms`;
  });

  streaming.onPacket((packet) => {
    netEq.insertPacket(packet);
  });

  streaming.connect();
}

let running = false;
startButton.addEventListener('click', async () => {
  if (!running) {
    const url = urlInput.value.trim();
    if (!url) {
      return alert('Please enter an audio URL.');
    }

    running = true;
    startButton.innerText = 'Stop';
    audioCtx.resume();
    streaming.startStream(url);
  } else {
    running = false;
    startButton.innerText = 'Start Streaming';
    audioCtx.suspend();
    streaming.stopStream();
  }
});

configureButton.addEventListener('click', async () => {
  const jitterMs = new Number(jitterInput.value.trim());
  if (Number.isNaN(jitterMs) || jitterMs < 0) {
    jitterMs = 0;
  }

  const packetLoss = new Number(packetLossInput.value.trim());
  if (Number.isNaN(packetLoss) || packetLoss < 0) {
    packetLoss = 0;
  } else if (packetLoss > 1) {
    packetLoss = 1;
  }

  streaming.configure({ jitterMs, packetLoss });
});

pausePacketsButton.addEventListener('click', async () => {
  const pauseDuration = new Number(pauseDurationInput.value.trim());
  if (Number.isNaN(pauseDuration) || pauseDuration < 0) {
    pauseDuration = 0;
  }

  streaming.pausePackets({ pauseMs: pauseDuration });
});

start();
