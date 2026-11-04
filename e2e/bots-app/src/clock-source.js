(() => {
  "use strict";

  if (globalThis.top !== globalThis.self) return;

  // Injected per bot by bot.ts (#2236); each axis falls back on its own. The
  // fallback pair must equal SD_SOURCE in posture.ts — locked by clock-source.test.ts.
  const dimension = (value, fallback) =>
    typeof value === "number" && Number.isInteger(value) && value > 0 ? value : fallback;
  const WIDTH = dimension(globalThis.__CLOCK_WIDTH, 640);
  const HEIGHT = dimension(globalThis.__CLOCK_HEIGHT, 480);

  // The layout below is authored against this reference frame and scaled to
  // WIDTH/HEIGHT, so changing the capture size cannot move text off-canvas.
  // At the reference size every scale is exactly 1, so rendering is unchanged.
  const LAYOUT_WIDTH = 1280;
  const LAYOUT_HEIGHT = 720;
  const SCALE_X = WIDTH / LAYOUT_WIDTH;
  const SCALE_Y = HEIGHT / LAYOUT_HEIGHT;
  // Fonts take the smaller scale so glyphs never distort or overflow the width.
  const SCALE_FONT = Math.min(SCALE_X, SCALE_Y);
  const MARKER_BANDS_Y = [230 * SCALE_Y, 410 * SCALE_Y];
  const MARKER_BAND_H = 20 * SCALE_Y;
  // Hoisted: the refactor turned these into per-frame template-literal builds
  // and float multiplies. Immaterial next to the canvas fill, but free to avoid.
  const MAXW_TIME = WIDTH - 160 * SCALE_X;
  // The time's width is computable (fixed 12-char monospace string), so its font
  // size takes a width term; the variable-length rows rely on fillText maxWidth.
  const TIME_CHARS = 12;
  const MONO_ADVANCE_EM = 0.6;
  const FONT_TIME_PX = Math.min(136 * SCALE_Y, MAXW_TIME / (TIME_CHARS * MONO_ADVANCE_EM));
  const FONT_TIME = `700 ${FONT_TIME_PX}px monospace`;
  const FONT_DATE = `500 ${42 * SCALE_FONT}px sans-serif`;
  const FONT_NAME = `600 ${48 * SCALE_FONT}px sans-serif`;
  const Y_TIME = 330 * SCALE_Y;
  const Y_DATE = 465 * SCALE_Y;
  const Y_NAME = 585 * SCALE_Y;
  const MAXW_DATE = WIDTH - 120 * SCALE_X;
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

  const SPEECH_TICK_MS = 250;
  const SPEECH_LOOKAHEAD_SLACK_MS = 1_000;
  const speechStatus = (status) => globalThis.console.info(`[talking-clock] ${status}`);

  const positiveInteger = (value, fallback, min) =>
    typeof value === "number" && Number.isInteger(value) && value >= min ? value : fallback;

  function startTalkingClock() {
    const speech = globalThis.__CLOCK_SPEECH;
    const encodedClips = globalThis.__CLOCK_VOICE_CLIPS;
    if (speech === undefined || encodedClips === null || typeof encodedClips !== "object") {
      speechStatus("no voice");
      return;
    }
    const lookaheadMs = speech.MAX_LEAD_MS + SPEECH_LOOKAHEAD_SLACK_MS;
    const fleetIndex = positiveInteger(globalThis.__CLOCK_FLEET_INDEX, 0, 0);
    const fleetSize = positiveInteger(globalThis.__CLOCK_FLEET_SIZE, 1, 1);
    const clips = new Map();
    const durationOf = (clip) => clips.get(clip)?.duration;
    let scheduledTurn = null;
    let scheduledSlot = null;

    function play(clip, when) {
      const buffer = clips.get(clip);
      if (buffer === undefined || when < audioContext.currentTime) return;
      const source = audioContext.createBufferSource();
      source.buffer = buffer;
      source.connect(audioDestination);
      source.start(when);
    }

    function scheduleSpeech() {
      if (audioContext.state !== "running") return;
      const nowMs = Date.now();
      const contextTimeAt = (epochSeconds) =>
        audioContext.currentTime + (epochSeconds * 1_000 - nowMs) / 1_000;
      const horizon = (nowMs + lookaheadMs) / 1_000;
      const turnStart = speech.turnStartFor(Math.floor(horizon));
      if (turnStart !== scheduledTurn) {
        scheduledTurn = turnStart;
        const { speaker, lead } = speech.turnPlanFor(turnStart, fleetSize);
        if (speaker === fleetIndex) {
          for (const { clip, at } of speech.turnClips(turnStart, durationOf, lead)) {
            play(clip, contextTimeAt(turnStart) + at);
          }
        }
      }
      const slot = speech.backchannelSlotFor(horizon);
      if (slot !== scheduledSlot) {
        scheduledSlot = slot;
        const backchannel = speech.backchannelForSlot(slot, fleetSize);
        if (backchannel?.listener === fleetIndex) {
          play(backchannel.clip, contextTimeAt(backchannel.start));
        }
      }
    }

    speechStatus("decoding");
    const decodes = Object.entries(encodedClips).map(async ([clip, base64]) => {
      const bytes = Uint8Array.from(globalThis.atob(base64), (c) => c.charCodeAt(0));
      clips.set(clip, await audioContext.decodeAudioData(bytes.buffer));
    });
    void Promise.allSettled(decodes).then((results) => {
      const failed = results.filter((r) => r.status === "rejected").length;
      speechStatus(
        `ready: ${clips.size} clips decoded, ${failed} failed, speech model ${speech.model}`,
      );
      globalThis.setInterval(scheduleSpeech, SPEECH_TICK_MS);
    });
  }

  try {
    startTalkingClock();
  } catch (error) {
    speechStatus(
      `startTalkingClock failed: ${error instanceof Error ? error.message : String(error)}`,
    );
  }

  const CLOCK_LOCALE = "en-GB";
  const CLOCK_TIME_ZONE = "UTC";
  const timeFormatter = new globalThis.Intl.DateTimeFormat(CLOCK_LOCALE, {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hourCycle: "h23",
    timeZone: CLOCK_TIME_ZONE,
  });
  const dateFormatter = new globalThis.Intl.DateTimeFormat(CLOCK_LOCALE, {
    year: "numeric",
    month: "short",
    day: "2-digit",
    timeZone: CLOCK_TIME_ZONE,
  });

  let cachedSecond = -1;
  let cachedTime = "";
  let cachedDate = "";
  let cachedHue = 0;

  // Injected per bot by bot.ts (issue 3089); the fallback must equal SD_VIDEO_NOISE
  // in video-noise.ts — locked by clock-source.test.ts.
  const injectedNoise = globalThis.__CLOCK_VIDEO_NOISE;
  const NOISE_ALPHA =
    typeof injectedNoise === "number" && injectedNoise >= 0 && injectedNoise <= 1
      ? injectedNoise
      : 0.1;
  const NOISE_TILE_COUNT = 8;
  const NOISE_PAD = 64;
  const NOISE_CELL = 6;

  function noiseTile() {
    const tile = globalThis.document.createElement("canvas");
    tile.width = WIDTH + NOISE_PAD;
    tile.height = HEIGHT + NOISE_PAD;
    const tileContext = tile.getContext("2d");
    const image = tileContext.createImageData(tile.width, tile.height);
    const gridWidth = Math.ceil(tile.width / NOISE_CELL) + 2;
    const grid = new Float32Array(gridWidth * (Math.ceil(tile.height / NOISE_CELL) + 2));
    for (let i = 0; i < grid.length; i++) grid[i] = Math.random();
    const data = image.data;
    for (let y = 0; y < tile.height; y++) {
      const y0 = Math.floor(y / NOISE_CELL);
      const fy = y / NOISE_CELL - y0;
      for (let x = 0; x < tile.width; x++) {
        const x0 = Math.floor(x / NOISE_CELL);
        const fx = x / NOISE_CELL - x0;
        const top = y0 * gridWidth + x0;
        const bottom = top + gridWidth;
        const texture =
          (grid[top] * (1 - fx) + grid[top + 1] * fx) * (1 - fy) +
          (grid[bottom] * (1 - fx) + grid[bottom + 1] * fx) * fy;
        const i = (y * tile.width + x) * 4;
        for (let channel = 0; channel < 3; channel++) {
          data[i + channel] = 255 * (0.5 * texture + 0.5 * Math.random());
        }
        data[i + 3] = 255;
      }
    }
    tileContext.putImageData(image, 0, 0);
    return tile;
  }

  let noiseTiles = [];
  if (NOISE_ALPHA > 0) {
    try {
      noiseTiles = Array.from({ length: NOISE_TILE_COUNT }, () => noiseTile());
    } catch (error) {
      globalThis.console.error(
        `[clock-source] noise background OFF, tile build failed: ${String(error)}`,
      );
    }
  }

  function drawNoise() {
    if (noiseTiles.length === 0) return;
    const tile = noiseTiles[Math.floor(Math.random() * noiseTiles.length)];
    context.globalAlpha = NOISE_ALPHA;
    context.drawImage(
      tile,
      -Math.floor(Math.random() * NOISE_PAD),
      -Math.floor(Math.random() * NOISE_PAD),
    );
    context.globalAlpha = 1;
  }

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
    drawNoise();

    context.textAlign = "center";
    context.textBaseline = "middle";
    context.fillStyle = "#ffffff";
    context.font = FONT_TIME;
    const milliseconds = String(now.getMilliseconds()).padStart(3, "0");
    context.fillText(`${cachedTime}.${milliseconds}`, WIDTH / 2, Y_TIME, MAXW_TIME);

    context.fillStyle = "rgba(255, 255, 255, 0.82)";
    context.font = FONT_DATE;
    context.fillText(`${cachedDate} ${CLOCK_TIME_ZONE}`, WIDTH / 2, Y_DATE, MAXW_DATE);

    const participant =
      typeof globalThis.__CLOCK_PARTICIPANT === "string" ? globalThis.__CLOCK_PARTICIPANT : "";
    if (participant !== "") {
      context.fillStyle = "rgba(255, 255, 255, 0.92)";
      context.font = FONT_NAME;
      context.fillText(participant, WIDTH / 2, Y_NAME, MAXW_NAME);
    }

    context.fillStyle = `hsl(${(cachedHue + 55) % 360} 72% 55%)`;
    for (const y of MARKER_BANDS_Y) context.fillRect(0, y, WIDTH, MARKER_BAND_H);
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

  // Draws only while a handed-out video track, or any clone of one, is live.
  const liveVideoClones = new Set();
  function registerVideo(track) {
    liveVideoClones.add(track);
    const nativeClone = track.clone;
    track.clone = function (...args) {
      const copy = Reflect.apply(nativeClone, this, args);
      return liveVideoClones.has(this) ? registerVideo(copy) : copy;
    };
    return track;
  }
  function videoWanted() {
    let wanted = false;
    for (const track of liveVideoClones) {
      if (track.readyState === "live") wanted = true;
      else liveVideoClones.delete(track);
    }
    return wanted;
  }

  drawClock();
  globalThis.setInterval(() => {
    if (videoWanted()) drawClock();
  }, FRAME_INTERVAL_MS);

  globalThis.navigator.mediaDevices.getUserMedia = async (constraints = {}) => {
    const tracks = [];
    if (constraints.video) {
      tracks.push(registerVideo(baseVideoTrack.clone()));
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
