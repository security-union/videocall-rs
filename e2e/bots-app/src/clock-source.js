(() => {
  "use strict";

  if (globalThis.top !== globalThis.self) return;

  const WIDTH = 1280;
  const HEIGHT = 720;
  const FRAME_INTERVAL_MS = 33;
  const DIMENSION_TIMEOUT_MS = 5_000;
  const DIMENSION_POLL_MS = 50;

  const canvas = globalThis.document.createElement("canvas");
  canvas.width = WIDTH;
  canvas.height = HEIGHT;
  const context = canvas.getContext("2d");
  if (context === null) {
    throw new Error("clock source: could not create 2D canvas context");
  }

  const clockStream = canvas.captureStream(30);
  const baseVideoTrack = clockStream.getVideoTracks()[0];
  if (baseVideoTrack === undefined) {
    throw new Error("clock source: canvas capture produced no video track");
  }

  const AudioContextConstructor = globalThis.AudioContext ?? globalThis.webkitAudioContext;
  if (AudioContextConstructor === undefined) {
    throw new Error("clock source: AudioContext is unavailable");
  }
  const audioContext = new AudioContextConstructor();
  const audioDestination = audioContext.createMediaStreamDestination();
  const silentGain = audioContext.createGain();
  const oscillator = audioContext.createOscillator();
  silentGain.gain.value = 0;
  oscillator.connect(silentGain);
  silentGain.connect(audioDestination);
  oscillator.start();
  const baseAudioTrack = audioDestination.stream.getAudioTracks()[0];
  if (baseAudioTrack === undefined) {
    throw new Error("clock source: silent audio destination produced no track");
  }

  const timeFormatter = new globalThis.Intl.DateTimeFormat(undefined, {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });
  const dateFormatter = new globalThis.Intl.DateTimeFormat(undefined, {
    year: "numeric",
    month: "short",
    day: "2-digit",
  });

  let cachedSecond = -1;
  let cachedTime = "";
  let cachedDate = "";
  let cachedHue = 0;

  function drawClock() {
    const now = new Date();
    const second = Math.floor(now.getTime() / 1_000);
    if (second !== cachedSecond) {
      cachedSecond = second;
      cachedTime = timeFormatter.format(now);
      cachedDate = dateFormatter.format(now);
      cachedHue = (second * 47) % 360;
    }

    context.fillStyle = `hsl(${cachedHue} 58% 18%)`;
    context.fillRect(0, 0, WIDTH, HEIGHT);

    context.fillStyle = `hsl(${(cachedHue + 55) % 360} 72% 55%)`;
    context.fillRect(0, 0, 28, HEIGHT);
    context.fillRect(WIDTH - 28, 0, 28, HEIGHT);

    context.textAlign = "center";
    context.textBaseline = "middle";
    context.fillStyle = "#ffffff";
    context.font = "700 136px monospace";
    const milliseconds = String(now.getMilliseconds()).padStart(3, "0");
    // maxWidth keeps the time clear of the side bars with horizontal padding
    // (the full HH:MM:SS.mmm string in monospace otherwise clips the tile edges).
    context.fillText(`${cachedTime}.${milliseconds}`, WIDTH / 2, 330, WIDTH - 160);

    context.fillStyle = "rgba(255, 255, 255, 0.82)";
    context.font = "500 42px sans-serif";
    context.fillText(cachedDate, WIDTH / 2, 465);

    const participant =
      typeof globalThis.__CLOCK_PARTICIPANT === "string" ? globalThis.__CLOCK_PARTICIPANT : "";
    if (participant !== "") {
      context.fillStyle = "rgba(255, 255, 255, 0.92)";
      context.font = "600 48px sans-serif";
      context.fillText(participant, WIDTH / 2, 585, WIDTH - 120);
    }
  }

  function waitForDimensions(track) {
    const deadline = Date.now() + DIMENSION_TIMEOUT_MS;
    return new Promise((resolve, reject) => {
      function poll() {
        const settings = track.getSettings();
        if ((settings.width ?? 0) > 0 && (settings.height ?? 0) > 0) {
          resolve();
          return;
        }
        if (Date.now() >= deadline) {
          reject(new Error("clock getUserMedia: track never reported dimensions"));
          return;
        }
        globalThis.setTimeout(poll, DIMENSION_POLL_MS);
      }
      poll();
    });
  }

  drawClock();
  globalThis.setInterval(drawClock, FRAME_INTERVAL_MS);

  globalThis.navigator.mediaDevices.getUserMedia = async (constraints = {}) => {
    const tracks = [];
    if (constraints.video) {
      tracks.push(baseVideoTrack.clone());
    }
    if (constraints.audio) {
      if (audioContext.state === "suspended") {
        void audioContext.resume().catch(() => undefined);
      }
      tracks.push(baseAudioTrack.clone());
    }

    const stream = new globalThis.MediaStream(tracks);
    const videoTrack = stream.getVideoTracks()[0];
    if (videoTrack === undefined) {
      return stream;
    }

    try {
      await waitForDimensions(videoTrack);
      return stream;
    } catch (error) {
      for (const track of stream.getTracks()) {
        track.stop();
      }
      throw error;
    }
  };
})();
