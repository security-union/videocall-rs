/*
 * MediaStreamTrackProcessor for browsers that lack it on the main thread.
 *
 * camera_encoder.rs and screen_encoder.rs read VideoFrames from
 * `new MediaStreamTrackProcessor({track}).readable`. Chrome has that natively
 * and never runs this file's polyfill. Elsewhere the readable comes from one
 * of two capture paths, picked per track:
 *
 * 1. Worker (Safari). Safari exposes MediaStreamTrackProcessor only in
 *    workers. A clone of the track is transferred to a dedicated worker
 *    (this same file), which reads it with the native processor and sends
 *    back one frame per pull, so a busy main thread drops frames at the
 *    capture source instead of queueing them. On iPhone the worker frames are
 *    raw sensor buffers, so the worker turns them upright (see
 *    `uprightRotation`). Nothing is drawn on the main thread.
 * 2. Element (Firefox, which has no MediaStreamTrackProcessor anywhere). The
 *    track plays in a <video>, and each pull wraps the next video frame in a
 *    VideoFrame (see `startElementCapture` for when it is drawn to a canvas).
 *
 * Both paths are pull-based: a frame is produced only when the encoder reads
 * one, so there is nothing to pause or resume.
 *
 * The file is one classic script on purpose: the main thread loads it from
 * index.html, the worker is started from the same URL, and Node loads it for
 * the tests in dioxus-ui/tests/js/ (only the pure helpers and the stream
 * source are exported).
 */
