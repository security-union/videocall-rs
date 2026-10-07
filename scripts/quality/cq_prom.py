"""Prometheus HTTP API access and PromQL building for the call quality scorer."""

import base64
import json
import math
import os
import time
import urllib.error
import urllib.parse
import urllib.request

RE2_SPECIAL = set(".+*?()|[]{}^$\\")


class PromError(Exception):
    pass


class Samples(list):
    """(ts, value) samples of one series; non_finite holds the timestamps of the NaN/±Inf samples left out."""
    non_finite = ()


def re2_escape(text):
    return "".join("\\" + ch if ch in RE2_SPECIAL else ch for ch in text)


def promql_string(value):
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n") + '"'


def any_of_regex(values):
    return "|".join(re2_escape(v) for v in sorted(values))


def selector(metric, matchers):
    """matchers: list of (label, op, value) with op in {'=', '=~'}; values are raw, escaped here."""
    parts = [f"{label}{op}{promql_string(value)}" for label, op, value in matchers]
    return metric + "{" + ",".join(parts) + "}"


def parse_matrix(payload):
    """Prometheus query_range JSON -> list of (labels dict, Samples). NaN and ±Inf samples are left out of the
    values and listed in Samples.non_finite, so the scorer can tell them from an absent sample."""
    if not isinstance(payload, dict) or payload.get("status") != "success":
        raise PromError(f"query failed: {payload.get('error') if isinstance(payload, dict) else payload!r}")
    data = payload.get("data") or {}
    if data.get("resultType") != "matrix":
        raise PromError(f"expected a matrix result, got {data.get('resultType')!r}")
    if payload.get("warnings"):
        raise PromError(f"partial or degraded response: {payload['warnings']!r}")
    out = []
    for item in data.get("result") or []:
        samples, dropped = Samples(), []
        for ts, raw in item.get("values", []):
            val = float(raw)
            if not math.isfinite(val):
                dropped.append(float(ts))
                continue
            samples.append((float(ts), val))
        samples.non_finite = dropped
        out.append((dict(item.get("metric", {})), samples))
    return out


def urllib_transport(url, body, headers, timeout=60):
    req = urllib.request.Request(url, data=body, method="POST", headers=headers)
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return resp.read()


def auth_headers(bearer_env=None, basic_env=None, environ=None):
    """Build auth headers from environment variable NAMES; secrets never appear on the command line."""
    env = os.environ if environ is None else environ
    if bearer_env:
        token = env.get(bearer_env)
        if not token:
            raise PromError(f"environment variable {bearer_env} is empty or unset")
        return {"Authorization": f"Bearer {token}"}
    if basic_env:
        user_var, _, pass_var = basic_env.partition(":")
        user, password = env.get(user_var), env.get(pass_var)
        if not user or password is None:
            raise PromError(f"environment variables {basic_env} are not both set")
        cred = base64.b64encode(f"{user}:{password}".encode()).decode()
        return {"Authorization": f"Basic {cred}"}
    return {}


def _retryable(exc):
    if isinstance(exc, urllib.error.HTTPError):
        return exc.code == 429 or exc.code >= 500
    return isinstance(exc, (urllib.error.URLError, TimeoutError, ConnectionError))


class PromClient:
    def __init__(self, base_url, headers=None, transport=urllib_transport, timeout=60, retries=3,
                 backoff_s=1.0, sleep=time.sleep):
        self.base_url = base_url.rstrip("/")
        self.headers = dict(headers or {})
        self.transport = transport
        self.timeout = timeout
        self.retries = retries
        self.backoff_s = backoff_s
        self.sleep = sleep
        self.queries = []

    def _post(self, url, body, headers, query):
        for attempt in range(self.retries + 1):
            try:
                if self.transport is urllib_transport:
                    return self.transport(url, body, headers, self.timeout)
                return self.transport(url, body, headers)
            except PromError:
                raise
            except Exception as exc:
                if attempt >= self.retries or not _retryable(exc):
                    raise PromError(f"request failed for {query}: {exc}") from exc
                self.sleep(self.backoff_s * (2 ** attempt))
        raise PromError(f"request failed for {query}")

    def range(self, query, start, end, step):
        self.queries.append(query)
        body = urllib.parse.urlencode(
            {"query": query, "start": f"{start:.3f}", "end": f"{end:.3f}", "step": f"{step}s"}
        ).encode()
        headers = {"Content-Type": "application/x-www-form-urlencoded", **self.headers}
        raw = self._post(self.base_url + "/api/v1/query_range", body, headers, query)
        try:
            payload = json.loads(raw)
        except ValueError as exc:
            raise PromError(f"non-JSON response for {query}") from exc
        return parse_matrix(payload)


def counter_delta(samples, window_start):
    """Increase of a cumulative value after window_start, from raw time-ordered samples.

    The baseline is the last sample at or before window_start, or 0 when the series did not
    exist yet (absent-before = 0). Decreases are treated as resets.
    """
    prev = 0.0
    total = 0.0
    for ts, val in samples:
        if ts <= window_start:
            prev = val
            continue
        total += val - prev if val >= prev else val
        prev = val
    return total


def merge_max(series_list):
    """Merge several sample lists into one timeline, taking the max per timestamp."""
    merged = {}
    for samples in series_list:
        for ts, val in samples:
            if ts not in merged or val > merged[ts]:
                merged[ts] = val
    return sorted(merged.items())
