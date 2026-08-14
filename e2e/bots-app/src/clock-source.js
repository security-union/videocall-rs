(() => {
  "use strict";

  if (globalThis.top !== globalThis.self) return;

  const WIDTH = 640;
  const HEIGHT = 480;

  // The layout below is authored against this reference frame and scaled to
  // WIDTH/HEIGHT, so changing the capture size cannot move text off-canvas.
  // At the reference size every scale is exactly 1, so rendering is unchanged.
  const LAYOUT_WIDTH = 1280;
  const LAYOUT_HEIGHT = 720;
  const SCALE_X = WIDTH / LAYOUT_WIDTH;
  const SCALE_Y = HEIGHT / LAYOUT_HEIGHT;
  // Fonts take the smaller scale so glyphs never distort or overflow the width.
  const SCALE_FONT = Math.min(SCALE_X, SCALE_Y);
  const BAR_W = 28 * SCALE_X;
  // Hoisted: the refactor turned these into per-frame template-literal builds
  // and float multiplies. Immaterial next to the canvas fill, but free to avoid.
  const MAXW_TIME = WIDTH - 160 * SCALE_X;
  // The time string is the one WIDTH-constrained element: 12 fixed monospace
  // characters ("HH:MM:SS.mmm"). Size it to the larger of what the height allows
  // and what the width fits, rather than `SCALE_FONT`.
  //
  // Why not simply scale by SCALE_Y (which would preserve the legibility fraction
  // exactly): at 640x480 that is 90.7px, and 12 monospace chars is ~653px against
  // a 640px frame — it does not fit at ANY padding, including zero. So full parity
  // is geometrically impossible for this string at 4:3; this recovers ~86% of the
  // reference fraction (16.2% of frame height vs 18.9%) instead of the 14.2% a
  // bare `min(SCALE_X, SCALE_Y)` gives. At the 1280x720 reference the height term
  // wins and the value is 136px exactly, so commit 1 stays byte-neutral.
  const TIME_CHARS = 12;
  const MONO_ADVANCE_EM = 0.6;
  const FONT_TIME_PX = Math.min(136 * SCALE_Y, MAXW_TIME / (TIME_CHARS * MONO_ADVANCE_EM));
  const FONT_TIME = `700 ${FONT_TIME_PX}px monospace`;
  const FONT_DATE = `500 ${42 * SCALE_FONT}px sans-serif`;
  const FONT_NAME = `600 ${48 * SCALE_FONT}px sans-serif`;
  const Y_TIME = 330 * SCALE_Y;
  const Y_DATE = 465 * SCALE_Y;
  const Y_NAME = 585 * SCALE_Y;
  const MAXW_NAME = WIDTH - 120 * SCALE_X;
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
    context.fillRect(0, 0, BAR_W, HEIGHT);
    context.fillRect(WIDTH - BAR_W, 0, BAR_W, HEIGHT);

    context.textAlign = "center";
    context.textBaseline = "middle";
    context.fillStyle = "#ffffff";
    context.font = FONT_TIME;
    const milliseconds = String(now.getMilliseconds()).padStart(3, "0");
    // maxWidth keeps the time clear of the side bars with horizontal padding
    // (the full HH:MM:SS.mmm string in monospace otherwise clips the tile edges).
    context.fillText(`${cachedTime}.${milliseconds}`, WIDTH / 2, Y_TIME, MAXW_TIME);

    context.fillStyle = "rgba(255, 255, 255, 0.82)";
    context.font = FONT_DATE;
    context.fillText(cachedDate, WIDTH / 2, Y_DATE);

    const participant =
      typeof globalThis.__CLOCK_PARTICIPANT === "string" ? globalThis.__CLOCK_PARTICIPANT : "";
    if (participant !== "") {
      context.fillStyle = "rgba(255, 255, 255, 0.92)";
      context.font = FONT_NAME;
      context.fillText(participant, WIDTH / 2, Y_NAME, MAXW_NAME);
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