(function () {
  "use strict";

  /**
   * Degrees clockwise that turn an iPhone worker MediaStreamTrackProcessor
   * frame upright.
   *
   * Safari 27.2 worker frames are the sensor buffer (1280x720 even in
   * portrait) and carry no VideoFrame.rotation or flip (measured: absent),
   * so the rotation is derived from the screen angle and the camera facing.
   * Measured on an iPhone 18 Pro (iOS 27.2.1, Safari 27.2), all six positions:
   *
   *   back  = (90 - angle) mod 360
   *   front = (90 + angle) mod 360
   *
   *   screen angle | front ("user") | back
   *   0 (portrait) | 90             | 90
   *   90           | 180            | 0
   *   270          | 0              | 180
   *
   * Known limits: with rotation lock on, the screen angle stays at the locked
   * orientation, so video from a phone held sideways goes out sideways (as
   * Safari's own rendering does). Frames captured during the iOS rotation
   * animation use the previous angle.
   *
   * `screenAngle` is screen.orientation.angle, or window.orientation (which
   * reports 270 as -90).
   */
  function uprightRotation(screenAngle, facingMode) {
    const angle = normalizeDegrees(Math.round((screenAngle || 0) / 90) * 90);
    return normalizeDegrees(facingMode === "user" ? 90 + angle : 90 - angle);
  }

  function normalizeDegrees(degrees) {
    return ((degrees % 360) + 360) % 360;
  }

  /**
   * The canvas size and setTransform matrix [a, b, c, d, e, f] that draw a
   * `width` x `height` frame turned `rotation` degrees clockwise (0, 90, 180
   * or 270) with drawImage(frame, 0, 0, width, height).
   */
  function rotationTransform(rotation, width, height) {
    switch (rotation) {
      case 90:
        return { matrix: [0, 1, -1, 0, height, 0], width: height, height: width };
      case 180:
        return { matrix: [-1, 0, 0, -1, width, height], width, height };
      case 270:
        return { matrix: [0, -1, 1, 0, 0, width], width: height, height: width };
      default:
        return { matrix: [1, 0, 0, 1, 0, 0], width, height };
    }
  }

  /**
   * Maps source frame timestamps (microseconds) onto the clock `nowUs`
   * reads. The first frame is anchored at nowUs(); later frames keep the
   * source spacing. Output is strictly increasing: if the source jumps
   * backwards, the mapping re-anchors just after the last output and keeps
   * the spacing from there.
   */
  function createTimestampRebaser(nowUs) {
    let offset = null;
    let last = -Infinity;
    return (sourceUs) => {
      if (offset === null) offset = nowUs() - sourceUs;
      let rebased = sourceUs + offset;
      if (rebased <= last) {
        offset = last + 1 - sourceUs;
        rebased = last + 1;
      }
      last = rebased;
      return rebased;
    };
  }

  // ---------------------------------------------------------------------
  // Worker side
  // ---------------------------------------------------------------------

  function runWorker() {
    let track = null;
    let reader = null;
    let rotation = 0;
    let rebase = null;
    let canvas = null;
    let context = null;

    // Returns an upright copy of `frame` stamped `timestamp`, and closes
    // `frame`.
    const upright = (frame, timestamp) => {
      if (rotation === 0) {
        // Same pixels, main-thread timestamp. No copy is made.
        const restamped = new VideoFrame(frame, { timestamp });
        frame.close();
        return restamped;
      }
      const transform = rotationTransform(rotation, frame.displayWidth, frame.displayHeight);
      if (!canvas) {
        canvas = new OffscreenCanvas(transform.width, transform.height);
        context = canvas.getContext("2d");
      } else if (canvas.width !== transform.width || canvas.height !== transform.height) {
        canvas.width = transform.width;
        canvas.height = transform.height;
      }
      context.setTransform(...transform.matrix);
      context.drawImage(frame, 0, 0, frame.displayWidth, frame.displayHeight);
      const rotated = new VideoFrame(canvas, { timestamp });
      frame.close();
      return rotated;
    };

    const stop = () => {
      reader?.cancel().catch(() => {});
      reader = null;
      track?.stop();
      track = null;
    };

    const sendFrame = async () => {
      try {
        const { value, done } = reader ? await reader.read() : { done: true };
        if (done) {
          self.postMessage({ type: "end" });
          return;
        }
        const frame = upright(value, rebase(value.timestamp));
        self.postMessage({ type: "frame", frame }, [frame]);
      } catch (error) {
        stop();
        self.postMessage({ type: "end", message: String(error) });
      }
    };

    self.onmessage = ({ data }) => {
      switch (data.type) {
        case "probe":
          self.postMessage({
            type: "probe",
            supported: typeof self.MediaStreamTrackProcessor === "function",
          });
          break;
        case "start":
          try {
            track = data.track;
            rotation = data.rotation;
            // The worker clock has its own origin; this maps frame
            // timestamps onto the main thread's performance.now().
            const mainTimeOrigin = data.timeOrigin;
            rebase = createTimestampRebaser(
              () => (performance.timeOrigin - mainTimeOrigin + performance.now()) * 1000,
            );
            reader = new self.MediaStreamTrackProcessor({ track }).readable.getReader();
            self.postMessage({ type: "started" });
          } catch (error) {
            stop();
            self.postMessage({ type: "error", message: String(error) });
          }
          break;
        case "rotation":
          rotation = data.rotation;
          break;
        case "pull":
          sendFrame();
          break;
        case "stop":
          stop();
          self.postMessage({ type: "stopped" });
          break;
      }
    };
  }

  // ---------------------------------------------------------------------
  // Main thread side
  // ---------------------------------------------------------------------

  const WORKER_URL = typeof document !== "undefined" && document.currentScript
    ? document.currentScript.src
    : null;
  const WORKER_REPLY_TIMEOUT_MS = 3000;

  // Only the iPhone has been measured (see uprightRotation). An iPad or a
  // track without a facing mode gets its frames unrotated.
  function rotatesFrames(track) {
    return /iPhone/.test(navigator.userAgent) &&
      typeof track.getSettings().facingMode === "string";
  }

  function screenAngle() {
    if (screen.orientation && typeof screen.orientation.angle === "number") {
      return screen.orientation.angle;
    }
    return typeof window.orientation === "number" ? window.orientation : 0;
  }

  // Calls `listener` on every screen orientation change. Returns a function
  // that removes it.
  function onOrientationChange(listener) {
    if (screen.orientation && screen.orientation.addEventListener) {
      screen.orientation.addEventListener("change", listener);
      return () => screen.orientation.removeEventListener("change", listener);
    }
    window.addEventListener("orientationchange", listener);
    return () => window.removeEventListener("orientationchange", listener);
  }

  // Resolves with the next message from `worker` that passes `accept`, or
  // null on a worker error or after WORKER_REPLY_TIMEOUT_MS.
  function nextMessage(worker, accept) {
    return new Promise((resolve) => {
      const done = (value) => {
        clearTimeout(timer);
        worker.removeEventListener("message", onMessage);
        worker.removeEventListener("error", onError);
        resolve(value);
      };
      const onMessage = ({ data }) => accept(data) && done(data);
      const onError = () => done(null);
      const timer = setTimeout(() => done(null), WORKER_REPLY_TIMEOUT_MS);
      worker.addEventListener("message", onMessage);
      worker.addEventListener("error", onError);
    });
  }

  // Whether a worker can run MediaStreamTrackProcessor. Asked once per page.
  let workerSupport = null;
  function probeWorker() {
    if (!workerSupport) {
      workerSupport = (async () => {
        if (!WORKER_URL || typeof Worker === "undefined") return false;
        let worker;
        try {
          worker = new Worker(WORKER_URL);
        } catch (error) {
          return false;
        }
        const reply = nextMessage(worker, (data) => data.type === "probe");
        worker.postMessage({ type: "probe" });
        const supported = Boolean((await reply)?.supported);
        worker.terminate();
        return supported;
      })();
    }
    return workerSupport;
  }

  // Path 1. Returns a capture ({pull, cancel}) or null when the worker path
  // is not available for this track.
  async function startWorkerCapture(track) {
    if (!(await probeWorker())) return null;
    const worker = new Worker(WORKER_URL);
    const rotates = rotatesFrames(track);
    const rotation = () =>
      rotates ? uprightRotation(screenAngle(), track.getSettings().facingMode) : 0;

    // The clone is what moves to the worker; the page keeps the original
    // (the self-view and the encoder's stop() use it).
    const clone = track.clone();
    const started = nextMessage(worker, (data) => data.type === "started" || data.type === "error");
    try {
      worker.postMessage(
        { type: "start", track: clone, rotation: rotation(), timeOrigin: performance.timeOrigin },
        [clone],
      );
    } catch (error) {
      // MediaStreamTrack is not transferable here.
      clone.stop();
      worker.terminate();
      return null;
    }
    const reply = await started;
    if (!reply || reply.type !== "started") {
      worker.terminate();
      return null;
    }

    const stopListening = onOrientationChange(() =>
      worker.postMessage({ type: "rotation", rotation: rotation() }),
    );
    let pending = null;
    worker.addEventListener("message", ({ data }) => {
      if (data.type !== "frame" && data.type !== "end") return;
      if (data.message) console.error("MSTP polyfill worker:", data.message);
      const resolve = pending;
      pending = null;
      if (resolve) {
        resolve(data.type === "frame" ? data.frame : null);
      } else if (data.frame) {
        data.frame.close();
      }
    });

    let cancelled = false;
    return {
      pull() {
        if (cancelled) return Promise.resolve(null);
        return new Promise((resolve) => {
          pending = resolve;
          worker.postMessage({ type: "pull" });
        });
      },
      cancel() {
        cancelled = true;
        stopListening();
        pending?.(null);
        pending = null;
        const stopped = nextMessage(worker, (data) => data.type === "stopped");
        worker.postMessage({ type: "stop" });
        stopped.then(() => worker.terminate());
      },
    };
  }

  // Path 2. Builds each pull's VideoFrame from a <video> playing the track.
  function startElementCapture(track) {
    const video = document.createElement("video");
    video.muted = true;
    video.playsInline = true;
    video.srcObject = new MediaStream([track]);
    const ready = Promise.all([
      video.play(),
      new Promise((resolve) => (video.onloadedmetadata = resolve)),
    ]);
    // new VideoFrame(video) wraps the decoded frame without a copy, so it is
    // the default. The one exception is an iPhone (rotatesFrames), which only
    // gets here when the worker path failed: there the frame built from the
    // element can be the sideways sensor buffer, while drawImage paints the
    // upright pixels the <video> shows. The canvas is also the fallback when
    // direct construction throws, and after that it is used for good.
    let direct = !rotatesFrames(track);
    let canvas = null;
    let context = null;
    let lastVideoTime = -1;
    let cancelled = false;

    // Resolves with the performance.now() time (milliseconds) of the next
    // new video frame. rVFC's `now` is used rather than metadata.mediaTime,
    // which a live MediaStream does not have to advance.
    const nextVideoFrame = () =>
      new Promise((resolve) => {
        if (video.requestVideoFrameCallback) {
          video.requestVideoFrameCallback((now) => resolve(now));
          return;
        }
        const tick = () => {
          if (video.currentTime !== lastVideoTime) {
            lastVideoTime = video.currentTime;
            resolve(performance.now());
          } else {
            requestAnimationFrame(tick);
          }
        };
        requestAnimationFrame(tick);
      });

    // Both clocks above are performance.now(); this only keeps the output
    // strictly increasing.
    const rebase = createTimestampRebaser(() => performance.now() * 1000);

    const drawnFrame = (width, height, timestamp) => {
      if (!canvas) {
        canvas = new OffscreenCanvas(width, height);
        context = canvas.getContext("2d");
      } else if (canvas.width !== width || canvas.height !== height) {
        canvas.width = width;
        canvas.height = height;
      }
      // Explicit size: a frame whose size changes a frame before or
      // after videoWidth/videoHeight is scaled, never cropped.
      context.drawImage(video, 0, 0, width, height);
      return new VideoFrame(canvas, { timestamp });
    };

    return {
      async pull() {
        await ready;
        while (!cancelled) {
          const nowMs = await nextVideoFrame();
          const width = video.videoWidth;
          const height = video.videoHeight;
          if (cancelled || !width || !height) continue;
          const timestamp = rebase(nowMs * 1000);
          if (direct) {
            try {
              return new VideoFrame(video, { timestamp });
            } catch (error) {
              console.warn("MSTP polyfill: VideoFrame(video) failed, drawing to a canvas:", error);
              direct = false;
            }
          }
          return drawnFrame(width, height, timestamp);
        }
        return null;
      },
      cancel() {
        cancelled = true;
        video.pause();
        video.srcObject = null;
      },
    };
  }

  // Picks the capture path for `track`. `isCancelled` stops it from starting
  // the element path for a stream that was cancelled during the worker probe.
  async function startCapture(track, isCancelled) {
    const worker = await startWorkerCapture(track);
    if (worker) {
      console.log("MSTP polyfill: worker capture");
      return worker;
    }
    if (isCancelled()) return null;
    console.log("MSTP polyfill: <video> capture");
    return startElementCapture(track);
  }

  /**
   * The ReadableStream underlying source over a capture ({pull, cancel}).
   * `start(isCancelled)` resolves with the capture, or null.
   *
   * A stream's cancel() does not wait for start() to settle, so the reader
   * can cancel while the capture is still starting (worker probe and
   * handshake). The capture that start() ends up with is then cancelled
   * right away; otherwise its worker and track clone would outlive the
   * stream and keep the camera on.
   */
  function videoSource(start) {
    let capture = null;
    let cancelled = false;
    return {
      async start() {
        const started = await start(() => cancelled);
        if (cancelled) {
          started?.cancel();
          return;
        }
        capture = started;
      },
      async pull(controller) {
        if (!capture) {
          controller.close();
          return;
        }
        const frame = await capture.pull();
        if (cancelled) {
          frame?.close();
        } else if (frame) {
          controller.enqueue(frame);
        } else {
          controller.close();
        }
      },
      cancel() {
        cancelled = true;
        capture?.cancel();
        capture = null;
      },
    };
  }

  function videoReadable(track) {
    return new ReadableStream(
      videoSource((isCancelled) => startCapture(track, isCancelled)),
      // Pull only when the encoder reads, one frame at a time.
      { highWaterMark: 0 },
    );
  }

  function installPolyfill() {
    if (self.MediaStreamTrackProcessor) return;
    self.MediaStreamTrackProcessor = class MediaStreamTrackProcessor {
      constructor({ track }) {
        if (track.kind !== "video") {
          throw new TypeError("This MediaStreamTrackProcessor polyfill handles video tracks only");
        }
        this.readable = videoReadable(track);
      }
    };
  }

  if (typeof module === "object" && module.exports) {
    module.exports = { uprightRotation, rotationTransform, createTimestampRebaser, videoSource };
  } else if (typeof WorkerGlobalScope !== "undefined" && self instanceof WorkerGlobalScope) {
    runWorker();
  } else {
    installPolyfill();
  }
})();
