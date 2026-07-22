/**
 * Client-side meeting recording module.
 *
 * Composite pipeline
 * ──────────────────
 *   WebSocket / WebTransport tracks
 *     → NetEQ decoder → peer_gain → master_gain ─┐
 *   Local microphone (getUserMedia) ──────────────┼→ mixDest → audio track
 *   Screen-share audio ────────────────────────────┘
 *
 *   Decoded peer HtmlCanvasElements + local #webcam
 *     → drawFrame() rendering engine
 *     → _offCanvas → captureStream(0) → CanvasCaptureMediaStreamTrack
 *     → rafLoop calls requestFrame() on every animation tick → video track
 *
 *   new MediaStream([ video track, audio track ])
 *     → MediaRecorder → streamed plaintext file or encrypted RAM fallback
 *
 * Public API (called from Rust via wasm_bindgen externs):
 *   window.__vcRecording.start(peerSessionIds, onStateChange, localUserName, isLocalUserHost)
 *   window.__vcRecording.stop()
 */
(function () {
  "use strict";

  /** Recording width and height for the composite canvas. */
  var RECORD_WIDTH = 1280;
  var RECORD_HEIGHT = 720;

  /** Timeslice in milliseconds — MediaRecorder flushes a chunk this often. */
  var CHUNK_MS = 3000;

  /**
   * Hard ceiling (bytes) on the in-memory fallback path (`_chunks`), used when
   * `_writer` (streaming-to-disk via the File System Access API) is
   * unavailable — every non-Chromium-desktop browser: Firefox, Safari/iOS, and
   * Chrome for Android all lack `showSaveFilePicker`. Without this cap the
   * fallback accumulates every encrypted chunk for the WHOLE recording, then at
   * save time reassembles all of them into a new Blob (a 30-min @ 2.5 Mbps
   * recording is ~560 MB accumulated), which OOMs the affected mobile tabs.
   *
   * Budget note: a browser tab does NOT get the whole device RAM. On the
   * devices that actually hit this fallback path — iOS Safari (WKWebView, which
   * Jetsam kills well under total device RAM) and Chrome for Android on ~2 GB
   * hardware — the realistic per-tab budget before an OOM/Jetsam kill is closer
   * to ~500 MB, and that has to share with everything else resident during an
   * active call: the WebCodecs decoders for other peers' video/screen, the
   * composite recording canvas, the DOM, and the rest of the SPA's JS heap
   * (realistically another ~150-300 MB).
   *
   * Save-time peak: the default-on E2EE path is the expensive one. In `onstop`,
   * `Promise.all(capturedChunks)` yields the ciphertext (~N bytes) which stays
   * referenced while `.map(decrypt)` produces the plaintext (another ~N, so
   * ciphertext + plaintext coexist ≈ 2N), then `new Blob(plainParts)` copies
   * again while `plainParts` is still live — a transient ~2.5-3x peak on top of
   * the accumulated N, NOT the ~2x it is easy to assume. (The non-encrypted raw
   * fallback branch is cheaper: `new Blob(capturedChunks)` over already-existing
   * Blobs doesn't force everything into JS-visible memory the same way.)
   *
   * When the running total of bytes pushed into `_chunks` crosses this
   * ceiling, the recording auto-stops and saves whatever has been captured so
   * far (identical code path to a manual stop() click) instead of growing
   * unbounded. 100 MiB keeps the ~2.5-3x E2EE save-time peak (~250-300 MiB)
   * safely within that realistic ~500 MB tab budget alongside the rest of the
   * app. At the ~19 MB/min rate documented on `_writer` below, this bounds the
   * fallback to roughly 5 minutes of captured footage.
   */
  var IN_MEMORY_FALLBACK_MAX_BYTES_DEFAULT = 100 * 1024 * 1024;

  /** Target recording frame rate in frames per second. */
  var TARGET_FPS = 30;
  /** Minimum interval between rendered frames in milliseconds (1000 / TARGET_FPS). */
  var FRAME_INTERVAL_MS = 1000 / TARGET_FPS;
  /**
   * A/V sync window in milliseconds.  When a peer's mic-unmute event and
   * their first decoded video frame arrive within this window, both are
   * treated as jointly activated.  The peer's tile is held as an avatar
   * until BOTH audio and video are simultaneously confirmed, preventing an
   * audio-before-video or video-before-audio flash in the recording.
   */
  var AV_SYNC_WINDOW_MS = 500;

  /** Preferred MIME types in priority order (MP4 first, WebM fallback). */
  var MIME_TYPES = [
    'video/mp4;codecs="avc1.42E01E,mp4a.40.2"',
    "video/mp4",
    'video/webm;codecs="h264,opus"',
    'video/webm;codecs="vp8,opus"',
    "video/webm",
  ];

  function pickMimeType() {
    for (var i = 0; i < MIME_TYPES.length; i++) {
      if (
        typeof MediaRecorder !== "undefined" &&
        MediaRecorder.isTypeSupported(MIME_TYPES[i])
      ) {
        return MIME_TYPES[i];
      }
    }
    return "";
  }

  function fileExtension(mimeType) {
    return mimeType && mimeType.startsWith("video/mp4") ? "mp4" : "webm";
  }

  var _recorder = null;
  var _chunks = [];
  var _mimeType = "";
  var _animFrameId = null;
  var _peerIds = [];
  /** Frame counter incremented every drawFrame() — used to throttle debug logs. */
  var _dbgFrameCount = 0;
  var _offCanvas = null;
  var _offCtx = null;
  var _state = "idle"; // idle | activating | recording | stopping
  var _onStateChange = null;
  var _visibHandler = null;
  var _recorderPaused = false;
  /**
   * CanvasCaptureMediaStreamTrack returned by captureStream(0).  Held so
   * rafLoop() can call requestFrame() on each animation tick to push one
   * video frame regardless of whether the canvas pixels changed.
   */
  var _videoTrack = null;
  /** FileSystemFileHandle from showSaveFilePicker, or null when unavailable. */
  var _fileHandle = null;
  /**
   * FileSystemWritableFileStream opened once at recording start so that
   * ondataavailable can write each chunk directly to disk.  Keeping the
   * stream open for the whole recording avoids buffering chunks in RAM
   * (~19 MB/min at 2.5 Mbps video + 128 kbps audio → OOM on a 30-min
   * recording on a 2 GB device).  null = streaming unavailable, fall back
   * to the in-memory _chunks accumulation path.
   */
  var _writer = null;
  /**
   * Promise chain that serialises incremental _writer.write() calls so
   * ondataavailable never races itself (the handler is synchronous but the
   * writes are async).
   */
  var _writeChain = Promise.resolve();
  /**
   * Effective byte ceiling for the in-memory fallback (`_chunks`) path, read
   * at recording start from `IN_MEMORY_FALLBACK_MAX_BYTES_DEFAULT` unless the
   * test-only override `window.__VC_RECORDING_MAX_FALLBACK_BYTES__` is set
   * (mirrors the `window.__VC_WT_CERT_HASHES__` E2E-injection convention — see
   * e2e/playwright.config.ts). Only the fallback path consults this; the
   * `_writer` streaming-to-disk path is RAM-safe and never checks it.
   */
  var _fallbackMaxBytes = IN_MEMORY_FALLBACK_MAX_BYTES_DEFAULT;
  /** Running total of bytes pushed into `_chunks` on the fallback path. */
  var _fallbackBytes = 0;
  /**
   * One-shot guard so the fallback byte-ceiling auto-stop fires exactly once,
   * even if several ondataavailable events land before `_state` flips away
   * from "recording".
   */
  var _fallbackCapTripped = false;
  /** Display name of the local recording user, passed from Rust on start(). */
  var _localUserName = "";
  /** Whether the local recording user is the meeting host, passed from Rust on start(). */
  var _localIsHost = false;
  /** Cached Path2D for the person-silhouette SVG icon (lazy-initialised). */
  var _peerIconPath = null;
  /**
   * Cached meeting background image (loaded from the body's --bg-image CSS
   * variable on first recording start).  null = not yet attempted; false =
   * load failed/unavailable.  Used by drawBackground().
   */
  var _bgImage = null;
  var _bgImageAttempted = false;
  /**
   * Per-peer recording-diagnostic state (sid → {hadDecoder, hadDecContent, hadDom}).
   * Compared each frame so we can log exactly when the decoder registers its
   * canvas, when it starts producing frames, and when the DOM canvas mounts.
   */
  var _peerRecState = {};
  /**
   * Timestamp of the last rendered frame (performance.now()).
   * Used to throttle the rAF loop to TARGET_FPS.
   */
  var _lastFrameMs = 0;
  /**
   * Per-peer audio activation timestamps (sid → ms, performance.now()).
   * Set when a peer's mic transitions from muted → unmuted.
   */
  var _peerAudioActivatedAt = {};
  /**
   * Per-peer video activation timestamps (sid → ms, performance.now()).
   * Set the first time a peer's decoder canvas has non-transparent content.
   * Cleared when the peer's canvas loses content (camera off / peer left).
   */
  var _peerVideoActivatedAt = {};
  /**
   * Previous per-peer mic-muted state, used to detect unmute transitions.
   * sid → boolean (true = was muted last checked frame).
   */
  var _prevPeerMicMuted = {};
  /**
   * Scene fingerprint from the previous drawn frame.  A change here forces
   * a canvas redraw even when no live video is present.
   */
  var _prevSceneKey = null;
  /** Previous #webcam paused state — null until first observed. */
  var _prevWebcamPaused = null;
  /** Previous #webcam readyState — -1 until first observed. */
  var _prevWebcamReadyState = -1;
  /** AudioContext used to mix all audio sources before sending to MediaRecorder. */
  var _audioMixerCtx = null;
  /**
   * Reference to the SharedAudioContext's master_gain node (window.__vcMasterGain).
   * Connected directly to mixDest so all decoded remote-peer audio reaches the
   * recorder.  Disconnected when recording stops.
   */
  var _masterGainRef = null;
  /** MediaStreamSourceNode for the local microphone (null when not acquired). */
  var _micSource = null;
  /** MediaStreamAudioDestinationNode mixer destination (kept for dynamic mic connect). */
  var _mixDest = null;
  /** Tracks the previous mic-active state to avoid redundant connect/disconnect calls. */
  var _prevMicOn = null;
  /** MediaStreamSourceNode for local screen-share audio (null when not active). */
  var _ssAudioSource = null;
  /** The srcObject previously seen on #screen-share-preview; used to detect changes. */
  var _prevSsObject = null;
  /**
   * AES-256-GCM CryptoKey generated at recording start for E2EE file
   * encryption.  null when the WebCrypto API is unavailable or key generation
   * failed (recording proceeds unencrypted in that case).
   */
  var _e2eeKey = null;

  function setState(s) {
    _state = s;
    if (typeof _onStateChange === "function") {
      try {
        _onStateChange(s);
      } catch (e) {
        console.error("[recording] onStateChange threw:", e);
      }
    }
  }

  // ─────────────────────────────────────────────────────────────────
  // Tile-rendering helpers
  // ─────────────────────────────────────────────────────────────────

  // ── Design tokens (kept in sync with tokens-v0.json + style.css) ──
  /** Tile background — --color-surface-elevated in the dark theme. */
  var TILE_BG = "#2C2C2E";
  /** Grid gap between tiles — matches CSS `gap: 16px` on #grid-container. */
  var TILE_GAP = 16;
  /** Outer padding around the tile grid — matches CSS `padding: 20px`. */
  var GRID_PAD = 20;
  /** Height reserved at canvas bottom for the controls bar zone (matches CSS `padding-bottom: 84px`). */
  var CONTROLS_BAR_H = 84;
  /** Tile corner radius — --radius-lg = 12px. */
  var TILE_RADIUS = 12;
  /** Tile border colour — --color-border-emphasis. */
  var TILE_BORDER_COLOR = "#48484A";

  /**
   * Lazy-init a Path2D for the person-silhouette icon (viewBox 0 0 512 512).
   * The path data comes from dioxus-ui/src/components/icons/peer.rs.
   */
  function getPeerIconPath() {
    if (!_peerIconPath && typeof Path2D !== "undefined") {
      _peerIconPath = new Path2D(
        "M458.159,404.216c-18.93-33.65-49.934-71.764-100.409-93.431" +
          "c-28.868,20.196-63.938,32.087-101.745,32.087" +
          "c-37.828,0-72.898-11.89-101.767-32.087" +
          "c-50.474,21.667-81.479,59.782-100.398,93.431" +
          "C28.731,448.848,48.417,512,91.842,512" +
          "c43.426,0,164.164,0,164.164,0s120.726,0,164.153,0" +
          "C463.583,512,483.269,448.848,458.159,404.216z " +
          "M256.005,300.641c74.144,0,134.231-60.108,134.231-134.242v-32.158" +
          "C390.236,60.108,330.149,0,256.005,0" +
          "c-74.155,0-134.252,60.108-134.252,134.242V166.4" +
          "C121.753,240.533,181.851,300.641,256.005,300.641z",
      );
    }
    return _peerIconPath;
  }

  /**
   * Extract the display name from a [data-tile-root] element.
   *
   * The name text lives inside `<span class="floating-name-text">` — this
   * wrapper was added on 2026-07-07 (commit 38943640) so `text-overflow:
   * ellipsis` works within the inline-flex `.floating-name` parent. Read that
   * span's text first. Fall back to the first plain text node directly inside
   * `.floating-name` for backward compatibility with the pre-wrap markup (and
   * so any future markup change that drops the span still resolves a name).
   *
   * The sibling `<span class="host-indicator">(Host)</span>` (crown) and
   * `<span class="guest-badge">Guest</span>` live OUTSIDE `.floating-name-text`,
   * so reading only the text span yields the clean display name — the "(Host)"
   * suffix is appended separately in collectTileData() via the `.host-indicator`
   * probe.
   */
  function getTileName(tileEl) {
    var nameEl = tileEl.querySelector(".floating-name");
    if (!nameEl) return "";
    var textEl = nameEl.querySelector(".floating-name-text");
    if (textEl && textEl.textContent.trim()) {
      return textEl.textContent.trim();
    }
    for (var n = nameEl.firstChild; n; n = n.nextSibling) {
      if (n.nodeType === 3 /* TEXT_NODE */ && n.textContent.trim()) {
        return n.textContent.trim();
      }
    }
    return "";
  }

  /**
   * Return the speaking highlight colour for a tile element, or null if
   * the peer is not currently speaking.  Reads the `.speaking-tile` class
   * and the inline `border-color` set by Rust's speak_style().
   */
  function getTileSpeakColor(tileEl) {
    if (!tileEl || !tileEl.classList.contains("speaking-tile")) return null;
    var color = tileEl.style && tileEl.style.borderColor;
    return color || "#2ecc71";
  }

  /**
   * Build a tile-data record from a [data-tile-root] DOM element.
   * Now also reads the signal level from data-signal-level / data-signal-lost
   * on the .signal-indicator button so we can draw it in the corner.
   */
  function collectTileData(
    tileEl,
    videoOverride,
    nameOverride,
    speakColorOverride,
    micMutedOverride,
    decoderCanvasMap,
  ) {
    // Video source
    var videoEl = videoOverride !== undefined ? videoOverride : null;
    if (videoEl === null && tileEl) {
      // ── Priority 1: live decoder canvas (bypasses Dioxus DOM mounting) ──
      // The decoder canvas is available even when `show_canvas = false` (budget
      // pressure, force_avatar, or the 50 ms reactive throttle that delays the
      // parent re-render after a camera-on event).  The tile div id is
      // "peer-video-{sessionId}-div"; extract the session ID to look up the
      // canvas that Rust registered via window.__vcGetPeerVideoCanvases().
      if (decoderCanvasMap && tileEl.id) {
        var m = tileEl.id.match(/^peer-video-(\d+)-div$/);
        if (m) {
          var dcCanvas = decoderCanvasMap[m[1]];
          if (dcCanvas && dcCanvas.width > 0 && dcCanvas.height > 0) {
            videoEl = dcCanvas;
          }
        }
      }

      // ── Priority 2: DOM canvas (fallback) ────────────────────────────────
      // Used when the decoder canvas map is unavailable or has no entry for
      // this peer (e.g. camera just turned on and decoder not yet attached).
      if (videoEl === null) {
        var c = tileEl.querySelector("canvas");
        if (c && c.width > 0 && c.height > 0) {
          videoEl = c;
        } else if (
          (_dbgFrameCount === 1 || _dbgFrameCount % 150 === 0) &&
          tileEl.id
        ) {
          console.warn(
            "[recording] tile",
            tileEl.id,
            "no videoEl —",
            c
              ? "canvas w=" +
                  c.width +
                  " h=" +
                  c.height +
                  " hidden=" +
                  c.hidden +
                  " hiddenAttr=" +
                  c.hasAttribute("hidden")
              : "no canvas found",
          );
        }
      }
    }
    if (videoEl && videoEl.tagName === "VIDEO") {
      // Basic readiness / dimension check.
      var videoReady = videoEl.readyState >= 2 && videoEl.videoWidth > 0;
      if (videoReady) {
        // Also verify that the underlying stream still has at least one live
        // (non-ended) video track.  When the local user turns their camera
        // off, the browser may stop the MediaStreamTrack but leave srcObject
        // set and keep videoWidth / readyState at their previous values, causing
        // the stale (often black) last frame to be drawn indefinitely.
        // This check catches that case and falls through to the avatar render.
        if (
          videoEl.srcObject &&
          typeof videoEl.srcObject.getVideoTracks === "function"
        ) {
          var liveTracks = videoEl.srcObject
            .getVideoTracks()
            .filter(function (t) {
              return t.readyState !== "ended";
            });
          if (!liveTracks.length) videoReady = false;
        } else if (!videoEl.srcObject) {
          // No source attached at all — treat as camera-off.
          videoReady = false;
        }
      }
      if (!videoReady) videoEl = null;
    }

    var name =
      nameOverride !== undefined
        ? nameOverride
        : tileEl
          ? getTileName(tileEl)
          : "";
    var speakColor =
      speakColorOverride !== undefined
        ? speakColorOverride
        : tileEl
          ? getTileSpeakColor(tileEl)
          : null;

    // Signal quality (from data attributes on .signal-indicator button)
    var signalLevel = -1; // -1 = no data
    var signalLost = false;
    if (tileEl) {
      var sigBtn = tileEl.querySelector(".signal-indicator");
      if (sigBtn) {
        var lvl = sigBtn.getAttribute("data-signal-level");
        if (lvl !== null) {
          signalLevel = parseInt(lvl, 10);
          signalLost = sigBtn.getAttribute("data-signal-lost") === "true";
        }
      }
    }

    // Transport badge: "wt", "ws", or null — from .transport-badge--wt / --ws spans.
    var transport = null;
    if (tileEl) {
      if (tileEl.querySelector(".transport-badge--wt")) {
        transport = "wt";
      } else if (tileEl.querySelector(".transport-badge--ws")) {
        transport = "ws";
      }
    }

    // Microphone muted state: prefer the `data-mic-muted` attribute set by
    // Rust on the .audio-indicator element (the most reliable source), with a
    // fallback to counting <line> elements in the MicIcon SVG (muted icon has
    // 2 lines — stand + diagonal slash; unmuted has 1 — stand only).
    // micMutedOverride is used for the local user tile (no DOM tile element).
    var micMuted = micMutedOverride !== undefined ? micMutedOverride : false;
    if (micMutedOverride === undefined && tileEl) {
      var audioInd = tileEl.querySelector(".audio-indicator");
      if (audioInd && audioInd.hasAttribute("data-mic-muted")) {
        micMuted = audioInd.getAttribute("data-mic-muted") === "true";
      } else {
        micMuted =
          tileEl.querySelectorAll(".audio-indicator svg line").length >= 2;
      }
    }

    // Host badge: CrownIcon renders as <span class="host-indicator">(Host)</span>
    // inside .floating-name. Append that text to the name chip when present.
    if (tileEl && tileEl.querySelector(".host-indicator")) {
      name = name + " (Host)";
    }

    return {
      videoEl: videoEl,
      name: name,
      speakColor: speakColor,
      signalLevel: signalLevel,
      signalLost: signalLost,
      transport: transport,
      micMuted: micMuted,
    };
  }

  /**
   * Draw a rounded-rectangle path (current path).  Falls back to
   * quadraticCurveTo when the native roundRect() API is absent.
   */
  function roundRect(x, y, w, h, r) {
    var ctx = _offCtx;
    if (ctx.roundRect) {
      ctx.beginPath();
      ctx.roundRect(x, y, w, h, r);
    } else {
      ctx.beginPath();
      ctx.moveTo(x + r, y);
      ctx.lineTo(x + w - r, y);
      ctx.quadraticCurveTo(x + w, y, x + w, y + r);
      ctx.lineTo(x + w, y + h - r);
      ctx.quadraticCurveTo(x + w, y + h, x + w - r, y + h);
      ctx.lineTo(x + r, y + h);
      ctx.quadraticCurveTo(x, y + h, x, y + h - r);
      ctx.lineTo(x, y + r);
      ctx.quadraticCurveTo(x, y, x + r, y);
      ctx.closePath();
    }
  }

  /** Draw el letterboxed (aspect-correct, centred, black surround) into (dx, dy, dw, dh). */
  function drawLetterboxed(el, dx, dy, dw, dh) {
    var srcW = el.videoWidth || el.width;
    var srcH = el.videoHeight || el.height;
    if (!srcW || !srcH || !dw || !dh) return;
    var aspect = srcW / srcH;
    var fitW = Math.min(dw, dh * aspect);
    var fitH = fitW / aspect;
    try {
      _offCtx.drawImage(
        el,
        dx + (dw - fitW) / 2,
        dy + (dh - fitH) / 2,
        fitW,
        fitH,
      );
    } catch (e) {
      console.error(
        "[recording] drawImage failed for",
        el.tagName,
        el.id || "(no id)",
        "srcW=" + srcW,
        "srcH=" + srcH,
        "dw=" + dw,
        "dh=" + dh,
        e,
      );
    }
  }

  /** Draw the person-silhouette avatar icon centred inside (tx, ty, tw, th). */
  function drawAvatar(tx, ty, tw, th) {
    var path = getPeerIconPath();
    if (!path) return;
    var iconSize = Math.min(tw, th) * 0.38;
    var scale = iconSize / 512;
    // Centre the 512×512 viewbox icon inside the tile (slightly above mid,
    // matching the meeting UI's visual weight).
    var ox = tx + (tw - iconSize) / 2;
    var oy = ty + th / 2 - iconSize * 0.6;
    _offCtx.save();
    _offCtx.translate(ox, oy);
    _offCtx.scale(scale, scale);
    _offCtx.fillStyle = "rgba(160, 160, 160, 0.45)";
    _offCtx.fill(path);
    _offCtx.restore();
  }

  /**
   * Draw the floating-name chip (top-left of tile) exactly as `.floating-name`
   * in the real meeting: translucent dark pill, white semi-bold text, clamped
   * to 55 % tile width, ellipsis on overflow.
   */
  function drawNameChip(name, tx, ty, tw, th) {
    if (!name) return;
    var fontSize = Math.max(10, Math.min(13, Math.round(th * 0.065)));
    _offCtx.save();
    _offCtx.font =
      "600 " + fontSize + "px -apple-system, BlinkMacSystemFont, sans-serif";
    var PAD_H = Math.round(Math.min(10, tw * 0.03));
    var PAD_V = Math.round(Math.min(5, th * 0.025));
    var chipH = fontSize + PAD_V * 2;
    var r = chipH / 2;
    var maxTW = tw * 0.55 - PAD_H * 2;
    // Measure and clamp text
    var textW = Math.min(_offCtx.measureText(name).width, maxTW);
    var chipW = textW + PAD_H * 2;
    var ox = tx + Math.round(Math.min(12, tw * 0.04));
    var oy = ty + Math.round(Math.min(10, th * 0.05));
    // Background pill
    _offCtx.fillStyle = "rgba(0, 0, 0, 0.44)";
    roundRect(ox, oy, chipW, chipH, r);
    _offCtx.fill();
    // Text (clipped to chip bounds)
    _offCtx.fillStyle = "#ffffff";
    _offCtx.textAlign = "left";
    _offCtx.textBaseline = "middle";
    _offCtx.beginPath();
    _offCtx.rect(ox + PAD_H, oy, chipW - PAD_H * 2, chipH);
    _offCtx.clip();
    _offCtx.fillText(name, ox + PAD_H, oy + chipH / 2, chipW - PAD_H * 2);
    _offCtx.restore();
  }

  /**
   * Draw a bottom gradient scrim over the tile video area, exactly like
   * `.grid-item .canvas-container.video-on::after` in global.css.
   */
  function drawVideoScrim(tx, ty, tw, th) {
    var scrimH = Math.min(64, th * 0.27);
    var grad = _offCtx.createLinearGradient(0, ty + th - scrimH, 0, ty + th);
    grad.addColorStop(0, "rgba(0,0,0,0)");
    grad.addColorStop(1, "rgba(0,0,0,0.55)");
    _offCtx.fillStyle = grad;
    _offCtx.fillRect(tx, ty + th - scrimH, tw, scrimH);
  }

  /**
   * Draw a coloured speaking border with glow on a tile, matching the
   * `speak_style()` CSS box-shadow applied by Rust to `.grid-item`.
   *
   * Uses an even-odd clip to restrict the shadow/stroke to the ring OUTSIDE
   * the tile boundary, exactly like `box-shadow` — the glow never bleeds over
   * the tile's video content.
   */
  function drawSpeakingBorder(tx, ty, tw, th, color) {
    var lw = Math.max(2, Math.round(Math.min(tw, th) * 0.02));
    var glowR = Math.round(lw * 4);
    var r = Math.min(TILE_RADIUS, tw / 2, th / 2);
    _offCtx.save();

    // Build a two-subpath path: (1) outer rect covering the full glow area,
    // (2) the rounded-rect tile interior as a hole.  clip("evenodd") keeps
    // only the ring between the two shapes, so the shadow stays outside.
    _offCtx.beginPath();
    _offCtx.rect(tx - glowR, ty - glowR, tw + glowR * 2, th + glowR * 2);
    if (_offCtx.roundRect) {
      _offCtx.roundRect(tx, ty, tw, th, r);
    } else {
      _offCtx.moveTo(tx + r, ty);
      _offCtx.lineTo(tx + tw - r, ty);
      _offCtx.quadraticCurveTo(tx + tw, ty, tx + tw, ty + r);
      _offCtx.lineTo(tx + tw, ty + th - r);
      _offCtx.quadraticCurveTo(tx + tw, ty + th, tx + tw - r, ty + th);
      _offCtx.lineTo(tx + r, ty + th);
      _offCtx.quadraticCurveTo(tx, ty + th, tx, ty + th - r);
      _offCtx.lineTo(tx, ty + r);
      _offCtx.quadraticCurveTo(tx, ty, tx + r, ty);
      _offCtx.closePath();
    }
    _offCtx.clip("evenodd");

    // Stroke the border with a glow — visible only in the outer ring.
    _offCtx.beginPath();
    _offCtx.strokeStyle = color;
    _offCtx.lineWidth = lw;
    _offCtx.shadowColor = color;
    _offCtx.shadowBlur = glowR;
    _offCtx.shadowOffsetX = 0;
    _offCtx.shadowOffsetY = 0;
    var mx = tx + lw / 2,
      my = ty + lw / 2,
      mw = tw - lw,
      mh = th - lw;
    if (_offCtx.roundRect) {
      _offCtx.roundRect(mx, my, mw, mh, r);
    } else {
      _offCtx.moveTo(mx + r, my);
      _offCtx.lineTo(mx + mw - r, my);
      _offCtx.quadraticCurveTo(mx + mw, my, mx + mw, my + r);
      _offCtx.lineTo(mx + mw, my + mh - r);
      _offCtx.quadraticCurveTo(mx + mw, my + mh, mx + mw - r, my + mh);
      _offCtx.lineTo(mx + r, my + mh);
      _offCtx.quadraticCurveTo(mx, my + mh, mx, my + mh - r);
      _offCtx.lineTo(mx, my + r);
      _offCtx.quadraticCurveTo(mx, my, mx + r, my);
      _offCtx.closePath();
    }
    _offCtx.stroke();
    _offCtx.restore();
  }

  /**
   * Draw one SVG icon (24×24 viewBox, stroke-based) centred at (cx, cy),
   * scaled to `size` pixels.  The callback `fn` performs the actual path/line
   * drawing inside the normalised 24×24 coordinate space.
   */
  function drawStrokeIcon(cx, cy, size, fn, color) {
    var scale = size / 24;
    _offCtx.save();
    _offCtx.translate(cx - size / 2, cy - size / 2);
    _offCtx.scale(scale, scale);
    _offCtx.strokeStyle = color || "#ffffff";
    _offCtx.lineWidth = 2; // matches SVG stroke-width="2" in 24×24 space
    _offCtx.lineCap = "round";
    _offCtx.lineJoin = "round";
    fn();
    _offCtx.restore();
  }

  /**
   * Draw a control bar icon by type at canvas position (cx, cy).
   * `on`    — whether the control is active (mic on, camera on, SS on).
   * `color` — stroke/fill colour for the icon.
   */
  function drawControlIcon(type, cx, cy, size, color, on) {
    if (type === "mic") {
      drawStrokeIcon(
        cx,
        cy,
        size,
        function () {
          _offCtx.stroke(
            new Path2D("M12 2a3 3 0 0 0-3 3v7a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3z"),
          );
          _offCtx.stroke(new Path2D("M19 10v2a7 7 0 0 1-14 0v-2"));
          _offCtx.beginPath();
          _offCtx.moveTo(12, 19);
          _offCtx.lineTo(12, 22);
          _offCtx.stroke();
          if (!on) {
            _offCtx.beginPath();
            _offCtx.moveTo(3, 3);
            _offCtx.lineTo(21, 21);
            _offCtx.stroke();
          }
        },
        color,
      );
    } else if (type === "cam") {
      drawStrokeIcon(
        cx,
        cy,
        size,
        function () {
          _offCtx.stroke(new Path2D("M23 7L16 12L23 17Z"));
          _offCtx.stroke(
            new Path2D(
              "M1 5a2 2 0 0 1 2-2h12a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H3a2 2 0 0 1-2-2z",
            ),
          );
          if (!on) {
            _offCtx.beginPath();
            _offCtx.moveTo(1, 1);
            _offCtx.lineTo(23, 23);
            _offCtx.stroke();
          }
        },
        color,
      );
    } else if (type === "ss") {
      drawStrokeIcon(
        cx,
        cy,
        size,
        function () {
          _offCtx.stroke(
            new Path2D(
              "M4 3a2 2 0 0 0-2 2v10a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2V5a2 2 0 0 0-2-2z",
            ),
          );
          _offCtx.beginPath();
          _offCtx.moveTo(8, 21);
          _offCtx.lineTo(16, 21);
          _offCtx.stroke();
          _offCtx.beginPath();
          _offCtx.moveTo(12, 17);
          _offCtx.lineTo(12, 21);
          _offCtx.stroke();
        },
        color,
      );
    } else if (type === "settings") {
      // Gear / settings icon (Feather-style cog, 24×24)
      drawStrokeIcon(
        cx,
        cy,
        size,
        function () {
          _offCtx.stroke(new Path2D("M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6z"));
          _offCtx.stroke(
            new Path2D(
              "M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1 0 2.83 2 2 0 0 1-2.83 0l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-2 2 2 2 0 0 1-2-2v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83 0 2 2 0 0 1 0-2.83l.06-.06A1.65 1.65 0 0 0 4.68 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1-2-2 2 2 0 0 1 2-2h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 0-2.83 2 2 0 0 1 2.83 0l.06.06A1.65 1.65 0 0 0 9 4.68a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 2-2 2 2 0 0 1 2 2v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 0 2 2 0 0 1 0 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 2 2 2 2 0 0 1-2 2h-.09a1.65 1.65 0 0 0-1.51 1z",
            ),
          );
        },
        color,
      );
    } else if (type === "leave") {
      // Filled phone/hang-up icon (viewBox 0 0 24 24, fill not stroke)
      var scale = size / 24;
      _offCtx.save();
      _offCtx.translate(cx - size / 2, cy - size / 2);
      _offCtx.scale(scale, scale);
      _offCtx.fillStyle = color || "#ffffff";
      _offCtx.fill(
        new Path2D(
          "M12.017 6.995c-2.306 0-4.534.408-6.215 1.507-1.737 1.135-2.788 2.944-2.797 " +
            "5.451a4.8 4.8 0 0 0 .01.62c.015.193.047.512.138.763a2.557 2.557 0 0 0 2.579 " +
            "1.677H7.31a2.685 2.685 0 0 0 2.685-2.684v-.645a.684.684 0 0 1 .684-.684h2.647a" +
            ".686.686 0 0 1 .686.687v.645c0 .712.284 1.395.787 1.898.478.478 1.101.787 1.847" +
            ".787h1.647a2.555 2.555 0 0 0 2.575-1.674c.09-.25.123-.57.137-.763.015-.2.022-.4" +
            "33.01-.617-.002-2.508-1.049-4.32-2.785-5.458-1.68-1.1-3.907-1.51-6.213-1.51Z",
        ),
      );
      _offCtx.restore();
    }
  }

  /**
   * Render the visible SVG child shapes from `svgEl` centred at (cx, cy)
   * within a `size × size` bounding box on the offscreen canvas.
   *
   * Handles element types used by the action-bar buttons:
   *   path, circle, line, rect, polygon, polyline (and g groups).
   *
   * fill/stroke="currentColor" resolve to `color`; "none" suppresses the op.
   * Per-element attributes take precedence over SVG root attributes.
   */
  function drawDomSvg(svgEl, cx, cy, size, color) {
    if (!svgEl) return;
    var vbParts = (
      svgEl.getAttribute("viewBox") ||
      svgEl.getAttribute("viewbox") ||
      "0 0 24 24"
    )
      .trim()
      .split(/[\s,]+/);
    var vbW = parseFloat(vbParts[2]) || 24;
    var vbH = parseFloat(vbParts[3]) || 24;
    var scale = size / Math.max(vbW, vbH);

    var rootFill = svgEl.getAttribute("fill") || "none";
    var rootStroke = svgEl.getAttribute("stroke") || "none";
    var rootSw = parseFloat(
      svgEl.getAttribute("stroke-width") ||
        svgEl.getAttribute("strokeWidth") ||
        "2",
    );
    var rootLineCap =
      svgEl.getAttribute("stroke-linecap") ||
      svgEl.getAttribute("strokeLinecap") ||
      "round";
    var rootLineJoin =
      svgEl.getAttribute("stroke-linejoin") ||
      svgEl.getAttribute("strokeLinejoin") ||
      "round";

    _offCtx.save();
    _offCtx.translate(cx - size / 2, cy - size / 2);
    _offCtx.scale(scale, scale);

    function resolveAttr(elAttr, rootAttr) {
      var v = elAttr !== null ? elAttr : rootAttr || "";
      return v === "currentColor" ? color : v === "none" ? null : v || null;
    }

    function drawEl(el) {
      if (!el || !el.tagName) return;
      var tag = el.tagName.toLowerCase().replace(/^.*:/, "");

      if (tag === "g") {
        var gch = el.children || el.childNodes;
        for (var gi = 0; gi < gch.length; gi++) drawEl(gch[gi]);
        return;
      }

      var eFill = el.getAttribute("fill");
      var eStroke = el.getAttribute("stroke");
      var fillC = resolveAttr(eFill, rootFill);
      var strokeC = resolveAttr(eStroke, rootStroke);
      var sw = parseFloat(
        el.getAttribute("stroke-width") ||
          el.getAttribute("strokeWidth") ||
          rootSw,
      );
      var lCap =
        el.getAttribute("stroke-linecap") ||
        el.getAttribute("strokeLinecap") ||
        rootLineCap;
      var lJoin =
        el.getAttribute("stroke-linejoin") ||
        el.getAttribute("strokeLinejoin") ||
        rootLineJoin;

      _offCtx.save();
      _offCtx.lineWidth = sw;
      _offCtx.lineCap = lCap;
      _offCtx.lineJoin = lJoin;

      if (tag === "path") {
        var d = el.getAttribute("d");
        if (d) {
          try {
            var p2d = new Path2D(d);
            if (fillC) {
              _offCtx.fillStyle = fillC;
              _offCtx.fill(p2d);
            }
            if (strokeC) {
              _offCtx.strokeStyle = strokeC;
              _offCtx.stroke(p2d);
            }
          } catch (_e) {}
        }
        _offCtx.restore();
        return;
      }

      _offCtx.beginPath();
      if (tag === "circle") {
        _offCtx.arc(
          parseFloat(el.getAttribute("cx") || "0"),
          parseFloat(el.getAttribute("cy") || "0"),
          parseFloat(el.getAttribute("r") || "0"),
          0,
          Math.PI * 2,
        );
      } else if (tag === "line") {
        _offCtx.moveTo(
          parseFloat(el.getAttribute("x1") || "0"),
          parseFloat(el.getAttribute("y1") || "0"),
        );
        _offCtx.lineTo(
          parseFloat(el.getAttribute("x2") || "0"),
          parseFloat(el.getAttribute("y2") || "0"),
        );
      } else if (tag === "rect") {
        var rrx = parseFloat(el.getAttribute("x") || "0");
        var rry = parseFloat(el.getAttribute("y") || "0");
        var rrw = parseFloat(el.getAttribute("width") || "0");
        var rrh = parseFloat(el.getAttribute("height") || "0");
        var rx2 = parseFloat(
          el.getAttribute("rx") || el.getAttribute("ry") || "0",
        );
        if (rx2 > 0) {
          var rr = Math.min(rx2, rrw / 2, rrh / 2);
          if (_offCtx.roundRect) {
            _offCtx.roundRect(rrx, rry, rrw, rrh, rr);
          } else {
            _offCtx.moveTo(rrx + rr, rry);
            _offCtx.lineTo(rrx + rrw - rr, rry);
            _offCtx.quadraticCurveTo(rrx + rrw, rry, rrx + rrw, rry + rr);
            _offCtx.lineTo(rrx + rrw, rry + rrh - rr);
            _offCtx.quadraticCurveTo(
              rrx + rrw,
              rry + rrh,
              rrx + rrw - rr,
              rry + rrh,
            );
            _offCtx.lineTo(rrx + rr, rry + rrh);
            _offCtx.quadraticCurveTo(rrx, rry + rrh, rrx, rry + rrh - rr);
            _offCtx.lineTo(rrx, rry + rr);
            _offCtx.quadraticCurveTo(rrx, rry, rrx + rr, rry);
            _offCtx.closePath();
          }
        } else {
          _offCtx.rect(rrx, rry, rrw, rrh);
        }
      } else if (tag === "polygon" || tag === "polyline") {
        var pts = (el.getAttribute("points") || "").trim().split(/[\s,]+/);
        if (pts.length >= 2) {
          _offCtx.moveTo(parseFloat(pts[0]), parseFloat(pts[1]));
          for (var pi = 2; pi + 1 < pts.length; pi += 2) {
            _offCtx.lineTo(parseFloat(pts[pi]), parseFloat(pts[pi + 1]));
          }
          if (tag === "polygon") _offCtx.closePath();
        }
      } else {
        _offCtx.restore();
        return;
      }

      if (fillC) {
        _offCtx.fillStyle = fillC;
        _offCtx.fill();
      }
      if (strokeC) {
        _offCtx.strokeStyle = strokeC;
        _offCtx.stroke();
      }
      _offCtx.restore();
    }

    var children = svgEl.children || svgEl.childNodes;
    for (var ci = 0; ci < children.length; ci++) drawEl(children[ci]);
    _offCtx.restore();
  }

  /**
   * Draw the bottom controls bar by reading ALL `.video-control-button`
   * elements from the live DOM.  This ensures the recording always shows the
   * full set of action-bar icons — including secondary controls that are
   * normally collapsed until hover — and that each button's icon and state
   * (active/off/danger) exactly matches the real meeting UI.
   */
  function drawControlsBar(w, h) {
    var BTN = 50;
    var GAP = 14;
    var PH = 16;
    var PV = 7;
    var BR = 40;
    var ICON = 22;

    var container = document.querySelector(".video-controls-container");
    if (!container) return;
    var btnEls = container.querySelectorAll(".video-control-button");
    if (!btnEls.length) return;

    var n = btnEls.length;
    var barW = n * BTN + (n - 1) * GAP + PH * 2;
    var barH = BTN + PV * 2;
    var bx = Math.max(0, (w - barW) / 2);
    var by = h - barH - 20;

    // Pill background
    _offCtx.save();
    _offCtx.fillStyle = "rgba(28, 28, 30, 0.95)";
    _offCtx.shadowColor = "rgba(0,0,0,0.55)";
    _offCtx.shadowBlur = 20;
    roundRect(bx, by, barW, barH, BR);
    _offCtx.fill();
    _offCtx.shadowBlur = 0;
    _offCtx.strokeStyle = "#38383A";
    _offCtx.lineWidth = 1;
    roundRect(bx, by, barW, barH, BR);
    _offCtx.stroke();
    _offCtx.restore();

    for (var i = 0; i < n; i++) {
      var el = btnEls[i];
      var isActive =
        el.classList.contains("active") ||
        el.classList.contains("record-active");
      var isOff = el.classList.contains("off");
      var isDanger = el.classList.contains("danger");

      var btnBg = isDanger
        ? "#ff453a"
        : isActive
          ? "#0a84ff"
          : isOff
            ? "rgba(255, 69, 58, 0.18)"
            : "#1c1c1e";
      var btnBorder = isDanger
        ? "#ff453a"
        : isActive
          ? "#0a84ff"
          : isOff
            ? "rgba(255, 69, 58, 0.35)"
            : "#48484a";
      var iconColor = isOff ? "#FF6961" : "#ffffff";

      var bcx = bx + PH + BTN / 2 + i * (BTN + GAP);
      var bcy = by + PV + BTN / 2;

      _offCtx.save();
      _offCtx.beginPath();
      _offCtx.arc(bcx, bcy, BTN / 2, 0, Math.PI * 2);
      _offCtx.fillStyle = btnBg;
      _offCtx.fill();
      _offCtx.strokeStyle = btnBorder;
      _offCtx.lineWidth = 1;
      _offCtx.stroke();
      _offCtx.restore();

      var svgEl = el.querySelector("svg");
      if (svgEl) drawDomSvg(svgEl, bcx, bcy, ICON, iconColor);
    }
  }

  /**
   * Draw a "● REC" indicator in the top-right corner of the recording canvas.
   */
  function drawRecIndicator(w) {
    var DOT_R = 5;
    var MARGIN = 16;
    var oy = MARGIN + DOT_R;
    _offCtx.save();
    // Red dot
    _offCtx.beginPath();
    _offCtx.arc(w - MARGIN, oy, DOT_R, 0, Math.PI * 2);
    _offCtx.fillStyle = "#ff3b30";
    _offCtx.fill();
    // "REC" label
    _offCtx.fillStyle = "rgba(255,255,255,0.80)";
    _offCtx.font = "600 12px -apple-system, BlinkMacSystemFont, sans-serif";
    _offCtx.textAlign = "right";
    _offCtx.textBaseline = "middle";
    _offCtx.fillText("REC", w - MARGIN - DOT_R - 6, oy);
    _offCtx.restore();
  }

  /**
   * Render one participant tile at (tx, ty, tw × th) using the same visual
   * language as the real meeting tiles:
   *   ① Rounded-corner background (#2C2C2E, --color-surface-elevated)
   *   ② 2 px border (#48484A, --color-border-emphasis)
   *   ③ Letterboxed video OR person-silhouette avatar (clipped to tile)
   *   ④ Bottom gradient scrim over video (mirrors .canvas-container::after)
   *   ⑤ Floating name chip at top-left (mirrors .floating-name)
   *   ⑥ Signal bars icon at top-right (mirrors .tile-top-icons .signal-indicator)
   *   ⑦ Speaking glow border (mirrors speak_style() inline border + glow)
   */
  function drawTile(tileData, tx, ty, tw, th) {
    var r = Math.min(TILE_RADIUS, tw / 2, th / 2);
    // ① Clip to tile shape
    _offCtx.save();
    roundRect(tx, ty, tw, th, r);
    _offCtx.clip();
    // ② Background
    _offCtx.fillStyle = TILE_BG;
    _offCtx.fillRect(tx, ty, tw, th);
    // ③ Video or avatar
    if (tileData.videoEl) {
      drawLetterboxed(tileData.videoEl, tx, ty, tw, th);
      drawVideoScrim(tx, ty, tw, th);
    } else {
      drawAvatar(tx, ty, tw, th);
    }
    _offCtx.restore(); // remove clip
    // ④ Border
    _offCtx.save();
    _offCtx.strokeStyle = TILE_BORDER_COLOR;
    _offCtx.lineWidth = 2;
    roundRect(tx + 1, ty + 1, tw - 2, th - 2, r);
    _offCtx.stroke();
    _offCtx.restore();
    // ⑤ Floating name chip (top-left)
    drawNameChip(tileData.name, tx, ty, tw, th);
    // ⑥ Tile top-right icon cluster — matches .tile-top-icons (flex-direction:
    //    row-reverse) in the real meeting.  Drawn right-to-left:
    //      mic indicator | transport badge (when present) | signal bars
    {
      var ICON_SZ = Math.max(14, Math.min(20, Math.round(tw * 0.1)));
      var iconPad = Math.round(Math.min(8, tw * 0.03));
      var ICON_GAP = Math.max(3, Math.round(Math.min(6, tw * 0.025)));
      var icnOY = ty + iconPad;
      var rx = tx + tw - iconPad; // right edge to lay out from

      // ── Mic indicator (rightmost, matches .audio-indicator circle) ──────
      var micR = ICON_SZ / 2;
      var micCX = rx - micR;
      var micCY = icnOY + micR;
      _offCtx.save();
      _offCtx.beginPath();
      _offCtx.arc(micCX, micCY, micR, 0, Math.PI * 2);
      _offCtx.fillStyle = "rgba(0, 0, 0, 0.55)";
      _offCtx.fill();
      _offCtx.restore();
      var micIconColor = tileData.micMuted ? "#FF6961" : "#ffffff";
      drawControlIcon(
        "mic",
        micCX,
        micCY,
        Math.round(ICON_SZ * 0.65),
        micIconColor,
        !tileData.micMuted,
      );
      rx -= ICON_SZ + ICON_GAP;

      // ── Transport badge (WT = blue, WS = amber) — when available ─────────
      if (tileData.transport) {
        var badgeText = tileData.transport === "wt" ? "WT" : "WS";
        var badgeFontSz = Math.max(8, Math.min(11, Math.round(ICON_SZ * 0.65)));
        var badgePadH = Math.max(3, Math.round(badgeFontSz * 0.4));
        var badgePadV = Math.max(2, Math.round(badgeFontSz * 0.25));
        _offCtx.save();
        _offCtx.font =
          "600 " +
          badgeFontSz +
          "px -apple-system, BlinkMacSystemFont, sans-serif";
        var badgeTxtW = _offCtx.measureText(badgeText).width;
        var badgeW = badgeTxtW + badgePadH * 2;
        var badgeH = badgeFontSz + badgePadV * 2;
        var bdX = rx - badgeW;
        var bdY = icnOY + (ICON_SZ - badgeH) / 2;
        var bdBg = tileData.transport === "wt" ? "#0062cc" : "#ff9f0a";
        var bdTxt = tileData.transport === "wt" ? "#ffffff" : "#000000";
        _offCtx.fillStyle = bdBg;
        roundRect(bdX, bdY, badgeW, badgeH, 4);
        _offCtx.fill();
        _offCtx.fillStyle = bdTxt;
        _offCtx.textAlign = "center";
        _offCtx.textBaseline = "middle";
        _offCtx.fillText(badgeText, bdX + badgeW / 2, bdY + badgeH / 2);
        _offCtx.restore();
        rx -= badgeW + ICON_GAP;
      }

      // ── Signal bars ───────────────────────────────────────────────────────
      if (tileData.signalLevel >= 0) {
        drawSignalBars(
          tileData.signalLevel,
          tileData.signalLost,
          rx - ICON_SZ,
          icnOY,
          ICON_SZ,
        );
      }
    }
    // ⑦ Speaking glow border (outermost)
    if (tileData.speakColor) {
      drawSpeakingBorder(tx, ty, tw, th, tileData.speakColor);
    }
  }

  /**
   * Lay out an array of tile-data records as a single-column strip inside
   * (rx, ry, rw, rh).  Used for the right-side participant panel during screen
   * share, matching the real meeting layout where tiles stack vertically.
   */
  function drawSingleColumn(tiles, rx, ry, rw, rh) {
    if (!tiles.length || rw <= 0 || rh <= 0) return;
    var g = TILE_GAP;
    var rows = tiles.length;
    var tileW = rw - g * 2;
    var tileH = (rh - g * (rows + 1)) / rows;
    if (tileW <= 0 || tileH <= 0) return;
    for (var i = 0; i < rows; i++) {
      drawTile(tiles[i], rx + g, ry + g + i * (tileH + g), tileW, tileH);
    }
  }

  /**
   * Lay out an array of tile-data records as an equal grid inside (rx, ry, rw, rh).
   * Columns = ceil(√N), rows = ceil(N / cols).
   */
  function drawGrid(tiles, rx, ry, rw, rh) {
    if (!tiles.length || rw <= 0 || rh <= 0) return;
    var g = TILE_GAP;
    var cols = Math.ceil(Math.sqrt(tiles.length));
    var rows = Math.ceil(tiles.length / cols);
    var tileW = (rw - g * (cols + 1)) / cols;
    var tileH = (rh - g * (rows + 1)) / rows;
    if (tileW <= 0 || tileH <= 0) return;
    for (var i = 0; i < tiles.length; i++) {
      var col = i % cols;
      var row = Math.floor(i / cols);
      drawTile(
        tiles[i],
        rx + g + col * (tileW + g),
        ry + g + row * (tileH + g),
        tileW,
        tileH,
      );
    }
  }

  // ─────────────────────────────────────────────────────────────────
  // Background image helpers
  // ─────────────────────────────────────────────────────────────────

  /**
   * Lazily load the meeting background image from the page's --bg-image CSS
   * variable (e.g. /static/themes/darktheme.png).  The image is cached in
   * _bgImage after the first successful load.
   */
  function ensureBgImage() {
    if (_bgImageAttempted) return;
    _bgImageAttempted = true;
    try {
      var raw = getComputedStyle(document.body)
        .getPropertyValue("--bg-image")
        .trim();
      // CSS value is url("/path") — extract the path
      var match = raw.match(/url\(["']?([^"')]+)["']?\)/);
      if (!match) return;
      var img = new Image();
      img.onload = function () {
        _bgImage = img;
      };
      img.onerror = function () {
        _bgImage = false;
      };
      img.src = match[1];
    } catch (_) {}
  }

  /**
   * Draw the meeting background image (cover-fit, same as CSS background-size:cover)
   * or fall back to a solid dark colour matching --color-bg.
   */
  function drawBackground(w, h) {
    if (_bgImage && _bgImage.naturalWidth) {
      var iw = _bgImage.naturalWidth,
        ih = _bgImage.naturalHeight;
      var scale = Math.max(w / iw, h / ih);
      var sw = iw * scale,
        sh = ih * scale;
      var sx = (w - sw) / 2,
        sy = (h - sh) / 2;
      _offCtx.drawImage(_bgImage, sx, sy, sw, sh);
    } else {
      _offCtx.fillStyle = "#000000";
      _offCtx.fillRect(0, 0, w, h);
    }
  }

  // ─────────────────────────────────────────────────────────────────
  // Signal bars helper (drawn in tile top-right corner)
  // ─────────────────────────────────────────────────────────────────

  /**
   * Draw 5 cellular signal bars (matching SignalBarsIcon in signal_bars.rs) at
   * position (ox, oy) with total bounding box `size` × `size`.
   *
   * @param {number} level     - 0..5, filled bars count (read from data-signal-level)
   * @param {boolean} lost     - if true, all bars grey + red slash
   * @param {number} ox        - left edge of icon bounding box
   * @param {number} oy        - top edge of icon bounding box
   * @param {number} size      - icon size in canvas pixels
   */
  function drawSignalBars(level, lost, ox, oy, size) {
    // 5 bars, each 3 units wide, 1.5 unit gap; viewBox 0 0 24 24
    var bars = [
      { x: 1.5, h: 6 },
      { x: 6.0, h: 9 },
      { x: 10.5, h: 12 },
      { x: 15.0, h: 15 },
      { x: 19.5, h: 18 },
    ];
    var scale = size / 24;
    var fillColor;
    if (lost) {
      fillColor = "#555";
    } else {
      fillColor =
        level >= 5
          ? "#5bcf9f"
          : level === 4
            ? "#4CAF50"
            : level === 3
              ? "#FFC107"
              : level === 2
                ? "#FF8C00"
                : level === 1
                  ? "#FF4444"
                  : "#555";
    }
    var unfilled = "#555";
    var eff = lost ? 0 : level;
    _offCtx.save();
    _offCtx.translate(ox, oy);
    _offCtx.scale(scale, scale);
    for (var i = 0; i < bars.length; i++) {
      var b = bars[i];
      _offCtx.fillStyle = i + 1 <= eff ? fillColor : unfilled;
      _offCtx.beginPath();
      _offCtx.roundRect(b.x, 22 - b.h, 3, b.h, 1);
      _offCtx.fill();
    }
    if (lost || level === 0) {
      _offCtx.strokeStyle = "#FF0000";
      _offCtx.lineWidth = 2;
      _offCtx.lineCap = "round";
      _offCtx.beginPath();
      _offCtx.moveTo(0.5, 3);
      _offCtx.lineTo(23.5, 21);
      _offCtx.stroke();
    }
    _offCtx.restore();
  }

  // ─────────────────────────────────────────────────────────────────
  // Toast notifications
  // ─────────────────────────────────────────────────────────────────

  /**
   * Draw visible `.peer-toast` notifications exactly as they appear in the
   * meeting UI — centred at the top, stacked downward.
   */
  function drawToasts(w) {
    var container = document.querySelector(".peer-toasts");
    if (!container) return;
    var toasts = container.querySelectorAll(".peer-toast");
    if (!toasts.length) return;

    var TOAST_MAX_W = 360;
    var TOAST_H = 50;
    var TOAST_GAP = 8;
    var TOAST_R = 12;
    var ICON_SZ = 28;
    var ICON_R = 8;
    var PAD_H = 14;
    var ICON_GAP = 10;
    var FONT_NAME = 13;
    var FONT_ACTION = 11;
    var toastW = Math.min(TOAST_MAX_W, w - 32);
    var toastX = (w - toastW) / 2;
    var toastY = 16;

    for (var i = 0; i < toasts.length; i++) {
      var toast = toasts[i];
      var isJoined = toast.classList.contains("toast-joined");
      var isError = toast.classList.contains("toast-error");
      var isSuccess = toast.classList.contains("toast-success");
      var isRec = toast.classList.contains("recording-status-banner");

      // Recording state banners are UI feedback only — omit them from the
      // recorded canvas so they do not appear in the output file.
      if (isRec) {
        continue;
      }

      // Skip toasts whose CSS exit animation has already completed (opacity 0).
      // The element lingers in the DOM for ~100 ms after the animation finishes,
      // so checking computed opacity avoids rendering invisible ghosts.
      try {
        if (parseFloat(window.getComputedStyle(toast).opacity) < 0.05) {
          continue;
        }
      } catch (_e) {}

      // Background pill
      _offCtx.save();
      _offCtx.fillStyle = "rgba(30, 30, 30, 0.88)";
      _offCtx.shadowColor = "rgba(0,0,0,0.4)";
      _offCtx.shadowBlur = 14;
      roundRect(toastX, toastY, toastW, TOAST_H, TOAST_R);
      _offCtx.fill();
      _offCtx.shadowBlur = 0;
      _offCtx.strokeStyle = "rgba(255,255,255,0.08)";
      _offCtx.lineWidth = 1;
      roundRect(toastX, toastY, toastW, TOAST_H, TOAST_R);
      _offCtx.stroke();
      _offCtx.restore();

      // Icon circle
      var iconX = toastX + PAD_H + ICON_SZ / 2;
      var iconCY = toastY + TOAST_H / 2;
      _offCtx.save();
      var iconBg = isJoined
        ? "rgba(52,199,89,0.20)"
        : isError
          ? "rgba(255,59,48,0.20)"
          : isSuccess
            ? "rgba(52,199,89,0.20)"
            : isRec
              ? "rgba(255,59,48,0.20)"
              : "rgba(255,255,255,0.08)";
      _offCtx.fillStyle = iconBg;
      if (_offCtx.roundRect) {
        _offCtx.beginPath();
        _offCtx.roundRect(
          toastX + PAD_H,
          toastY + (TOAST_H - ICON_SZ) / 2,
          ICON_SZ,
          ICON_SZ,
          ICON_R,
        );
        _offCtx.fill();
      } else {
        _offCtx.beginPath();
        _offCtx.arc(iconX, iconCY, ICON_SZ / 2, 0, Math.PI * 2);
        _offCtx.fill();
      }
      _offCtx.restore();

      // Icon glyph — person (joined/left) or REC dot
      if (isRec) {
        _offCtx.save();
        _offCtx.beginPath();
        _offCtx.arc(iconX, iconCY, 5, 0, Math.PI * 2);
        _offCtx.fillStyle = "#ff3b30";
        _offCtx.fill();
        _offCtx.restore();
      } else {
        var glyphColor = isJoined
          ? "#34c759"
          : isError
            ? "#ff3b30"
            : isSuccess
              ? "#34c759"
              : "rgba(255,255,255,0.50)";
        drawStrokeIcon(
          iconX,
          iconCY,
          16,
          function () {
            _offCtx.stroke(
              new Path2D("M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"),
            );
            _offCtx.stroke(new Path2D("M9 7a4 4 0 1 0 8 0 4 4 0 0 0-8 0"));
          },
          glyphColor,
        );
      }

      // Text — name + optional action line
      var nameEl = toast.querySelector(".toast-name");
      var actionEl = toast.querySelector(".toast-action");
      var name = nameEl ? nameEl.textContent.trim() : "";
      var action = actionEl ? actionEl.textContent.trim() : "";
      var textX = toastX + PAD_H + ICON_SZ + ICON_GAP;
      var textMaxW = toastW - PAD_H - ICON_SZ - ICON_GAP - PAD_H;
      _offCtx.save();
      _offCtx.textAlign = "left";
      _offCtx.textBaseline = "middle";
      _offCtx.fillStyle = "rgba(255,255,255,0.95)";
      _offCtx.font =
        "600 " + FONT_NAME + "px -apple-system, BlinkMacSystemFont, sans-serif";
      _offCtx.beginPath();
      _offCtx.rect(textX, toastY, textMaxW, TOAST_H);
      _offCtx.clip();
      _offCtx.fillText(name, textX, iconCY - (action ? 7 : 0), textMaxW);
      if (action) {
        _offCtx.fillStyle = "rgba(255,255,255,0.50)";
        _offCtx.font =
          "500 " +
          FONT_ACTION +
          "px -apple-system, BlinkMacSystemFont, sans-serif";
        _offCtx.fillText(action, textX, iconCY + 7, textMaxW);
      }
      _offCtx.restore();

      toastY += TOAST_H + TOAST_GAP;
    }
  }

  /**
   * Render one frame of the recording to look exactly as the meeting looks
   * to any participant:
   *
   *   No screen share → full tile grid with 20 px outer padding, 84 px
   *                     bottom padding (space for controls bar)
   *   Screen share    → left 2/3 = letterboxed screen content,
   *                     right 1/3 = tile grid of participants
   *
   * Every tile shows the real meeting visual language: rounded corners,
   * surface-elevated background, border, floating name chip at top-left,
   * bottom gradient scrim over video, signal bars in top-right, speaking
   * glow border.
   *
   * Toast notifications (join/leave/etc.) are captured live from the DOM
   * and drawn at the top, matching their CSS position.
   *
   * The controls bar and REC indicator are drawn last (always on top).
   */
  /**
   * Dynamically connect or disconnect the local microphone source from the
   * audio mixer based on the current state of the mic button in the DOM.
   * Called on every animation frame so mic mute/unmute during recording is
   * reflected in the audio output with ≤1 frame latency (~33 ms at 30 fps).
   */
  function updateMicConnection() {
    if (!_micSource || !_mixDest) return;
    var firstBtn = document.querySelector(
      ".video-controls-container .video-control-button",
    );
    var micOn = !!(firstBtn && firstBtn.classList.contains("active"));
    if (micOn === _prevMicOn) return;
    _prevMicOn = micOn;
    try {
      if (micOn) {
        _micSource.connect(_mixDest);
      } else {
        _micSource.disconnect(_mixDest);
      }
    } catch (_e) {
      // Ignore: already connected / already disconnected.
    }
  }

  /**
   * Dynamically connect or disconnect local screen-share audio from the mixer.
   * Detects changes to #screen-share-preview.srcObject so that screen sharing
   * started or stopped AFTER recording began is still captured correctly.
   * Remote screen-share audio travels through WebRTC → Rust's SharedAudioContext
   * → audioStream and is already included in the remote audio mix.
   */
  function updateScreenShareAudio() {
    if (!_audioMixerCtx || !_mixDest) return;
    var ssEl = document.getElementById("screen-share-preview");
    var currentSrc = ssEl ? ssEl.srcObject : null;
    if (currentSrc === _prevSsObject) return;
    // srcObject changed — disconnect the old source first.
    if (_ssAudioSource) {
      try {
        _ssAudioSource.disconnect();
      } catch (_e) {}
      _ssAudioSource = null;
    }
    _prevSsObject = currentSrc;
    // Connect new source if it carries audio (user chose "Share audio").
    if (currentSrc && typeof currentSrc.getAudioTracks === "function") {
      var tracks = currentSrc.getAudioTracks();
      if (tracks.length) {
        try {
          _ssAudioSource = _audioMixerCtx.createMediaStreamSource(
            new MediaStream(tracks),
          );
          _ssAudioSource.connect(_mixDest);
        } catch (e) {
          console.warn(
            "[recording] Could not connect screen-share audio:",
            e.message || e,
          );
          _ssAudioSource = null;
        }
      }
    }
  }

  /**
   * Dedicated 1×1 canvas used by canvasHasContent() to sample a single pixel
   * from a decoder canvas without triggering Chrome's "multiple readback
   * operations" warning.  The decoder canvas was obtained via
   * canvas.getContext("2d") without {willReadFrequently:true} (that option
   * belongs to Rust, not JS), so calling getImageData on it directly produces
   * the warning.  Drawing one pixel into this CPU-backed helper canvas and
   * reading from there avoids the GPU readback on the original canvas.
   */
  var _contentCheckCanvas = null;
  var _contentCheckCtx = null;

  /**
   * Check whether a canvas element has any visible (non-transparent) pixel
   * content by sampling its centre pixel.
   *
   * Video frames drawn via `draw_image_with_video_frame_and_dw_and_dh` always
   * produce α=255 in every canvas pixel.  `clear_rect()` — called by
   * `clear_canvas()` when budget pressure removes a peer from the active
   * decode set — sets α=0 (transparent) across the whole bitmap while leaving
   * `canvas.width` and `canvas.height` unchanged.  Checking one centre pixel
   * cheaply distinguishes "has live video" from "cleared / never drawn".
   *
   * Uses a shared 1×1 helper canvas with {willReadFrequently:true} so that
   * getImageData reads from a CPU-backed surface instead of triggering a GPU
   * readback on the decoder canvas itself.
   *
   * Falls back to `true` (assume content) if getImageData throws (e.g. a
   * CORS-tainted canvas), so that we never silently discard valid video.
   *
   * @param {HTMLCanvasElement} canvas
   * @returns {boolean}
   */
  function canvasHasContent(canvas) {
    if (!canvas.width || !canvas.height) return false;
    // A canvas whose dimensions are STILL at the browser default (300×150)
    // has never been painted by the decoder — decoders resize on the first
    // frame.  Skip the pixel check in that case, it is guaranteed transparent.
    if (canvas.width === 300 && canvas.height === 150) return false;
    // The decoder may have painted at some point and then been cleared
    // (camera off → `force_clear_canvas()` sets every pixel to α=0 while
    // leaving the dimensions at e.g. 640×480).  Sample the centre pixel to
    // distinguish "currently showing video" from "cleared to transparent".
    // Without this check a cleared 640×480 canvas would be treated as live
    // video and drawn as a black rectangle over the tile background, hiding
    // the avatar that should be rendered instead.
    try {
      if (!_contentCheckCanvas) {
        _contentCheckCanvas = document.createElement("canvas");
        _contentCheckCanvas.width = 1;
        _contentCheckCanvas.height = 1;
        _contentCheckCtx = _contentCheckCanvas.getContext("2d", {
          willReadFrequently: true,
        });
      }
      // Draw the centre pixel of the source canvas into the 1×1 helper.
      _contentCheckCtx.clearRect(0, 0, 1, 1);
      _contentCheckCtx.drawImage(
        canvas,
        canvas.width >> 1,
        canvas.height >> 1,
        1,
        1,
        0,
        0,
        1,
        1,
      );
      return _contentCheckCtx.getImageData(0, 0, 1, 1).data[3] > 0;
    } catch (_e) {
      return true; // CORS-tainted or other error — proceed with drawImage
    }
  }

  /**
   * Build the per-frame participant list.
   *
   * The DECODER CANVAS MAP is the authoritative source of remote peer
   * identity — every peer with an attached video decoder canvas becomes a
   * participant, regardless of whether their DOM tile has mounted yet.  DOM
   * tiles are then merged in for metadata (name, mic state, signal, speaking)
   * and to catch peers who are in the meeting but whose camera is off
   * (avatar-only tiles have no decoder canvas registered).
   *
   * Called once per frame from drawFrame() so every rendered tile reflects
   * the actual participant state at that instant.
   *
   * For each participant the video element is resolved in priority order:
   *   1. Decoder canvas from window.__vcGetPeerVideoCanvases() — the source of
   *      truth for what the local user's browser has decoded.  A canvas whose
   *      dimensions differ from the browser default (300×150) has already
   *      been painted; `canvasHasContent()` short-circuits to `true` in that
   *      case, so post-first-frame content is never rejected.
   *   2. DOM <canvas> element inside the tile (fallback when the decoder map
   *      does not include this peer — e.g. before the recording started).
   *   3. null → avatar silhouette rendered by drawTile().
   *
   * @param {Element}  grid             #grid-container DOM element.
   * @param {Object}   decoderCanvasMap sid→HtmlCanvasElement map from __vcGetPeerVideoCanvases.
   * @returns {Array<{domTile:Element|null, videoEl:Element|null, sid:string|null}>}
   */
  function buildFrameParticipants(grid, decoderCanvasMap) {
    var participants = [];
    var seenSids = {};

    // ── Build sid → DOM tile lookup once so both passes can enrich cheaply ─
    var sidToDomTile = {};
    var domTiles = grid.querySelectorAll("[data-tile-root]");
    var domTileOrder = [];
    for (var di = 0; di < domTiles.length; di++) {
      var dt = domTiles[di];
      if (dt.classList.contains("split-screen-tile")) continue;
      var dm = dt.id.match(/^peer-video-(\d+)-div$/);
      if (dm) {
        sidToDomTile[dm[1]] = dt;
      }
      domTileOrder.push({ tile: dt, sid: dm ? dm[1] : null });
    }

    // ── Pass 1: DOM tiles in layout order — preserves the visual grid ────
    // Every DOM tile is included (as an avatar entry when no video is
    // available) so peers who have muted their camera still appear.
    for (var i = 0; i < domTileOrder.length; i++) {
      var t = domTileOrder[i].tile;
      var sid = domTileOrder[i].sid;

      var videoEl = null;
      var decHasContent = false;
      var domHasContent = false;
      var decW = null;
      var domW = null;

      if (sid && decoderCanvasMap[sid]) {
        var dc = decoderCanvasMap[sid];
        decW = dc.width;
        if (dc.width > 0 && dc.height > 0) {
          decHasContent = canvasHasContent(dc);
          if (decHasContent) videoEl = dc;
        }
      }
      if (videoEl === null) {
        var domCanvas = t.querySelector("canvas");
        if (domCanvas) {
          domW = domCanvas.width;
          if (domCanvas.width > 0 && domCanvas.height > 0) {
            domHasContent = canvasHasContent(domCanvas);
            if (domHasContent) videoEl = domCanvas;
          }
        }
      }

      if (sid) seenSids[sid] = true;
      participants.push({
        domTile: t,
        videoEl: videoEl,
        sid: sid,
        _decHasContent: decHasContent,
        _decW: decW,
        _domHasContent: domHasContent,
        _domW: domW,
      });
    }

    // ── Pass 2: decoder peers not covered by any DOM tile ────────────────
    // These are peers whose Dioxus tile has not yet mounted (mid-recording
    // join, camera-on race, or a re-render that momentarily removed the
    // element) — trust the decoder canvas so their video appears immediately
    // in the recording without waiting for the UI to catch up.
    for (var dsid in decoderCanvasMap) {
      if (seenSids[dsid]) continue;
      var dc2 = decoderCanvasMap[dsid];
      var dc2HasContent =
        dc2 && dc2.width > 0 && dc2.height > 0 ? canvasHasContent(dc2) : false;
      var videoEl2 = dc2HasContent ? dc2 : null;
      participants.push({
        domTile: sidToDomTile[dsid] || null,
        videoEl: videoEl2,
        sid: dsid,
        _decHasContent: dc2HasContent,
        _decW: dc2 ? dc2.width : null,
        _domHasContent: false,
        _domW: null,
      });
    }

    return participants;
  }

  /**
   * Detect and immediately log significant video state changes during recording.
   * Runs every frame but only emits console output on actual transitions.
   *
   * Covers four diagnostic scenarios:
   *   1. Local webcam paused/unpaused.
   *   2. HTMLVideoElement readyState transitions (stream set before video
   *      is really activated shows as hasSrcObject=true + readyState < 2).
   *   3. Decoder canvas appearing for the first time for a remote peer
   *      ("decoder registered") — tells you when Rust wired up the canvas.
   *   4. Decoder producing its first non-transparent frame ("decoder frames")
   *      — tells you when actual decoding is in progress.
   *
   * @param {Array}  participants    buildFrameParticipants() result.
   * @param {Element|null} localVid  #webcam HTMLVideoElement (may be null).
   * @param {Object} decoderMap     sid→canvas from __vcGetPeerVideoCanvases.
   */
  function logVideoStateChanges(participants, localVid, decoderMap) {
    // ── Remote peer decoder / DOM-canvas state changes ────────────────
    for (var i = 0; i < participants.length; i++) {
      var p = participants[i];
      var sid = p.sid;
      if (!sid) continue;

      var prev = _peerRecState[sid] || {
        hadDecoder: false,
        hadDecContent: false,
        hadDom: false,
      };

      var hasDecoder = !!decoderMap[sid];
      var hasDecContent = !!p._decHasContent;
      var hasDom = p._domW !== null;

      if (hasDecoder && !prev.hadDecoder) {
        var dc = decoderMap[sid];
        console.log(
          "[recording] decoder registered for peer",
          sid,
          "canvas " + dc.width + "\xd7" + dc.height,
          "frame #" + _dbgFrameCount,
        );
      }

      if (hasDecContent && !prev.hadDecContent) {
        var dc2 = decoderMap[sid];
        console.log(
          "[recording] decoder producing frames for peer",
          sid,
          "canvas " + dc2.width + "\xd7" + dc2.height,
          "frame #" + _dbgFrameCount,
        );
      }

      if (hasDom && !prev.hadDom) {
        console.log(
          "[recording] DOM canvas mounted for peer",
          sid,
          "domW=" + p._domW,
          "frame #" + _dbgFrameCount,
        );
      }

      _peerRecState[sid] = {
        hadDecoder: hasDecoder,
        hadDecContent: hasDecContent,
        hadDom: hasDom,
      };
    }

    if (!localVid) return;

    // ── Local webcam state changes ────────────────────────────────────
    var paused = localVid.paused;
    var readyState = localVid.readyState;
    var hasSrc = !!localVid.srcObject;
    var vw = localVid.videoWidth;
    var vh = localVid.videoHeight;

    // 1. Paused / unpaused
    if (_prevWebcamPaused !== null && paused !== _prevWebcamPaused) {
      console.log(
        "[recording] webcam " + (paused ? "PAUSED" : "RESUMED"),
        "readyState=" + readyState,
        "hasSrcObject=" + hasSrc,
        "frame #" + _dbgFrameCount,
      );
    }
    _prevWebcamPaused = paused;

    // 2. readyState transition
    if (_prevWebcamReadyState !== -1 && readyState !== _prevWebcamReadyState) {
      console.log(
        "[recording] webcam readyState",
        _prevWebcamReadyState,
        "\u2192",
        readyState,
        "videoSize=" + vw + "\xd7" + vh,
        "hasSrcObject=" + hasSrc,
        "paused=" + paused,
        "frame #" + _dbgFrameCount,
      );
    }
    _prevWebcamReadyState = readyState;

    // 3. Stream set before video is really activated (srcObject present but
    //    video not yet ready — readyState < HAVE_CURRENT_DATA).
    if (hasSrc && readyState < 2 && _dbgFrameCount % 30 === 0) {
      console.warn(
        "[recording] webcam srcObject set but video not yet active",
        "readyState=" + readyState,
        "videoSize=" + vw + "\xd7" + vh,
        "paused=" + paused,
        "frame #" + _dbgFrameCount,
      );
    }
  }

  /**
   * Build a lightweight fingerprint of the current scene state.
   *
   * Covers all layout-affecting properties: screen share presence, local mic
   * state, local speaking, and per-peer session ID, video presence, mic muted,
   * speaking, and signal level.  Two frames with the same key produce identical
   * canvas output (modulo live video pixels) — a key change forces a redraw.
   *
   * @param {Array<{sid:string|null, hasVideo:boolean, tileData:Object}>} tileDataList
   * @param {boolean}     hasScreenShare  Whether a screen-share source is active.
   * @param {boolean}     micOn           Whether the local mic is active.
   * @param {string|null} localSpeakColor Speaking highlight colour for the local user.
   * @returns {string}
   */
  function buildSceneKey(tileDataList, hasScreenShare, micOn, localSpeakColor) {
    var parts = [
      "ss:" + (hasScreenShare ? "1" : "0"),
      "m:" + (micOn ? "1" : "0"),
      "ls:" + (localSpeakColor ? "1" : "0"),
    ];
    for (var i = 0; i < tileDataList.length; i++) {
      var e = tileDataList[i];
      var td = e.tileData;
      parts.push(
        (e.sid || "?") +
          ":" +
          (e.hasVideo ? "v" : "-") +
          (td.micMuted ? "M" : "m") +
          (td.speakColor ? "S" : "-") +
          (td.signalLost ? "X" : String(td.signalLevel)),
      );
    }
    return parts.join("|");
  }

  /**
   * Update per-peer A/V sync tracking and decide whether to show the peer's
   * video tile this frame.
   *
   * This gate is intentionally conservative: it ONLY defers showing the video
   * during a brief window when a peer's mic-unmute and their first decoded
   * video frame arrive within AV_SYNC_WINDOW_MS of each other AND we are still
   * inside that window.  In every other case the peer's live video is shown
   * as soon as it is available.
   *
   * The gate must NEVER hide a peer's video just because their mic is muted —
   * a peer with camera on and mic off is a normal state and must always be
   * rendered.  A regression that hid such peers was the reason "no remote
   * video appears in the recording" symptom occurred.
   *
   * @param {string}  sid      Peer session ID.
   * @param {boolean} hasVideo Whether the peer currently has decoded video content.
   * @param {boolean} micMuted Whether the peer's mic is currently muted.
   * @param {number}  now      performance.now() timestamp for this frame.
   * @returns {boolean} true = show video tile; false = hold as avatar this frame.
   */
  function updatePeerAvSync(sid, hasVideo, micMuted, now) {
    // ── Detect audio activation (mic unmute transition) ───────────────────
    var wasMuted = _prevPeerMicMuted[sid];
    if (wasMuted === true && !micMuted) {
      _peerAudioActivatedAt[sid] = now;
    }
    _prevPeerMicMuted[sid] = micMuted;

    // ── Detect video activation (first decoded frame for this session) ────
    if (hasVideo && !_peerVideoActivatedAt[sid]) {
      _peerVideoActivatedAt[sid] = now;
    }

    // No video content → always show avatar; clear the video timestamp so a
    // later re-activation is treated as a fresh event.
    if (!hasVideo) {
      delete _peerVideoActivatedAt[sid];
      return false;
    }

    var audioAt = _peerAudioActivatedAt[sid];
    var videoAt = _peerVideoActivatedAt[sid];

    // ── Joint activation hold ─────────────────────────────────────────────
    // Only defer when BOTH events fired within the sync window AND the video
    // event is still fresh (within the window).  Once the window has elapsed
    // the video is shown regardless of mic state — a permanently-muted peer
    // with camera on is a normal case and must be rendered every frame.
    if (
      audioAt &&
      videoAt &&
      Math.abs(audioAt - videoAt) <= AV_SYNC_WINDOW_MS &&
      now - videoAt <= AV_SYNC_WINDOW_MS &&
      micMuted
    ) {
      return false; // waiting for mic to catch up with just-arrived video
    }

    return true; // default: show whatever video content the peer has
  }

  function drawFrame() {
    if (!_offCtx) return;
    _dbgFrameCount++;

    // ── Phase 1: Audio bookkeeping (always runs, no canvas drawing) ───────
    // Re-check master_gain connection on every frame.  If the initial
    // connect() in start() was missed (masterGain null at that moment), this
    // establishes it the first time window.__vcMasterGain is available.
    ensureMasterGainConnected();

    // Update local mic connection state so mute/unmute during recording is captured.
    updateMicConnection();
    // Update screen-share audio so sharing started/stopped during recording is captured.
    updateScreenShareAudio();

    var w = RECORD_WIDTH;
    var h = RECORD_HEIGHT;

    // Ensure bg image is loading (no-op after first call)
    ensureBgImage();

    // ── Phase 2: Gather meeting state (no canvas drawing yet) ────────────
    var grid = document.getElementById("grid-container");
    if (!grid) return;

    // ── Screen-share source and sharer name ─────────────────────────
    var screenSource = null;
    var screenShareName = "";

    // ── Priority 1 for remote screen share: decoder canvas from Rust ────
    // window.__vcGetPeerScreenCanvases() bypasses the Dioxus DOM the same way
    // the peer video decoder map does, so remote screen shares appear in the
    // recording the instant the first frame is decoded — no waiting for the
    // .split-screen-tile <canvas> to mount and the use_effect to run
    // set_peer_screen_canvas().
    var screenDecoderMap = {};
    if (typeof window.__vcGetPeerScreenCanvases === "function") {
      try {
        var screenCanvases = window.__vcGetPeerScreenCanvases();
        for (var sci = 0; sci < screenCanvases.length; sci++) {
          var scEntry = screenCanvases[sci];
          if (scEntry && scEntry.id && scEntry.canvas) {
            screenDecoderMap[scEntry.id] = scEntry.canvas;
            // Pick the first screen decoder canvas that has decoded content.
            // Multiple peers sharing simultaneously is unsupported by the UI
            // (only one .split-screen-tile is rendered), so this matches the
            // meeting behaviour.
            if (!screenSource) {
              var scc = scEntry.canvas;
              if (scc.width > 0 && scc.height > 0 && canvasHasContent(scc)) {
                screenSource = scc;
              }
            }
          }
        }
      } catch (_e) {
        console.error("[recording] __vcGetPeerScreenCanvases threw:", _e);
      }
    }

    var splitTile = grid.querySelector(".split-screen-tile");
    if (splitTile) {
      if (!screenSource) {
        var sc = splitTile.querySelector("canvas");
        // Accept the canvas regardless of current width/height — WASM may not
        // have drawn the first frame yet (300×150 HTML default).  drawLetterboxed
        // guards the actual draw with `if (!srcW || !srcH) return`.
        if (sc) screenSource = sc;
      }
      // Extract sharer name from the split-screen-tile's floating-name element.
      // Rust formats it as "{peer_display_name}-screen". Reuse getTileName so the
      // `.floating-name-text` span (added 2026-07-07) is read correctly here too —
      // the old inline direct-text-node scan returned "" after that markup change,
      // leaving remote screen-share tiles unlabelled in the recording.
      screenShareName = getTileName(splitTile);
    }
    var localScreenEl = document.getElementById("screen-share-preview");
    if (!screenSource && localScreenEl) {
      // Detect local screen share by checking srcObject directly — more reliable
      // than style.display (which may lag behind Dioxus re-renders) or videoWidth
      // (which is 0 until the first frame is decoded from the MediaStream).
      var localSrcObj = localScreenEl.srcObject;
      var hasLiveScreenTrack =
        localSrcObj &&
        typeof localSrcObj.getVideoTracks === "function" &&
        localSrcObj.getVideoTracks().some(function (t) {
          return t.readyState === "live";
        });
      if (hasLiveScreenTrack) {
        // The <video> element is styled `display:none` while off and toggled
        // to `display:block` when sharing; hidden elements can pause their
        // playback in some browsers, leaving videoWidth stuck at 0 so the
        // recording sees a blank source.  Kick play() every frame while the
        // element is present but paused, and safely swallow the promise since
        // autoplay may reject without user gesture.
        if (localScreenEl.paused && typeof localScreenEl.play === "function") {
          var pp = localScreenEl.play();
          if (pp && typeof pp.catch === "function") {
            pp.catch(function () {});
          }
        }
        // Only treat as a real source once the video has produced dimensions;
        // drawLetterboxed() early-returns when videoWidth === 0, which would
        // leave the left panel blank for the whole recording if we accepted
        // the element too eagerly here.
        if (localScreenEl.videoWidth > 0 && localScreenEl.videoHeight > 0) {
          screenSource = localScreenEl;
          screenShareName = _localUserName
            ? _localUserName + "-screen"
            : "screen";
        }
      }
    }

    // ── Read actual control states from DOM ──────────────────────────
    // Must happen before the local tile is collected so !micOn is correct.
    var micOn = false;
    var vcContainer = document.querySelector(".video-controls-container");
    if (vcContainer) {
      var cBtns = vcContainer.querySelectorAll(".video-control-button");
      micOn = !!(cBtns[0] && cBtns[0].classList.contains("active"));
    }

    // ── Live decoder canvases (authoritative source) ──────────────────
    // window.__vcGetPeerVideoCanvases() is installed by Rust before recording
    // starts (prepare_recording_peer_canvases).  It returns an Array of
    // {id: sessionId, canvas: HtmlCanvasElement} for every peer whose video
    // decoder has an attached canvas — regardless of whether Dioxus has
    // mounted the DOM <canvas> element (which it may not have when
    // show_canvas=false due to force_avatar, budget pressure, or the 50 ms
    // reactive throttle on camera-on events).
    var decoderCanvasMap = {};
    var decoderCanvasRaw = [];
    var decoderFnAvailable =
      typeof window.__vcGetPeerVideoCanvases === "function";
    if (decoderFnAvailable) {
      try {
        var decoderCanvases = window.__vcGetPeerVideoCanvases();
        for (var dci = 0; dci < decoderCanvases.length; dci++) {
          var dcEntry = decoderCanvases[dci];
          if (dcEntry && dcEntry.id && dcEntry.canvas) {
            decoderCanvasMap[dcEntry.id] = dcEntry.canvas;
            decoderCanvasRaw.push({
              id: dcEntry.id,
              w: dcEntry.canvas.width,
              h: dcEntry.canvas.height,
            });
          }
        }
      } catch (_e) {
        console.error("[recording] __vcGetPeerVideoCanvases threw:", _e);
      }
    }

    // ── Camera participant tiles ─────────────────────────────────────
    // For each frame get the actual current participant list, apply A/V sync
    // gating per peer, and build the tile data list for change detection.
    var participants = buildFrameParticipants(grid, decoderCanvasMap);
    var now = performance.now();
    var tiles = [];
    var tileDataList = [];
    var frameHasLiveVideo = false;

    for (var i = 0; i < participants.length; i++) {
      var p = participants[i];
      // videoEl is pre-resolved (decoder canvas > DOM canvas > null), so pass
      // it explicitly rather than having collectTileData redo the lookup.
      var td = collectTileData(
        p.domTile,
        p.videoEl,
        undefined,
        undefined,
        undefined,
        {},
      );

      // ── A/V sync gating ───────────────────────────────────────────────
      // When mic-unmute and first-video-frame arrive within AV_SYNC_WINDOW_MS
      // for the same peer, hold the tile as an avatar until BOTH audio and
      // video are simultaneously confirmed in this frame.
      if (p.sid) {
        var avReady = updatePeerAvSync(p.sid, !!td.videoEl, td.micMuted, now);
        if (!avReady && td.videoEl) {
          td.videoEl = null; // hold as avatar during joint A/V activation
        }
      }

      if (td.videoEl) frameHasLiveVideo = true;
      tiles.push(td);
      tileDataList.push({ sid: p.sid, hasVideo: !!td.videoEl, tileData: td });
    }

    // ── Local webcam element (fetched early so readiness logs can use it) ──
    var localCamEl = document.getElementById("webcam");

    // ── Local user tile ──────────────────────────────────────────────────
    var hostNav = document.getElementById("host-controls-nav");
    // Read local speaking state from the data-speaking attribute set by Rust
    // on #host-controls-nav.  Fall back to null (no highlight) when absent.
    var localSpeakColor =
      hostNav && hostNav.dataset && hostNav.dataset.speaking === "true"
        ? "#2ecc71"
        : null;
    // Append "(Host)" to the local user's name chip when they are the host,
    // matching the CrownIcon label shown on remote-peer host tiles.
    var localDisplayName = _localIsHost
      ? _localUserName + " (Host)"
      : _localUserName;
    // Local user has no signal indicator in the DOM; pass -1 so no bars drawn.
    // Pass local mic muted state (inverse of micOn) so the mic icon reflects
    // the real control-bar state for the local user's tile.
    var localTd = collectTileData(
      null,
      localCamEl,
      localDisplayName,
      localSpeakColor,
      !micOn,
    );
    if (localTd.videoEl) frameHasLiveVideo = true;
    tiles.push(localTd);
    tileDataList.push({
      sid: "local",
      hasVideo: !!localTd.videoEl,
      tileData: localTd,
    });

    // ── Phase 3: Scene change detection ──────────────────────────────────
    var sceneKey = buildSceneKey(
      tileDataList,
      !!screenSource,
      micOn,
      localSpeakColor,
    );
    var sceneChanged = sceneKey !== _prevSceneKey;
    _prevSceneKey = sceneKey;

    // ── Per-second render decision log ───────────────────────────────────
    // Reports every second whether the frame is being drawn or skipped and
    // exactly which of {scene change | live video | screen share} triggered
    // the redraw.  Also enumerates the screen decoder canvas map so it is
    // obvious when a remote screen share is (or isn't) reaching the recording.
    if (_dbgFrameCount % 30 === 0) {
      var screenDecoderSummary = [];
      for (var sdsid in screenDecoderMap) {
        var sdc = screenDecoderMap[sdsid];
        screenDecoderSummary.push({
          sid: sdsid,
          w: sdc.width,
          h: sdc.height,
          hasContent: sdc.width > 0 && sdc.height > 0 && canvasHasContent(sdc),
        });
      }
      console.log(
        "[recording] frame #" + _dbgFrameCount + " render decision",
        "sceneChanged=" + sceneChanged,
        "frameHasLiveVideo=" + frameHasLiveVideo,
        "screenSource=" +
          (screenSource
            ? screenSource.tagName +
              (screenSource.id ? "#" + screenSource.id : "") +
              " " +
              (screenSource.videoWidth || screenSource.width || 0) +
              "\xd7" +
              (screenSource.videoHeight || screenSource.height || 0)
            : "none"),
        "screenDecoders=" + JSON.stringify(screenDecoderSummary),
        "willRedraw=" + (sceneChanged || frameHasLiveVideo || !!screenSource),
      );
    }
    // ── Per-second video-readiness log ──────────────────────────────────
    // Fires every 30 frames (~1 s at 30 fps) so it's easy to track whether
    // each remote peer's canvas has live content during a recording session.
    if (_dbgFrameCount % 30 === 0) {
      var readiness = [];
      for (var ri = 0; ri < participants.length; ri++) {
        var rp = participants[ri];
        readiness.push({
          sid: rp.sid,
          videoReady: rp.videoEl !== null,
          source: !rp.videoEl
            ? "avatar"
            : !rp.domTile
              ? "decoder-only"
              : rp._decHasContent
                ? "decoder"
                : "dom",
          decW: rp._decW,
          decContent: rp._decHasContent,
          domW: rp._domW,
          domContent: rp._domHasContent,
        });
      }
      // Local webcam state included in the per-second snapshot.
      var webcamState = null;
      if (localCamEl) {
        var lcHasSrc = !!localCamEl.srcObject;
        var lcLiveTracks =
          lcHasSrc && typeof localCamEl.srcObject.getVideoTracks === "function"
            ? localCamEl.srcObject.getVideoTracks().filter(function (t) {
                return t.readyState !== "ended";
              }).length
            : 0;
        webcamState = {
          paused: localCamEl.paused,
          readyState: localCamEl.readyState,
          hasSrcObject: lcHasSrc,
          liveTracks: lcLiveTracks,
          srcBeforeReady: lcHasSrc && localCamEl.readyState < 2,
          videoW: localCamEl.videoWidth,
          videoH: localCamEl.videoHeight,
        };
      }
      console.log(
        "[recording] frame #" +
          _dbgFrameCount +
          " video-ready participants:" +
          readiness.length,
        JSON.stringify({ peers: readiness, webcam: webcamState }),
      );
    }

    // ── Debug snapshot (frame 1, then every ~5 s at 30 fps) ───────────
    if (_dbgFrameCount === 1 || _dbgFrameCount % 150 === 0) {
      // Decoder canvas map summary
      console.log(
        "[recording] frame #" +
          _dbgFrameCount +
          " __vcGetPeerVideoCanvases available=" +
          decoderFnAvailable +
          " decoderCanvases=" +
          decoderCanvasRaw.length,
        JSON.stringify(decoderCanvasRaw),
      );

      // Per-participant summary
      var dbgRemote = [];
      for (var di = 0; di < participants.length; di++) {
        var dp = participants[di];
        dbgRemote.push({
          sid: dp.sid,
          tileId: dp.domTile ? dp.domTile.id : null,
          videoReady: dp.videoEl !== null,
          source: !dp.videoEl
            ? "avatar"
            : !dp.domTile
              ? "decoder-only"
              : dp._decHasContent
                ? "decoder"
                : "dom",
          videoW: dp.videoEl
            ? dp.videoEl.width || dp.videoEl.videoWidth || 0
            : null,
          videoH: dp.videoEl
            ? dp.videoEl.height || dp.videoEl.videoHeight || 0
            : null,
          decW: dp._decW,
          decContent: dp._decHasContent,
          domW: dp._domW,
          domContent: dp._domHasContent,
        });
      }
      console.log(
        "[recording] frame #" +
          _dbgFrameCount +
          " participants:" +
          dbgRemote.length +
          " screenShare:" +
          (screenSource ? "yes" : "no"),
        JSON.stringify(dbgRemote),
      );
    }

    // ── Video state change detection (fires immediately on transition) ──
    logVideoStateChanges(participants, localCamEl, decoderCanvasMap);

    // ── Phase 4: Skip redraw when nothing has changed ─────────────────────
    // If the scene key matches the previous frame AND no live pixels are
    // changing (no peer video, no screen share), the offscreen canvas already
    // contains the correct image.  Skip the expensive draw pass; rafLoop()
    // pushes a requestFrame() at TARGET_FPS regardless, so the recording
    // timeline advances without gaps even during fully-static meeting moments.
    //
    // Screen-share sources (both local <video> and remote decoder <canvas>)
    // update pixels every frame, so their presence always forces a redraw.
    if (!sceneChanged && !frameHasLiveVideo && !screenSource) {
      return;
    }

    // ── Phase 5: Render scene to offscreen canvas ─────────────────────────
    // ── Background (meeting theme image or solid fallback) ────────────
    drawBackground(w, h);

    // ── Layout grid area (matches #grid-container CSS padding) ───────
    var padT = GRID_PAD;
    var padL = GRID_PAD;
    var padR = GRID_PAD;
    var padB = CONTROLS_BAR_H;

    if (screenSource) {
      var leftW = Math.round((w * 2) / 3);
      _offCtx.fillStyle = "rgba(0,0,0,0.55)";
      _offCtx.fillRect(0, 0, leftW, h);
      drawLetterboxed(
        screenSource,
        padL,
        padT,
        leftW - padL - 4,
        h - padT - padB,
      );
      // Draw the sharer's name chip over the screen-share panel, matching the
      // floating-name label visible on the split-screen-tile in the real meeting.
      if (screenShareName) {
        drawNameChip(
          screenShareName,
          padL,
          padT,
          leftW - padL - 4,
          h - padT - padB,
        );
      }
      _offCtx.fillStyle = "#38383A";
      _offCtx.fillRect(leftW, 0, 1, h);
      // Right panel: single-column stack of participant tiles, matching the
      // real meeting layout where peers are arranged vertically during screen share.
      drawSingleColumn(
        tiles,
        leftW + padL,
        padT,
        w - leftW - padL - padR,
        h - padT - padB,
      );
    } else {
      drawGrid(tiles, padL, padT, w - padL - padR, h - padT - padB);
    }

    // ── Toast notifications (live from DOM) ───────────────────────────
    drawToasts(w);

    // ── Controls bar (bottom centre) ─────────────────────────────────
    drawControlsBar(w, h);

    // ── REC indicator (top right) ─────────────────────────────────────
    drawRecIndicator(w);
  }

  /**
   * Ensure master_gain is connected to mixDest.
   *
   * Called at the start of every drawFrame() as a safety net: if the
   * initial masterGain.connect(mixDest) in start() was skipped (window.
   * __vcMasterGain was null) or threw, this re-tries on every frame until
   * it succeeds.  The check `mg === _masterGainRef` makes it a no-op once
   * connected, so there is no overhead on the hot path.
   */
  function ensureMasterGainConnected() {
    if (!_mixDest) return;
    var mg =
      typeof window.__vcMasterGain !== "undefined"
        ? window.__vcMasterGain
        : null;
    if (!mg || mg === _masterGainRef) return;
    // Only connect when both nodes share the same AudioContext — a
    // cross-context connect() throws InvalidAccessError and must be skipped.
    try {
      if (mg.context === _audioMixerCtx) {
        mg.connect(_mixDest);
        _masterGainRef = mg;
      }
    } catch (_) {}
  }

  /**
   * Frame loop — renders at TARGET_FPS and pushes one video frame per interval.
   *
   * Uses `setInterval` rather than `requestAnimationFrame` because rAF is
   * heavily throttled (or fully paused) when the tab is backgrounded.  With
   * rAF a backgrounded recording would push far fewer than 30 video frames per
   * second while audio kept flowing at real time — the resulting file would
   * play the audio ahead of the video, exactly the "audio arrives first,
   * video and screen-share catch up later" symptom reported by the user.
   * `setInterval` (like `setTimeout`) still fires in backgrounded tabs, so the
   * video timeline stays aligned with real time regardless of tab visibility.
   */
  function frameTick() {
    // Run during both activating (captureStream started, recorder not yet
    // started) and recording states.
    if (_state !== "recording" && _state !== "activating") return;

    var now = performance.now();
    var elapsed = now - _lastFrameMs;
    if (elapsed < FRAME_INTERVAL_MS - 1) {
      return; // interval hasn't quite elapsed (setInterval has ±1 ms jitter)
    }
    // Drift-corrected baseline: absorb any overrun so the next interval
    // starts from the ideal time rather than the actual (late) fire time.
    _lastFrameMs = now - (elapsed % FRAME_INTERVAL_MS);

    // Draw current meeting state to the offscreen canvas.  Phase 4 inside
    // drawFrame() returns early when nothing has changed and no live video
    // is present — the previous canvas content is reused in that case.
    drawFrame();

    // Push an explicit video frame on every interval tick.  captureStream(0)
    // (manual mode) does NOT auto-capture; requestFrame() is the only way to
    // get a frame into the video track.  This guarantees one frame per tick
    // regardless of whether drawFrame() drew anything new, preventing gaps in
    // the recording during static meeting moments.
    if (_state === "recording" && _videoTrack) {
      try {
        _videoTrack.requestFrame();
      } catch (_) {}
    }
  }

  function stopRafLoop() {
    if (_animFrameId !== null) {
      clearInterval(_animFrameId);
      _animFrameId = null;
    }
  }

  /**
   * Visibility change handler (kept for symmetry with removeEventListener in
   * onstop, but no longer pauses the recorder).
   *
   * Previously this paused MediaRecorder when the tab was hidden to avoid
   * capturing unrelated content.  That pause created gaps in the recording
   * and caused duration drift.  Since drawFrame() reads from the meeting DOM
   * (not the screen), hiding the tab does not change what is composited —
   * there is no "unrelated content" to avoid.
   */
  function onVisibilityChange() {
    // intentionally empty — recording continues uninterrupted when tab hides
  }

  /** Trigger a browser download for a Blob with the given file extension. */
  function triggerDownload(blob, ext) {
    var url = URL.createObjectURL(blob);
    var a = document.createElement("a");
    var ts = new Date().toISOString().replace(/[:.]/g, "-");
    a.href = url;
    a.download = "meeting-recording-" + ts + "." + ext;
    document.body.appendChild(a);
    a.click();
    document.body.removeChild(a);
    // Revoke after a short delay to let the download start.
    setTimeout(function () {
      URL.revokeObjectURL(url);
    }, 10000);
  }

  window.__vcRecording = {
    /**
     * Start recording.
     *
     * Composite pipeline
     * ──────────────────
     * STAGE 1 — Audio mixer (built synchronously inside the user-gesture frame)
     *
     *   WebSocket / WebTransport tracks
     *     → NetEQ decoder → peer_gain → master_gain ─┐
     *   Local microphone (getUserMedia) ──────────────┼→ mixDest → audio track
     *   Screen-share audio (dynamic via updateScreenShareAudio) ────────────────┘
     *
     * STAGE 2 — Composite video track
     *
     *   Decoded peer HtmlCanvasElements + local #webcam video element
     *     → drawFrame() rendering engine
     *       (grid / screen-share layout, participant tiles, speaker borders,
     *        name chips, transport badges, controls bar, REC indicator, toasts)
     *     → _offCanvas (1280 × 720)
     *     → _offCanvas.captureStream(30) → video track
     *
     * STAGE 3 — Record
     *
     *   new MediaStream([ composite video track, mixed audio track ])
     *     → MediaRecorder → streamed plaintext file or encrypted RAM fallback
     *
     * @param {string[]} peerSessionIds  Session IDs of currently connected peers.
     * @param {function(string):void} onStateChange  Called with state strings.
     * @param {string} localUserName   Display name of the recording user.
     * @param {boolean} isLocalUserHost  Whether the recording user is the host.
     */
    start: async function (
      peerSessionIds,
      onStateChange,
      localUserName,
      isLocalUserHost,
    ) {
      if (_state !== "idle") {
        console.warn("[recording] start() called while state=" + _state);
        return;
      }

      _peerIds = Array.isArray(peerSessionIds) ? peerSessionIds : [];
      _onStateChange = onStateChange;
      _chunks = [];
      _writer = null;
      _writeChain = Promise.resolve();
      // Reset the in-memory fallback ceiling + counters for this recording.
      // window.__VC_RECORDING_MAX_FALLBACK_BYTES__ is a TEST-ONLY override
      // (E2E injects a tiny ceiling so the auto-stop can be exercised without a
      // multi-minute real recording); production always uses the default.
      _fallbackMaxBytes =
        typeof window.__VC_RECORDING_MAX_FALLBACK_BYTES__ === "number" &&
        window.__VC_RECORDING_MAX_FALLBACK_BYTES__ > 0
          ? window.__VC_RECORDING_MAX_FALLBACK_BYTES__
          : IN_MEMORY_FALLBACK_MAX_BYTES_DEFAULT;
      _fallbackBytes = 0;
      _fallbackCapTripped = false;
      _mimeType = pickMimeType();
      // Store the local user's display name so drawFrame() can render their tile.
      _localUserName = typeof localUserName === "string" ? localUserName : "";
      // Store whether the local user is the host so drawFrame() can append "(Host)".
      _localIsHost = isLocalUserHost === true;

      if (!_mimeType) {
        console.error(
          "[recording] No supported MIME type found for MediaRecorder",
        );
        setState("idle");
        return;
      }

      setState("activating");

      // ── STAGE 1: Audio mixer (must happen inside the user-gesture frame) ──
      // Read the SharedAudioContext and master_gain GainNode exposed by Rust
      // via window.__vcSharedAudioCtx / window.__vcMasterGain so all audio
      // sources share one clock (no cross-context drift, no AV sync issues).
      //
      // Audio graph:
      //   peer_gain (per remote peer) → master_gain ─┐
      //   local mic (getUserMedia, below) ────────────┼→ mixDest → audio track
      //   screen-share audio (updateScreenShareAudio) ┘
      var sharedAudioCtx =
        typeof window.__vcSharedAudioCtx !== "undefined"
          ? window.__vcSharedAudioCtx
          : null;
      var masterGain =
        typeof window.__vcMasterGain !== "undefined"
          ? window.__vcMasterGain
          : null;

      // If masterGain exists, use its own AudioContext as the mixer regardless
      // of what __vcSharedAudioCtx reports.  This guarantees that masterGain
      // and mixDest are always in the SAME AudioContext — a cross-context
      // AudioNode.connect() throws InvalidAccessError and silently discards
      // all remote audio.
      var AudioCtx = window.AudioContext || window.webkitAudioContext;
      if (masterGain && masterGain.context) {
        _audioMixerCtx = masterGain.context;
      } else {
        _audioMixerCtx =
          sharedAudioCtx ||
          (AudioCtx ? new AudioCtx({ sampleRate: 48000 }) : null);
      }
      var mixedTrack = null;
      var micStream = null;

      if (_audioMixerCtx) {
        var mixDest = _audioMixerCtx.createMediaStreamDestination();
        _mixDest = mixDest;

        if (masterGain) {
          // Connect master_gain directly to mixDest — plain fan-out within one
          // AudioContext.  All decoded remote-peer audio (peer_gain →
          // master_gain) will be captured automatically the moment each peer's
          // NetEQ worklet connects, including peers who unmute AFTER recording
          // starts.
          try {
            masterGain.connect(mixDest);
            _masterGainRef = masterGain;
          } catch (e) {
            console.warn(
              "[recording] Could not connect master_gain to mixer:",
              e.message || e,
            );
          }
        }

        // Resume the AudioContext now, while still inside the user-gesture
        // stack frame.  The SharedAudioContext is already running; for a fresh
        // fallback context resume() is required by some browsers.
        if (_audioMixerCtx.state === "suspended") {
          _audioMixerCtx.resume().catch(function () {});
        }
      }

      // ── Fallback encryption key generation (first await) ─────────────────
      // Generate a non-extractable 256-bit AES-GCM key for the in-memory
      // fallback used when streaming-to-disk is unavailable. The primary File
      // System Access path writes MediaRecorder chunks to disk as plaintext;
      // only fallback chunks are encrypted in RAM, then decrypted and assembled
      // into a standard playable file on save. No separate key file is produced.
      _e2eeKey = null;
      if (window.crypto && window.crypto.subtle) {
        try {
          _e2eeKey = await window.crypto.subtle.generateKey(
            { name: "AES-GCM", length: 256 },
            false /* non-extractable — key stays in browser KeyStore */,
            ["encrypt", "decrypt"],
          );
        } catch (e) {
          console.warn(
            "[recording] E2EE key generation failed, recording unencrypted:",
            e,
          );
          _e2eeKey = null;
        }
      }

      // ── File picker (second await) ────────────────────────────────────────
      // Open the writable stream immediately after the picker so each
      // ondataavailable chunk can be streamed directly to disk instead of
      // accumulating in _chunks (prevents ~1 GB RAM growth on a 1-hour
      // recording and eliminates the 3× memory spike at save time).
      _fileHandle = null;
      _writer = null;
      _writeChain = Promise.resolve();
      if (typeof window.showSaveFilePicker === "function") {
        var ext = fileExtension(_mimeType);
        var ts = new Date().toISOString().replace(/[:.]/g, "-");
        try {
          _fileHandle = await window.showSaveFilePicker({
            suggestedName: "meeting-recording-" + ts + "." + ext,
            types: [
              {
                description: "Video file",
                accept: { [_mimeType.split(";")[0]]: ["." + ext] },
              },
            ],
          });
          // Open the writable stream now, inside the user-gesture frame, so
          // ondataavailable can call _writer.write() without any further prompts.
          try {
            _writer = await _fileHandle.createWritable();
          } catch (we) {
            console.warn(
              "[recording] createWritable() failed, falling back to in-memory:",
              we,
            );
            _fileHandle = null;
            _writer = null;
          }
        } catch (e) {
          if (e.name === "AbortError") {
            // User cancelled the picker — abort recording and clean up audio.
            if (_masterGainRef && _mixDest) {
              try {
                _masterGainRef.disconnect(_mixDest);
              } catch (_) {}
            }
            _masterGainRef = null;
            if (_audioMixerCtx && _audioMixerCtx !== sharedAudioCtx) {
              _audioMixerCtx.close().catch(function () {});
            }
            _audioMixerCtx = null;
            _mixDest = null;
            _e2eeKey = null;
            setState("idle");
            return;
          }
          // Other errors (e.g. security restrictions): fall back to auto-download.
          console.warn(
            "[recording] showSaveFilePicker failed, falling back to download:",
            e,
          );
          _fileHandle = null;
        }
      }

      // ── STAGE 2: Composite video canvas ───────────────────────────────────
      _offCanvas = document.createElement("canvas");
      _offCanvas.width = RECORD_WIDTH;
      _offCanvas.height = RECORD_HEIGHT;
      _offCtx = _offCanvas.getContext("2d");

      // Draw an initial frame so captureStream() has content immediately.
      drawFrame();

      // ── STAGE 2: Composite video track ───────────────────────────────────
      // captureStream(0) = manual frame mode.  Frames are only pushed to the
      // video track when rafLoop() calls videoTrack.requestFrame() explicitly.
      // This is more reliable than captureStream(30) (automatic mode) because
      // Chrome's automatic capture silently skips frames when canvas pixels
      // have not changed since the previous capture, causing the recorded video
      // to be shorter than the actual meeting duration during static periods.
      //
      // The draw loop is started IMMEDIATELY after captureStream so that
      // every new peer join, camera-on event, or screen-share change is
      // composited into the next captured frame without waiting for the
      // MediaRecorder to fire its onstart callback.  We use setInterval (not
      // rAF) so the tick keeps firing at real time while the tab is in the
      // background — see the comment on frameTick() for the full rationale.
      var videoStream;
      try {
        videoStream = _offCanvas.captureStream(0);
        _videoTrack = videoStream.getVideoTracks()[0] || null;
        // Start continuous draw loop immediately — runs in "activating" state
        // so the canvas is refreshed every frame even before the recorder fires.
        // setInterval fires slightly faster than FRAME_INTERVAL_MS so the
        // drift-corrected `elapsed >= FRAME_INTERVAL_MS - 1` gate in frameTick
        // reliably catches every intended frame boundary.
        if (_animFrameId === null) {
          _lastFrameMs = performance.now();
          _animFrameId = setInterval(
            frameTick,
            Math.max(1, FRAME_INTERVAL_MS - 4),
          );
        }
      } catch (e) {
        console.error("[recording] captureStream() failed:", e);
        if (_writer) {
          _writer.abort().catch(function () {});
          _writer = null;
        }
        _writeChain = Promise.resolve();
        if (_masterGainRef && _mixDest) {
          try {
            _masterGainRef.disconnect(_mixDest);
          } catch (_) {}
        }
        _masterGainRef = null;
        if (_audioMixerCtx && _audioMixerCtx !== sharedAudioCtx) {
          _audioMixerCtx.close().catch(function () {});
        }
        _audioMixerCtx = null;
        _mixDest = null;
        _e2eeKey = null;
        setState("idle");
        return;
      }

      // ── Local mic (third await) ───────────────────────────────────────────
      // Acquired here and connected directly to mixDest (NOT through
      // master_gain) to avoid speaker bleed.  AEC/NS/AGC are disabled so the
      // WebAudio mixer is the sole authority on what reaches the recording.
      if (_audioMixerCtx) {
        try {
          if (navigator.mediaDevices && navigator.mediaDevices.getUserMedia) {
            micStream = await navigator.mediaDevices.getUserMedia({
              audio: {
                echoCancellation: false,
                noiseSuppression: false,
                autoGainControl: false,
              },
              video: false,
            });
            _micSource = _audioMixerCtx.createMediaStreamSource(micStream);
            var firstBtn = document.querySelector(
              ".video-controls-container .video-control-button",
            );
            var localMicActive = !!(
              firstBtn && firstBtn.classList.contains("active")
            );
            if (localMicActive) {
              _micSource.connect(mixDest);
            }
            _prevMicOn = localMicActive;
          }
        } catch (e) {
          console.warn("[recording] Could not get microphone:", e.message || e);
        }

        var mixedTracks = mixDest.stream.getAudioTracks();
        if (mixedTracks.length) mixedTrack = mixedTracks[0];
      }

      if (!mixedTrack) {
        console.warn(
          "[recording] No audio track available; recording will be silent.",
        );
      }

      // ── STAGE 3: Composite MediaStream → MediaRecorder → chunks ──────────
      //   composite video track (captureStream) ─┐
      //                                          ├→ MediaStream → MediaRecorder
      //   mixed audio track (mixDest.stream) ────┘
      var videoTracks = videoStream.getVideoTracks();
      var compositeStream = new MediaStream(
        mixedTrack ? videoTracks.concat([mixedTrack]) : videoTracks,
      );

      var options = {
        mimeType: _mimeType,
        videoBitsPerSecond: 2500000,
        audioBitsPerSecond: 128000,
      };

      try {
        _recorder = new MediaRecorder(compositeStream, options);
      } catch (e) {
        console.error("[recording] MediaRecorder constructor failed:", e);
        stopRafLoop();
        _videoTrack = null;
        if (_writer) {
          _writer.abort().catch(function () {});
          _writer = null;
        }
        _writeChain = Promise.resolve();
        if (_micSource) {
          try {
            _micSource.disconnect();
          } catch (_) {}
          _micSource = null;
        }
        if (_masterGainRef && _mixDest) {
          try {
            _masterGainRef.disconnect(_mixDest);
          } catch (_) {}
        }
        _masterGainRef = null;
        if (_audioMixerCtx && _audioMixerCtx !== sharedAudioCtx) {
          _audioMixerCtx.close().catch(function () {});
        }
        _audioMixerCtx = null;
        _mixDest = null;
        _prevMicOn = null;
        if (_ssAudioSource) {
          try {
            _ssAudioSource.disconnect();
          } catch (_) {}
          _ssAudioSource = null;
        }
        _prevSsObject = null;
        if (micStream) {
          micStream.getTracks().forEach(function (t) {
            t.stop();
          });
        }
        _e2eeKey = null;
        setState("idle");
        return;
      }

      _recorder.ondataavailable = function (e) {
        if (!(e.data && e.data.size > 0)) return;
        if (_writer) {
          // Streaming path: write each chunk directly to disk so it never
          // accumulates in RAM.  Writes are serialised through _writeChain
          // because the FileSystemWritableFileStream is not concurrency-safe.
          var chunk = e.data;
          _writeChain = _writeChain
            .then(function () {
              return _writer.write(chunk);
            })
            .catch(function (err) {
              console.error("[recording] Incremental write failed:", err);
            });
        } else if (_e2eeKey && window.crypto && window.crypto.subtle) {
          // In-memory fallback (no file handle): encrypt each chunk so the
          // RAM copy is always ciphertext rather than raw video.
          var chunkIv = window.crypto.getRandomValues(new Uint8Array(12));
          var chunkKey = _e2eeKey;
          _chunks.push(
            e.data.arrayBuffer().then(function (buf) {
              return window.crypto.subtle
                .encrypt({ name: "AES-GCM", iv: chunkIv }, chunkKey, buf)
                .then(function (ct) {
                  return { iv: chunkIv, ct: ct };
                });
            }),
          );
          // Track raw byte growth (e.data.size is available synchronously on
          // the Blob regardless of the async encrypt) and auto-stop before RAM
          // grows unbounded on browsers without disk streaming.
          _fallbackBytes += e.data.size;
          if (!_fallbackCapTripped && _fallbackBytes > _fallbackMaxBytes) {
            _fallbackCapTripped = true;
            console.warn(
              "[recording] In-memory fallback exceeded " +
                _fallbackMaxBytes +
                " bytes (no disk-streaming API on this browser) — auto-stopping and saving now to avoid an out-of-memory crash.",
            );
            window.__vcRecording.stop();
          }
        } else {
          _chunks.push(e.data);
          // Same byte-ceiling guard for the raw (unencrypted) fallback branch.
          _fallbackBytes += e.data.size;
          if (!_fallbackCapTripped && _fallbackBytes > _fallbackMaxBytes) {
            _fallbackCapTripped = true;
            console.warn(
              "[recording] In-memory fallback exceeded " +
                _fallbackMaxBytes +
                " bytes (no disk-streaming API on this browser) — auto-stopping and saving now to avoid an out-of-memory crash.",
            );
            window.__vcRecording.stop();
          }
        }
      };

      _recorder.onstart = function () {
        setState("recording");
        _recorderPaused = false;
        // rAF loop is already running (started right after captureStream).
      };

      _recorder.onerror = function (e) {
        console.error("[recording] MediaRecorder error:", e);
      };

      _recorder.onstop = function () {
        stopRafLoop();
        setState("saving");

        var ext = fileExtension(_mimeType);

        // Capture volatile state before the async save operation.
        var capturedKey = _e2eeKey;
        var capturedHandle = _fileHandle;
        var capturedWriter = _writer;
        var capturedWriteChain = _writeChain;
        var capturedChunks = _chunks;
        _e2eeKey = null;
        _fileHandle = null;
        _writer = null;
        _writeChain = Promise.resolve();
        _chunks = [];
        _recorder = null;
        _offCanvas = null;
        _offCtx = null;
        _videoTrack = null;
        _peerIds = [];
        _localUserName = "";
        _localIsHost = false;
        // Allow bg-image to be re-read next recording (theme may have changed).
        _bgImageAttempted = false;
        _bgImage = null;
        // Reset A/V sync and scene-change tracking so the next recording starts clean.
        _peerAudioActivatedAt = {};
        _peerVideoActivatedAt = {};
        _prevPeerMicMuted = {};
        _prevSceneKey = null;
        _lastFrameMs = 0;
        // Disconnect master_gain from mixDest to avoid a dangling connection
        // inside the SharedAudioContext.
        if (_masterGainRef && _mixDest) {
          try {
            _masterGainRef.disconnect(_mixDest);
          } catch (_) {}
        }
        _masterGainRef = null;
        // Only close the AudioContext if we created it; never close the
        // SharedAudioContext (that would kill all meeting audio).
        if (_audioMixerCtx && _audioMixerCtx !== window.__vcSharedAudioCtx) {
          _audioMixerCtx.close().catch(function () {});
        }
        _audioMixerCtx = null;
        // Release the mic source node reference.
        if (_micSource) {
          try {
            _micSource.disconnect();
          } catch (_) {}
          _micSource = null;
        }
        _mixDest = null;
        _prevMicOn = null;
        // Release the screen-share audio source.
        if (_ssAudioSource) {
          try {
            _ssAudioSource.disconnect();
          } catch (_) {}
          _ssAudioSource = null;
        }
        _prevSsObject = null;
        // Stop the local mic stream acquired for recording so the browser
        // removes the "microphone in use" indicator.
        if (micStream) {
          micStream.getTracks().forEach(function (t) {
            t.stop();
          });
          micStream = null;
        }

        // Remove visibility listener.
        if (_visibHandler) {
          document.removeEventListener("visibilitychange", _visibHandler);
          _visibHandler = null;
        }

        /**
         * Persist the final (possibly encrypted) blob using the file handle
         * chosen at recording start, or auto-download if unavailable.
         * Also transitions the state to "saved".
         */
        function persistBlob(blobToSave, saveExt) {
          if (capturedHandle) {
            capturedHandle
              .createWritable()
              .then(function (writable) {
                return writable.write(blobToSave).then(function () {
                  return writable.close();
                });
              })
              .catch(function (e) {
                console.error(
                  "[recording] FileSystemWritableFileStream failed, falling back to download:",
                  e,
                );
                triggerDownload(blobToSave, saveExt);
              });
          } else {
            triggerDownload(blobToSave, saveExt);
          }
          setState("saved");
          setTimeout(function () {
            if (_state === "saved") setState("idle");
          }, 3000);
        }

        // ── Save ──────────────────────────────────────────────────────────
        if (capturedWriter) {
          // Streaming path: all chunks were written to disk incrementally.
          // Wait for the last in-flight write to complete, then close the
          // stream to finalise the file.  No blob assembly, no 3× memory
          // spike, and the file is usable even if the tab crashes mid-recording.
          capturedWriteChain
            .then(function () {
              return capturedWriter.close();
            })
            .then(function () {
              setState("saved");
              setTimeout(function () {
                if (_state === "saved") setState("idle");
              }, 3000);
            })
            .catch(function (err) {
              console.error(
                "[recording] Failed to finalise streamed file:",
                err,
              );
              setState("idle");
            });
        } else if (capturedKey && window.crypto && window.crypto.subtle) {
          // In-memory E2EE fallback: decrypt all accumulated chunks and save.
          Promise.all(capturedChunks)
            .then(function (encChunks) {
              return Promise.all(
                encChunks.map(function (enc) {
                  return window.crypto.subtle.decrypt(
                    { name: "AES-GCM", iv: enc.iv },
                    capturedKey,
                    enc.ct,
                  );
                }),
              );
            })
            .then(function (plainParts) {
              persistBlob(new Blob(plainParts, { type: _mimeType }), ext);
            })
            .catch(function (err) {
              console.error("[recording] E2EE chunk decryption failed:", err);
              setState("idle");
            });
        } else {
          // In-memory raw fallback (no file handle, no E2EE).
          persistBlob(new Blob(capturedChunks, { type: _mimeType }), ext);
        }
      };

      // Start with timeslice so each ondataavailable chunk is streamed to disk.
      try {
        _recorder.start(CHUNK_MS);
      } catch (e) {
        console.error("[recording] recorder.start() failed:", e);
        if (_writer) {
          _writer.abort().catch(function () {});
          _writer = null;
        }
        _writeChain = Promise.resolve();
        setState("idle");
      }
    },

    /** Stop an in-progress recording and trigger the file download. */
    stop: function () {
      if (_state !== "recording") {
        console.warn("[recording] stop() called while state=" + _state);
        return;
      }
      setState("stopping");
      stopRafLoop();
      if (_recorder && _recorder.state !== "inactive") {
        try {
          // Explicitly request any data buffered since the last timeslice so
          // the final partial chunk is delivered to ondataavailable before stop()
          // fires onstop.  Some browsers do not reliably fire a final
          // ondataavailable on stop() alone when a timeslice is used.
          _recorder.requestData();
          _recorder.stop();
        } catch (e) {
          console.error("[recording] recorder.stop() failed:", e);
          setState("idle");
        }
      }
    },

    /** Return the current recording state string. */
    getState: function () {
      return _state;
    },

    /**
     * Test/diagnostic accessor: resolve the display name the recording
     * compositor extracts for every peer tile currently in `#grid-container`,
     * using the SAME production `getTileName()` that drawFrame() feeds into the
     * name chips. Returns `[{ id, name }]` where `id` is the peer session id
     * parsed from the tile div (`peer-video-{sid}-div`), or `null` for a tile
     * whose id does not match.
     *
     * This exists so an E2E/regression test can assert that remote peer names
     * resolve to non-empty strings through the real DOM-scraping path — the
     * exact path that regressed when the name text was wrapped in
     * `.floating-name-text`. It performs no drawing and has no side effects.
     */
    readTileNames: function () {
      var grid = document.getElementById("grid-container");
      if (!grid) return [];
      var out = [];
      var tiles = grid.querySelectorAll("[data-tile-root]");
      for (var i = 0; i < tiles.length; i++) {
        var t = tiles[i];
        if (t.classList.contains("split-screen-tile")) continue;
        var m = t.id ? t.id.match(/^peer-video-(\d+)-div$/) : null;
        out.push({ id: m ? m[1] : null, name: getTileName(t) });
      }
      return out;
    },
  };
})();
