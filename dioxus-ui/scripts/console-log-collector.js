// Console Log Collector — captures browser console output and uploads periodically.
// Reads window.__APP_CONFIG.consoleLogUploadEnabled; does nothing if falsy.
// See docs/2026-04-13-console-log-collection-proposal.md for full design.
(function () {
  "use strict";

  var config = (window.__APP_CONFIG || {});
  if (config.consoleLogUploadEnabled !== "true") {
    // Feature disabled — expose a no-op API so WASM calls do not throw.
    window.__consoleLogCollector = {
      setContext: function () {},
      flush: function () {},
    };
    return;
  }

  // ---------------------------------------------------------------------------
  // State
  // ---------------------------------------------------------------------------
  var BUFFER_CAP = 10000;
  var BUFFER_BYTE_BUDGET = 768 * 1024; // 768 KB — flush before hitting the 1 MB server limit
  var UPLOAD_INTERVAL_MS = 30000;

  var buffer = [];
  var bufferBytes = 0; // running byte count of buffer contents
  var meetingId = null;
  var userId = null;
  var displayName = null;
  var appVersion = null; // populated from __APP_CONFIG.imageTag (Helm-injected)
  var sessionTimestampMs = null; // set once per page load via setContext
  var preambleWritten = false;
  var uploadTimer = null;
  var uploadInFlight = false;
  var nextSeq = 0;

  var highEntropyPlatform = null;

  if (navigator.userAgentData && typeof navigator.userAgentData.getHighEntropyValues === "function") {
    try {
      navigator.userAgentData.getHighEntropyValues(["platform", "platformVersion"])
        .then(function (ua) {
          var pv = ua.platformVersion || "";
          var major = parseInt(pv.split(".")[0], 10);
          if (ua.platform === "Windows" && !isNaN(major)) {
            highEntropyPlatform = major >= 13 ? "Windows 11" : "Windows 10";
            highEntropyPlatform += " (platformVersion=" + pv + ")";
          } else if (ua.platform) {
            highEntropyPlatform = ua.platform + " " + pv;
          }
        })
        .catch(function () {});
    } catch (_) {}
  }

  // ---------------------------------------------------------------------------
  // PII / secret scrubbing (best-effort, pattern-based)
  // ---------------------------------------------------------------------------

  // JWTs (base64url-encoded header starting with eyJ)
  var JWT_RE = /eyJ[A-Za-z0-9_-]{10,}/g;
  // Bearer tokens (including non-JWT tokens)
  var BEARER_RE = /Bearer\s+[A-Za-z0-9_.~+/=-]{8,}/gi;
  // Sensitive URL query parameters
  var SENSITIVE_PARAM_RE = /([?&])(token|jwt|authorization|password|secret|access_token|refresh_token|id_token|code|api_key|cookie|session|state|key|sig|signature)=[^&]*/gi;
  // Email addresses (OAuth sub, display names, etc.)
  var EMAIL_RE = /[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}/g;
  // IPv4 addresses (SDP/ICE candidates, server URLs)
  var IPV4_RE = /\b(?:(?:25[0-5]|2[0-4]\d|[01]?\d?\d)\.){3}(?:25[0-5]|2[0-4]\d|[01]?\d?\d)\b/g;
  // IPv6 addresses (condensed and full forms)
  var IPV6_RE = /\b(?:[0-9a-fA-F]{1,4}:){2,7}[0-9a-fA-F]{1,4}\b/g;
  // Cookie header values
  var COOKIE_RE = /Cookie:\s*[^\r\n]*/gi;
  // Crypto key material (hex or base64 blobs after key-related labels)
  var CRYPTO_KEY_RE = /\b(aes_key|rsa_pub_key|private_key|iv|encryption_key|secret_key)\s*[:=]\s*["']?[A-Za-z0-9+/=_-]{8,}["']?/gi;

  // ---------------------------------------------------------------------------
  // Helpers
  // ---------------------------------------------------------------------------

  function scrub(msg) {
    if (typeof msg !== "string") return msg;
    return msg
      .replace(JWT_RE, "[REDACTED_JWT]")
      .replace(BEARER_RE, "Bearer [REDACTED]")
      .replace(SENSITIVE_PARAM_RE, "$1$2=[REDACTED]")
      .replace(CRYPTO_KEY_RE, "$1=[REDACTED]")
      .replace(COOKIE_RE, "Cookie: [REDACTED]")
      .replace(EMAIL_RE, "[REDACTED_EMAIL]")
      .replace(IPV4_RE, "[REDACTED_IP]")
      .replace(IPV6_RE, "[REDACTED_IP]");
  }

  function stringify(args) {
    var parts = [];
    for (var i = 0; i < args.length; i++) {
      var a = args[i];
      if (typeof a === "string") {
        parts.push(a);
      } else {
        try {
          parts.push(JSON.stringify(a));
        } catch (_) {
          parts.push(String(a));
        }
      }
    }
    return scrub(parts.join(" "));
  }

  function pushEntry(level, args) {
    var entry = JSON.stringify({
      seq: nextSeq++,
      ts: new Date().toISOString(),
      level: level,
      msg: stringify(args),
    });
    buffer.push(entry);
    bufferBytes += entry.length + 1; // +1 for the newline in the upload payload

    // Evict oldest entries if over line cap
    if (buffer.length > BUFFER_CAP) {
      var removed = buffer.splice(0, buffer.length - BUFFER_CAP);
      var removedCount = removed.length;
      for (var i = 0; i < removed.length; i++) {
        bufferBytes -= removed[i].length + 1;
      }
      var warnEntry = JSON.stringify({
        seq: nextSeq++,
        ts: new Date().toISOString(),
        level: "warn",
        msg: "CONSOLE_LOG_COLLECTOR: evicted " + removedCount + " oldest entries (buffer cap=" + BUFFER_CAP + ")"
      });
      buffer.push(warnEntry);
      bufferBytes += warnEntry.length + 1;
    }

    // Auto-flush when byte budget is exceeded to prevent 413s from the
    // server's 1 MB DefaultBodyLimit, especially with Debug-level logging.
    if (bufferBytes >= BUFFER_BYTE_BUDGET && meetingId && userId && sessionTimestampMs) {
      doUpload(false);
    }
  }

  function writePreamble() {
    if (preambleWritten) return;
    preambleWritten = true;

    var nav = navigator || {};
    var perf = performance || {};
    var mem = perf.memory || {};
    var scr = screen || {};

    var heapUsed = mem.usedJSHeapSize
      ? Math.round(mem.usedJSHeapSize / (1024 * 1024)) + "MB"
      : "N/A";
    var heapTotal = mem.jsHeapSizeLimit
      ? Math.round(mem.jsHeapSizeLimit / (1024 * 1024)) + "MB"
      : "N/A";
    var deviceMemory;
    if (nav.deviceMemory) {
      var capped = (nav.deviceMemory === 8) ? " (browser-capped)" : "";
      deviceMemory = nav.deviceMemory + " GB" + capped;
    } else {
      deviceMemory = "N/A (unsupported)";
    }
    var platform;
    if (highEntropyPlatform) {
      platform = highEntropyPlatform;
    } else if (nav.userAgentData && nav.userAgentData.platform) {
      platform = nav.userAgentData.platform;
    } else {
      platform = nav.platform || "N/A";
    }
    var languages = nav.languages
      ? nav.languages.join(",")
      : (nav.language || "N/A");
    var dpr = window.devicePixelRatio || 1;

    var msg = "appVersion=" + (appVersion || "unknown")
      + "; displayName=" + (displayName || "unknown")
      + "; userAgent=" + (nav.userAgent || "N/A")
      + "; cores=" + (nav.hardwareConcurrency || "N/A")
      + "; memory=" + deviceMemory
      + "; heap=" + heapUsed + "/" + heapTotal
      + "; screen=" + scr.width + "x" + scr.height + "@" + dpr + "x"
      + "; platform=" + platform
      + "; languages=" + languages;

    var entry = JSON.stringify({
      seq: nextSeq++,
      ts: new Date().toISOString(),
      level: "preamble",
      msg: msg,
    });
    buffer.push(entry);
    bufferBytes += entry.length + 1;
  }

  // ---------------------------------------------------------------------------
  // Upload
  // ---------------------------------------------------------------------------

  function buildUrl() {
    if (!meetingId) return null;
    // Derive the base URL from the config; the meeting-api may live at a
    // different origin than the UI.
    var base = config.meetingApiBaseUrl || config.apiBaseUrl || "";
    return base + "/api/v1/meetings/" + encodeURIComponent(meetingId) + "/console-logs";
  }

  function doUpload(useKeepalive) {
    if (buffer.length === 0) return;
    if (uploadInFlight && !useKeepalive) return;
    var url = buildUrl();
    if (!url || !userId || !sessionTimestampMs) return;

    // Drain the buffer into a payload. On failure the entries will be
    // re-prepended so they are retried on the next tick.
    var payload = buffer.join("\n") + "\n";
    var drained = buffer.splice(0, buffer.length);
    var drainedBytes = bufferBytes;
    bufferBytes = 0;

    var opts = {
      method: "POST",
      credentials: "include",
      headers: {
        "Content-Type": "text/plain",
        "X-User-Id": userId,
        "X-Session-Timestamp": String(sessionTimestampMs),
      },
      body: payload,
    };
    if (useKeepalive) {
      opts.keepalive = true;
    }

    uploadInFlight = true;
    fetch(url, opts)
      .then(function (resp) {
        uploadInFlight = false;
        if (resp.status >= 500) {
          // Server error — re-queue for retry
          requeue(drained, drainedBytes);
        }
        // 4xx errors: drop the entries (client bug or disabled endpoint)
      })
      .catch(function () {
        uploadInFlight = false;
        // Network failure — re-queue for retry
        requeue(drained, drainedBytes);
      });
  }

  function requeue(entries, entryBytes) {
    // Prepend the failed entries back. If combined length exceeds the cap,
    // the oldest entries (from the failed batch) are dropped.
    buffer = entries.concat(buffer);
    bufferBytes += entryBytes;
    if (buffer.length > BUFFER_CAP) {
      var removed = buffer.splice(0, buffer.length - BUFFER_CAP);
      var removedCount = removed.length;
      for (var i = 0; i < removed.length; i++) {
        bufferBytes -= removed[i].length + 1;
      }
      var warnEntry = JSON.stringify({
        seq: nextSeq++,
        ts: new Date().toISOString(),
        level: "warn",
        msg: "CONSOLE_LOG_COLLECTOR: evicted " + removedCount + " oldest entries (buffer cap=" + BUFFER_CAP + ")"
      });
      buffer.push(warnEntry);
      bufferBytes += warnEntry.length + 1;
    }
  }

  function startTimer() {
    if (uploadTimer) return;
    uploadTimer = setInterval(doUpload, UPLOAD_INTERVAL_MS);
  }

  function flushNow() {
    doUpload(true);
  }

  // ---------------------------------------------------------------------------
  // Console interception
  // ---------------------------------------------------------------------------

  var LEVELS = ["log", "warn", "error", "info", "debug"];
  var originals = {};

  LEVELS.forEach(function (level) {
    originals[level] = console[level];
    console[level] = function () {
      // Call original first — DevTools output is unchanged
      originals[level].apply(console, arguments);
      // Buffer the entry
      pushEntry(level, arguments);
    };
  });

  // ---------------------------------------------------------------------------
  // Page close — fire-and-forget flush via keepalive fetch or sendBeacon.
  // We listen to beforeunload, pagehide, and visibilitychange because
  // mobile browsers (iOS Safari, Android Chrome) do not reliably fire
  // beforeunload on swipe-away or backgrounding.
  // ---------------------------------------------------------------------------

  var pageCloseFlushed = false;

  function onPageClose() {
    if (pageCloseFlushed) return;
    if (buffer.length === 0) return;
    var url = buildUrl();
    if (!url || !userId || !sessionTimestampMs) return;
    pageCloseFlushed = true;

    var payload = buffer.join("\n") + "\n";

    // Prefer fetch with keepalive — supports custom headers and survives
    // page navigation in all modern browsers.
    try {
      fetch(url, {
        method: "POST",
        credentials: "include",
        keepalive: true,
        headers: {
          "Content-Type": "text/plain",
          "X-User-Id": userId,
          "X-Session-Timestamp": String(sessionTimestampMs),
        },
        body: payload,
      });
    } catch (_) {
      // fetch+keepalive not available — fall back to sendBeacon.
      // Embed user_id and session_ts in the body as a metadata header line
      // instead of query params to avoid leaking identifiers to access logs.
      try {
        var meta = JSON.stringify({
          ts: new Date().toISOString(),
          level: "meta",
          msg: "user_id=" + userId + "; session_ts=" + sessionTimestampMs,
        });
        var beaconPayload = meta + "\n" + payload;
        var blob = new Blob([beaconPayload], { type: "text/plain" });
        navigator.sendBeacon(url, blob);
      } catch (__) {
        // Best-effort — nothing more we can do on tab close.
      }
    }
  }

  window.addEventListener("beforeunload", onPageClose);
  window.addEventListener("pagehide", onPageClose);
  document.addEventListener("visibilitychange", function () {
    if (document.visibilityState === "hidden") {
      onPageClose();
    } else if (document.visibilityState === "visible") {
      // Reset so a subsequent hide/unload can flush logs accumulated while
      // the tab was re-activated.
      pageCloseFlushed = false;
      doUpload(false);
    }
  });

  // ---------------------------------------------------------------------------
  // Public API
  // ---------------------------------------------------------------------------

  window.__consoleLogCollector = {
    /**
     * Called by WASM when joining a meeting. Idempotent on soft reconnect:
     * sessionTimestampMs is only set once per page load.
     */
    setContext: function (mid, uid, dname) {
      meetingId = mid;
      userId = uid;
      displayName = dname;
      appVersion = config.imageTag || null;
      if (!sessionTimestampMs) {
        sessionTimestampMs = Date.now();
      }
      writePreamble();
      startTimer();
    },

    /** Called by WASM on hangup — immediate upload of buffered entries. */
    flush: function () {
      flushNow();
    },
  };
})();
