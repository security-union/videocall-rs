// Console Log Collector — captures browser console output and uploads periodically.
// Reads window.__APP_CONFIG.consoleLogUploadEnabled; does nothing if falsy.
// See docs/2026-04-13-console-log-collection-proposal.md for full design.
//
// IMPORTANT: The client metadata section below (GPU, network, battery, UA) is
// independent of console-log upload because the WASM health reporter reads
// window.__videocall_client_metadata directly. Operators can disable it with
// hardwareMetricsEnabled=false.
(function () {
  "use strict";

  var config = (window.__APP_CONFIG || {});
  // Health telemetry is independent of console-log upload, but operators can
  // disable hardware/network/battery collection explicitly. Default-on keeps
  // existing production diagnostics and the vanilla local stack useful.
  var hardwareMetricsEnabled = config.hardwareMetricsEnabled !== "false"
    && config.hardwareMetricsEnabled !== false
    && config.hardwareMetricsEnabled !== 0;

  // ===========================================================================
  // CLIENT METADATA (independent of log upload — feeds health_reporter.rs)
  // ===========================================================================

  var highEntropyPlatform = null;
  var highEntropyArchitecture = null;
  var highEntropyModel = null;
  var highEntropyMobile = null;

  if (hardwareMetricsEnabled && navigator.userAgentData && typeof navigator.userAgentData.getHighEntropyValues === "function") {
    try {
      navigator.userAgentData.getHighEntropyValues(["platform", "platformVersion", "architecture", "model"])
        .then(function (ua) {
          var pv = ua.platformVersion || "";
          var major = parseInt(pv.split(".")[0], 10);
          if (ua.platform === "Windows" && !isNaN(major)) {
            highEntropyPlatform = major >= 13 ? "Windows 11" : "Windows 10";
            highEntropyPlatform += " (platformVersion=" + pv + ")";
          } else if (ua.platform) {
            highEntropyPlatform = ua.platform + " " + pv;
          }
          if (ua.architecture) {
            highEntropyArchitecture = ua.architecture;
          }
          highEntropyModel = ua.model || "";
          highEntropyMobile = (ua.mobile === true);
          updateClientMetadata();
        })
        .catch(function () {});
    } catch (_) {}
  }

  // TELEM-1: GPU renderer detection
  var gpuRenderer = null;
  if (hardwareMetricsEnabled) {
    try {
      var _c = document.createElement("canvas");
      var _gl = _c.getContext("webgl") || _c.getContext("experimental-webgl");
      if (_gl) {
        var _ext = _gl.getExtension("WEBGL_debug_renderer_info");
        if (_ext) {
          gpuRenderer = _gl.getParameter(_ext.UNMASKED_RENDERER_WEBGL);
        }
      }
    } catch (_) {}
  }

  // TELEM-4: Network information (live — updated on change events)
  var networkInfo = null;
  function readNetworkInfo() {
    if (hardwareMetricsEnabled && navigator.connection) {
      networkInfo = {
        effectiveType: navigator.connection.effectiveType || null,
        downlink: navigator.connection.downlink || null,
        rtt: navigator.connection.rtt || null,
        saveData: navigator.connection.saveData || false,
        type: navigator.connection.type || null,
        downlinkMax: navigator.connection.downlinkMax || null,
      };
    }
  }
  readNetworkInfo();
  if (hardwareMetricsEnabled && navigator.connection && navigator.connection.addEventListener) {
    navigator.connection.addEventListener("change", function () {
      readNetworkInfo();
      updateClientMetadata();
    });
  }

  // TELEM-5: Battery status (async, live — updated on change events)
  var batteryInfo = null;
  if (hardwareMetricsEnabled && navigator.getBattery) {
    try {
      navigator.getBattery().then(function (battery) {
        batteryInfo = { charging: battery.charging, level: battery.level };
        updateClientMetadata();
        if (battery.addEventListener) {
          battery.addEventListener("chargingchange", function () {
            batteryInfo.charging = battery.charging;
            updateClientMetadata();
          });
          battery.addEventListener("levelchange", function () {
            batteryInfo.level = battery.level;
            updateClientMetadata();
          });
        }
      }).catch(function () {});
    } catch (_) {}
  }

  // Shared helper: (re)write window.__videocall_client_metadata from current state.
  // Called from async callbacks (getHighEntropyValues, getBattery, connection change)
  // and from writePreamble().
  function updateClientMetadata() {
    if (!hardwareMetricsEnabled) {
      return;
    }
    var nav = navigator || {};
    var osStr = highEntropyPlatform
      || (nav.userAgentData && nav.userAgentData.platform)
      || nav.platform
      || "";
    var uaStr = (typeof nav.userAgent === "string") ? nav.userAgent : "";
    var maxTouch = (typeof nav.maxTouchPoints === "number") ? nav.maxTouchPoints : 0;
    var TABLET_RE = /ipad|tablet|sm-t|nexus 7|nexus 9/i;
    var isIpadDesktopUA = (maxTouch > 1 && /Macintosh/.test(uaStr));
    var deviceType = "";
    if (highEntropyMobile === true) {
      deviceType = "mobile";
      if (TABLET_RE.test(highEntropyModel || "") || TABLET_RE.test(uaStr) || isIpadDesktopUA) {
        deviceType = "tablet";
      }
    } else if (highEntropyMobile === false) {
      deviceType = "desktop";
    } else {
      if (isIpadDesktopUA || /iPad/.test(uaStr)) {
        deviceType = "tablet";
      } else if (/Mobi|Android|iPhone/i.test(uaStr)) {
        deviceType = "mobile";
      } else {
        deviceType = "desktop";
      }
    }
    var deviceMemoryGb = (typeof nav.deviceMemory === "number") ? nav.deviceMemory : undefined;
    window.__videocall_client_metadata = {
      architecture: highEntropyArchitecture || "",
      gpu: gpuRenderer || "",
      network_effective_type: networkInfo ? (networkInfo.effectiveType || "") : "",
      network_downlink: networkInfo ? (networkInfo.downlink || 0) : 0,
      network_rtt: networkInfo ? (networkInfo.rtt || 0) : 0,
      network_type: networkInfo ? (networkInfo.type || "") : "",
      network_downlink_max: networkInfo ? (networkInfo.downlinkMax || 0) : 0,
      battery_charging: batteryInfo ? batteryInfo.charging : null,
      battery_level: batteryInfo ? batteryInfo.level : null,
      os: osStr,
      device_type: deviceType,
      device_memory_gb: deviceMemoryGb
    };
  }

  // Write initial metadata synchronously (before any async callbacks resolve)
  if (hardwareMetricsEnabled) {
    updateClientMetadata();
  }

  // ===========================================================================
  // CONSOLE LOG CAPTURE (gated on consoleLogUploadEnabled)
  // ===========================================================================

  if (config.consoleLogUploadEnabled !== "true") {
    window.__consoleLogCollector = {
      setContext: function () {},
      setAuthToken: function () {},
      flush: function () {},
    };
    return;
  }

  // ---------------------------------------------------------------------------
  // State
  // ---------------------------------------------------------------------------
  var BUFFER_CAP = 10000;
  var UPLOAD_TRIGGER_BYTES = 48 * 1024;
  // Chromium rejects a keepalive request once the document's in-flight
  // keepalive bodies would exceed 64 KiB (UTF-8 bytes, headers excluded).
  var KEEPALIVE_QUOTA_BYTES = 64 * 1024;
  var META_RESERVE_BYTES = 512;
  var UPLOAD_INTERVAL_MS = 30000;

  var buffer = [];
  var bufferBytes = 0; // UTF-8 size of the upload payload the buffer would produce
  var meetingId = null;
  var userId = null;
  var displayName = null;
  // room_token for this meeting, set via setAuthToken() on join + refresh.
  // Attached to uploads as `Authorization: Bearer <token>`. Treated as a
  // credential: never logged, never placed in the upload body.
  var authToken = null;
  var appVersion = null; // populated from __APP_CONFIG.imageTag (Helm-injected)
  var sessionTimestampMs = null; // set once per page load via setContext
  var preambleWritten = false;
  var uploadTimer = null;
  var uploadInFlight = false;
  var nextSeq = 0;
  var nextChunkSeq = 1;
  var keepaliveBytesInFlight = 0;

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
  var CRYPTO_KEY_RE = /\b(aes_key|rsa_pub_key|private_key|iv|encryption_key|secret_key|lease_?secret)\\?["']?\s*[:=]\s*\\?["']?[A-Za-z0-9+/=_-]{8,}\\?["']?/gi;

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

  // Exact for JSON.stringify output, which never contains a lone surrogate.
  function utf8Length(str) {
    var n = str.length;
    for (var i = 0; i < str.length; i++) {
      var c = str.charCodeAt(i);
      if (c >= 0x80) n += (c >= 0x800 && (c < 0xd800 || c > 0xdfff)) ? 2 : 1;
    }
    return n;
  }

  function pushEntry(level, args) {
    var entry = JSON.stringify({
      seq: nextSeq++,
      ts: new Date().toISOString(),
      level: level,
      msg: stringify(args),
    });
    buffer.push(entry);
    bufferBytes += utf8Length(entry) + 1;

    // Evict oldest entries if over line cap
    if (buffer.length > BUFFER_CAP) {
      var removed = buffer.splice(0, buffer.length - BUFFER_CAP);
      var removedCount = removed.length;
      for (var i = 0; i < removed.length; i++) {
        bufferBytes -= utf8Length(removed[i]) + 1;
      }
      var warnEntry = JSON.stringify({
        seq: nextSeq++,
        ts: new Date().toISOString(),
        level: "warn",
        msg: "CONSOLE_LOG_COLLECTOR: evicted " + removedCount + " oldest entries (buffer cap=" + BUFFER_CAP + ")"
      });
      buffer.push(warnEntry);
      bufferBytes += utf8Length(warnEntry) + 1;
    }

    if (bufferBytes >= UPLOAD_TRIGGER_BYTES && meetingId && userId && sessionTimestampMs) {
      doUpload();
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

    // Phase 8a / TELEM-2: capability_score is set by Rust right before
    // calling setContext. It's the iteration count of a 100 ms f64
    // multiply-add microbenchmark — a stable cross-machine signal we use
    // to spot under-powered devices in production logs (e.g. an Intel
    // 2014 MBP scoring ~5,000 vs an M2 scoring > 50,000). Falls back to
    // "N/A" if the score wasn't computed (older WASM build).
    var capabilityScore = window.__videocall_capability_score;
    if (typeof capabilityScore !== "number" || !isFinite(capabilityScore)) {
      capabilityScore = "N/A";
    }

    // TELEM-1: architecture
    var architecture = highEntropyArchitecture || "N/A";

    // TELEM-4: network summary
    var networkSummary = "N/A";
    if (networkInfo) {
      networkSummary = (networkInfo.effectiveType || "?")
        + "/" + (networkInfo.downlink != null ? networkInfo.downlink + "Mbps" : "?")
        + "/rtt" + (networkInfo.rtt != null ? networkInfo.rtt + "ms" : "?");
      if (networkInfo.saveData) networkSummary += "/saveData";
    }

    // TELEM-5: battery summary
    var batterySummary = "N/A";
    if (batteryInfo) {
      batterySummary = (batteryInfo.charging ? "charging" : "discharging")
        + "@" + Math.round(batteryInfo.level * 100) + "%";
    }

    var msg = "appVersion=" + (appVersion || "unknown")
      + "; displayName=" + (displayName || "unknown")
      + "; userAgent=" + (nav.userAgent || "N/A")
      + "; cores=" + (nav.hardwareConcurrency || "N/A")
      + "; memory=" + deviceMemory
      + "; heap=" + heapUsed + "/" + heapTotal
      + "; screen=" + scr.width + "x" + scr.height + "@" + dpr + "x"
      + "; platform=" + platform
      + "; languages=" + languages
      + "; capability_score=" + capabilityScore
      + "; architecture=" + architecture
      + "; gpu=" + (gpuRenderer || "N/A")
      + "; network=" + networkSummary
      + "; battery=" + batterySummary;

    var entry = JSON.stringify({
      seq: nextSeq++,
      ts: new Date().toISOString(),
      level: "preamble",
      msg: msg,
    });
    buffer.push(entry);
    bufferBytes += utf8Length(entry) + 1;

    // Expose static metadata as window globals for the WASM health reporter (TELEM-7).
    updateClientMetadata();
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

  function uploadHeaders(chunkSeq) {
    var headers = {
      "Content-Type": "text/plain",
      "X-Session-Timestamp": String(sessionTimestampMs),
      "X-Chunk-Seq": String(chunkSeq),
    };
    // Authenticate via the room_token. The console-logs route validates this
    // room_token (a meeting-api-signed JWT) as the sole credential on BOTH
    // deployments — it does not read the session cookie — so the Bearer header
    // is required everywhere (Google OAuth and Ascend Okta PKCE alike).
    // `credentials:"include"` is harmless and left in place, but is not an auth
    // fallback: without a room_token the upload is rejected.
    if (authToken) {
      headers["Authorization"] = "Bearer " + authToken;
    }
    return headers;
  }

  function reportFailure(what, entryCount, err) {
    // Use originals.error (unwrapped) to avoid recursion — this bypasses the collector.
    var msg = "CONSOLE_LOG_COLLECTOR: " + what + ", " + entryCount + " entries dropped";
    if (err === undefined) {
      originals.error.call(console, msg);
    } else {
      originals.error.call(console, msg + ":", err);
    }
  }

  function seqOf(entry) {
    try {
      return JSON.parse(entry).seq;
    } catch (_) {
      return "?";
    }
  }

  function postChunk(url, lines, bytes, keepalive, done) {
    var payload = lines.join("\n") + "\n";
    // Increment BEFORE fetch so even failed requests advance the counter,
    // preventing filename collisions on retry.
    var chunkSeq = nextChunkSeq++;
    var what = keepalive ? "keepalive upload" : "upload";
    if (keepalive) keepaliveBytesInFlight += bytes;
    var settle = function () {
      if (keepalive) keepaliveBytesInFlight -= bytes;
      if (done) done();
    };
    try {
      fetch(url, {
        method: "POST",
        credentials: "include",
        keepalive: keepalive,
        headers: uploadHeaders(chunkSeq),
        body: payload,
      })
        .then(function (resp) {
          return resp.text().then(function () {
            // 409: chunk already exists (prior upload succeeded).
            if (!resp.ok && resp.status !== 409) {
              reportFailure(what + " failed (" + resp.status + ")", lines.length);
            }
          });
        })
        .catch(function (err) {
          reportFailure(what + " network error", lines.length, err);
        })
        .then(settle);
    } catch (_) {
      settle();
      if (keepalive) sendBeacon(url, payload, chunkSeq);
    }
  }

  function sendBeacon(url, payload, chunkSeq) {
    // Cannot carry the Bearer header, so the route 401s it. Identifiers go in
    // the body rather than the query to keep them out of access logs.
    try {
      var meta = JSON.stringify({
        ts: new Date().toISOString(),
        level: "meta",
        msg: "user_id=" + userId + "; session_ts=" + sessionTimestampMs + "; chunk_seq=" + chunkSeq,
      });
      var blob = new Blob([meta + "\n" + payload], { type: "text/plain" });
      navigator.sendBeacon(url, blob);
    } catch (__) {}
  }

  function freeKeepaliveBytes() {
    return KEEPALIVE_QUOTA_BYTES - keepaliveBytesInFlight - META_RESERVE_BYTES;
  }

  // Uses keepalive when the payload fits, so an upload in flight at unload is
  // not cancelled with the page.
  function doUpload() {
    if (buffer.length === 0) return;
    if (uploadInFlight) return;
    var url = buildUrl();
    if (!url || !userId || !sessionTimestampMs) return;

    var lines = buffer;
    var bytes = bufferBytes;
    buffer = [];
    bufferBytes = 0;
    uploadInFlight = true;
    postChunk(url, lines, bytes, bytes <= freeKeepaliveBytes(), function () {
      uploadInFlight = false;
    });
  }

  // Sends the oldest entries that fit the free keepalive quota and leaves the
  // rest buffered. On final unload it sends the newest instead and replaces
  // the older ones with one `meta` line.
  function sendKeepalive(isFinal) {
    if (buffer.length === 0) return;
    var url = buildUrl();
    if (!url || !userId || !sessionTimestampMs) return;

    var budget = freeKeepaliveBytes();
    var bytes = 0;
    var n = 0;
    while (n < buffer.length) {
      var entry = buffer[isFinal ? buffer.length - 1 - n : n];
      var size = utf8Length(entry) + 1;
      if (bytes + size > budget) break;
      bytes += size;
      n++;
    }
    var lines;
    if (!isFinal) {
      if (n === 0) return;
      lines = buffer.splice(0, n);
      bufferBytes -= bytes;
    } else {
      var dropped = buffer.length - n;
      lines = buffer.slice(dropped);
      if (dropped > 0) {
        var meta = JSON.stringify({
          seq: nextSeq++,
          ts: new Date().toISOString(),
          level: "meta",
          msg: "CONSOLE_LOG_COLLECTOR: keepalive quota exceeded at page close, dropped " + dropped
            + " oldest entries (seq " + seqOf(buffer[0]) + ".." + seqOf(buffer[dropped - 1])
            + ", " + (bufferBytes - bytes) + " bytes)",
        });
        lines.unshift(meta);
        bytes += utf8Length(meta) + 1;
      }
      buffer = [];
      bufferBytes = 0;
    }
    postChunk(url, lines, bytes, true);
  }

  function startTimer() {
    if (uploadTimer) return;
    uploadTimer = setInterval(doUpload, UPLOAD_INTERVAL_MS);
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

  function onUnload() {
    sendKeepalive(true);
  }
  window.addEventListener("beforeunload", onUnload);
  window.addEventListener("pagehide", onUnload);
  document.addEventListener("visibilitychange", function () {
    if (document.visibilityState === "hidden") {
      sendKeepalive(false);
    } else if (document.visibilityState === "visible") {
      doUpload();
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

    /**
     * Set/update the room_token used to authenticate uploads
     * (`Authorization: Bearer <token>`). Called by WASM on join and again on
     * every token refresh so long calls stay authenticated. O(1); does not
     * touch the buffer or upload timer. The token is a credential and is
     * never logged or written into the upload body.
     */
    setAuthToken: function (token) {
      authToken = token || null;
    },

    /** Called by WASM on hangup — uploads what fits the keepalive quota; the rest stays buffered. */
    flush: function () {
      sendKeepalive(false);
    },
  };
})();
