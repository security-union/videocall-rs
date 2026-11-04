// Copyright 2026 Security Union LLC
// Licensed under MIT OR Apache-2.0

(function (plan, tokenUrl) {
  window.__original_fetch = window.__original_fetch || window.fetch;
  window.__hostApi = { plan: plan, posts: [], refreshes: 0, errors: [] };
  if (!window.__hostApiHooked) {
    window.__hostApiHooked = true;
    var record = function (message) {
      if (window.__hostApi) window.__hostApi.errors.push(String(message));
    };
    window.addEventListener("error", function (e) {
      record(e.message);
    });
    window.addEventListener("unhandledrejection", function (e) {
      record(e.reason);
    });
    var consoleError = console.error;
    console.error = function () {
      var line = Array.prototype.join.call(arguments, " ");
      if (line.indexOf("panicked at") >= 0) record(line);
      return consoleError.apply(console, arguments);
    };
  }
  window.fetch = function (input, init) {
    var req = typeof input === "string" ? null : input;
    var url = req ? req.url : input;
    var method = String((init && init.method) || (req && req.method) || "GET").toUpperCase();
    var respond = function (status, body) {
      var resp = new Response(JSON.stringify(body), {
        status: status,
        headers: { "Content-Type": "application/json" },
      });
      Object.defineProperty(resp, "url", { value: url });
      return Promise.resolve(resp);
    };
    if (method === "POST" && url === tokenUrl) {
      window.__hostApi.refreshes += 1;
      return respond(200, {
        access_token: "refreshed-" + window.__hostApi.refreshes,
        token_type: "Bearer",
        expires_in: 3600,
      });
    }
    var path = url.replace(/^[a-z]+:\/\/[^\/]+/, "");
    if (method === "POST") {
      window.__hostApi.posts.push([path, (req && req.headers.get("Authorization")) || ""]);
    }
    var queue = window.__hostApi.plan[path] || [];
    var step = queue.length ? queue.shift() : { status: 200 };
    var reply = function () {
      if (step.status === 200) {
        return respond(200, { success: true, result: step.result || {} });
      }
      var code = step.code || { 401: "UNAUTHORIZED", 403: "NOT_HOST" }[step.status] || "INTERNAL_ERROR";
      return respond(step.status, {
        success: false,
        result: { code: code, message: step.message || code },
      });
    };
    if (step.held) {
      return new Promise(function (resolve) {
        window.__hostApi.release = function () {
          resolve(reply());
        };
      });
    }
    return reply();
  };
});
