#!/usr/bin/env python3
"""Call quality scorer v1 (Discussion #2913; design: docs/quality-at-scale/CALL_QUALITY_SCORING.md).

Run mode:      --manifest run.json --prom-url URL
Real meeting:  --meeting ID --window START,END --prom-url URL
Validate only: --manifest run.json --validate-only

Identities of real people are pseudonymised by default (--no-pseudonymise opts out, refused when CI is set).

Exit codes: 0 PASS or REPORT, 1 FAIL, 2 INVALID, 3 error. Stdlib only (PyYAML optional for YAML manifests).
"""

import argparse
import hashlib
import json
import math
import os
import re
import shlex
import sys
import urllib.parse
from datetime import datetime

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import cq_manifest  # noqa: E402
import cq_privacy  # noqa: E402
import cq_prom  # noqa: E402
import cq_report  # noqa: E402
import cq_score  # noqa: E402

EXIT = {"PASS": 0, "REPORT": 0, "FAIL": 1, "INVALID": 2}
EXIT_ERROR = 3


class UsageError(Exception):
    pass


class NoDataError(Exception):
    pass


def parse_time(text):
    text = text.strip()
    try:
        return float(text)
    except ValueError:
        pass
    try:
        dt = datetime.fromisoformat(text.replace("Z", "+00:00"))
    except ValueError as exc:
        raise UsageError(f"cannot parse time {text!r}: use epoch seconds or ISO 8601 with a zone") from exc
    if dt.tzinfo is None:
        raise UsageError(f"time {text!r} has no zone; add Z or an offset")
    return dt.timestamp()


def parse_window(text):
    parts = text.split(",")
    if len(parts) != 2:
        raise UsageError(f"--window must be START,END (got {text!r})")
    start, end = parse_time(parts[0]), parse_time(parts[1])
    if start >= end:
        raise UsageError(f"--window start must be before end (got {text!r})")
    return start, end


def redact_url(url):
    parts = urllib.parse.urlsplit(url)
    netloc = parts.hostname or ""
    if parts.port:
        netloc += f":{parts.port}"
    return urllib.parse.urlunsplit((parts.scheme, netloc, parts.path, "", ""))


def redacted_argv(argv, hide_exclude=False):
    redactors = {"--prom-url": redact_url}
    if hide_exclude:
        redactors["--exclude-regex"] = lambda _: "<redacted>"
    out, pending = [], None
    for arg in argv:
        flag, eq, value = arg.partition("=")
        if pending:
            out.append(pending(arg))
            pending = None
        elif arg in redactors:
            out.append(arg)
            pending = redactors[arg]
        elif eq and flag in redactors:
            out.append(f"{flag}={redactors[flag](value)}")
        else:
            out.append(arg)
    return out


def in_ci(environ):
    return (environ.get("CI") or "").lower() not in ("", "0", "false")


def finite_or_null(obj, path, nulled):
    """Copy of obj with NaN/±Inf floats replaced by None; their paths are appended to nulled."""
    if isinstance(obj, float) and not math.isfinite(obj):
        nulled.append(path)
        return None
    if isinstance(obj, dict):
        return {k: finite_or_null(v, f"{path}.{k}", nulled) for k, v in obj.items()}
    if isinstance(obj, (list, tuple)):
        return [finite_or_null(v, f"{path}[{i}]", nulled) for i, v in enumerate(obj)]
    return obj


class _Parser(argparse.ArgumentParser):
    def error(self, message):
        head, sep, rest = message.partition("unrecognized arguments: ")
        if sep:
            message = head + sep + " ".join(t.split("=")[0] if t.startswith("-") else "<value>" for t in rest.split())
        message = re.sub(r"(\w+://)[^/\s@]*@", r"\1<redacted>@", message)
        self.print_usage(sys.stderr)
        self.exit(EXIT_ERROR, f"{self.prog}: error: {message}\n")


def build_parser():
    p = _Parser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter, allow_abbrev=False)
    p.add_argument("--manifest", help="run manifest (JSON, or YAML if PyYAML is installed)")
    p.add_argument("--meeting", help="real-meeting mode: meeting_id")
    p.add_argument("--window", help="real-meeting mode: START,END (epoch seconds or ISO 8601)")
    p.add_argument("--exclude-regex", help="real-meeting mode: drop reporters whose user id matches")
    p.add_argument("--prom-url", help="Prometheus base URL (or a Grafana datasource-proxy URL)")
    p.add_argument("--auth-bearer-env", metavar="VAR", help="env var holding a bearer token")
    p.add_argument("--auth-basic-env", metavar="USER_VAR:PASS_VAR", help="env vars holding basic-auth credentials")
    p.add_argument("--timeout", type=float, default=60.0, help="per-request timeout in seconds (default 60)")
    p.add_argument("--retries", type=int, default=3, help="retries with exponential backoff on 429/5xx/timeouts")
    p.add_argument("--config", help="JSON file deep-merged over call_quality_config.json")
    p.add_argument("--generator-verdict", help='JSON file {"ok": bool, "detail": str} from the load fleet (G-V5)')
    p.add_argument("--split-transport", action="store_true", help="split cells by the receiver's actual transport")
    p.add_argument("--out-dir", help="write result.json and report.md here (default: print Markdown)")
    p.add_argument("--no-pseudonymise", action="store_true",
                   help="print real identities (emails, display names); refused when the CI env var is set")
    p.add_argument("--validate-only", action="store_true", help="validate the manifest and exit")
    p.add_argument("--print-queries", action="store_true", help="print the PromQL the scorer would run and exit")
    return p


def _discover_users(client, meeting, start, end, step_s, exclude):
    users = set()
    for name in (cq_score.M_SENT, cq_score.M_PEER_INFO):
        expr = cq_prom.selector(name, [("meeting_id", "=", meeting)])
        for labels, _ in client.range(expr, start, end, step_s):
            if labels.get("peer_id"):
                users.add(labels["peer_id"])
    if exclude:
        rx = re.compile(exclude)
        users = {u for u in users if not rx.search(u)}
    return users


def main(argv=None, transport=None, environ=None):
    args = build_parser().parse_args(argv)
    env = os.environ if environ is None else environ
    hide_text = str

    def emit(text):
        print(hide_text(text), file=sys.stderr)
    try:
        if args.prom_url and "@" in args.prom_url:
            raise UsageError("--prom-url must not carry credentials (it contains '@'); pass them with "
                             "--auth-basic-env USER_VAR:PASS_VAR or --auth-bearer-env VAR")
        if args.no_pseudonymise and in_ci(env):
            raise UsageError("--no-pseudonymise is refused in CI: reports must not carry real identities")
        cfg = cq_score.load_config(args.config)
        step_s = cq_score.cv(cfg, "sampling", "scrape_step_s")
        real = args.manifest is None
        if real and not (args.meeting and args.window):
            raise UsageError("either --manifest, or --meeting with --window, is required")
        if not real:
            manifest = cq_manifest.load_manifest(args.manifest)
            hide_text = cq_privacy.text_pseudonymiser(cq_privacy.raw_manifest_identities(manifest), os.urandom(16))
            cq_manifest.require_valid(manifest)
            hide_text = str
            if args.validate_only:
                print(f"manifest OK: {len(manifest['participants'])} participants, {len(manifest['steps'])} steps")
                return 0
        salt = os.urandom(16)
        if not args.no_pseudonymise and not real:
            hide_text = cq_privacy.text_pseudonymiser(cq_privacy.identities_to_hide(manifest, {}, False), salt)
        if args.print_queries:
            if real:
                raise UsageError("--print-queries needs --manifest")
            for step in manifest["steps"]:
                obs = {p["user_id"] for p in manifest["participants"] if p["observer"] and p["user_id"]}
                for name, expr, start, end in cq_score.step_queries(manifest, step, cfg, obs):
                    print(f"[{step['step_id']}] {name}: {hide_text(expr)}")
            return 0
        if not args.prom_url:
            raise UsageError("--prom-url is required")
        headers = cq_prom.auth_headers(args.auth_bearer_env, args.auth_basic_env, env)
        client = cq_prom.PromClient(args.prom_url, headers, transport or cq_prom.urllib_transport,
                                    timeout=args.timeout, retries=args.retries)
        if real:
            start, end = parse_window(args.window)
            users = _discover_users(client, args.meeting, start, end, step_s, args.exclude_regex)
            if not users:
                raise NoDataError(f"no participants found for meeting {args.meeting!r} in window "
                                  f"{start:.0f}..{end:.0f} (check the meeting id, the window and the Prometheus URL)")
            manifest = cq_manifest.synthesize_real_meeting_manifest(args.meeting, start, end, users)
            observer_filter = None
        else:
            observer_filter = {p["user_id"] for p in manifest["participants"] if p["observer"] and p["user_id"]}
        datasets = {s["step_id"]: cq_score.fetch_step_data(client, manifest, s, cfg, observer_filter)
                    for s in manifest["steps"]}
        verdict = None
        if args.generator_verdict:
            with open(args.generator_verdict, encoding="utf-8") as fh:
                verdict = json.load(fh)
            bad = cq_manifest.non_finite_paths(verdict)
            if bad:
                raise UsageError(f"--generator-verdict holds NaN or infinity at {', '.join(bad)}")
        result = cq_score.score_run(manifest, datasets, cfg, "real" if real else "run", verdict,
                                    args.split_transport)
        result["run"]["config_sha256"] = hashlib.sha256(
            json.dumps(cfg, sort_keys=True).encode()).hexdigest()
        if real and not result["steps"][0]["cells"]:
            raise NoDataError(f"meeting {args.meeting!r} has participants but no per-pair quality series "
                              f"in the window; nothing to report")
        hide = not args.no_pseudonymise
        if hide:
            result = cq_privacy.pseudonymise(result, cq_privacy.identities_to_hide(manifest, datasets, real), salt)
        nulled = []
        result = finite_or_null(result, "$", nulled)
        if nulled:
            result["run"]["non_finite_values_nulled"] = nulled
        result["run"]["pseudonymised"] = hide
        command = "python3 scripts/quality/call_quality_score.py " + " ".join(
            shlex.quote(a) for a in redacted_argv(argv if argv is not None else sys.argv[1:], hide))
        markdown = cq_report.render_markdown(result, command)
        if args.out_dir:
            os.makedirs(args.out_dir, exist_ok=True)
            with open(os.path.join(args.out_dir, "result.json"), "w", encoding="utf-8") as fh:
                json.dump(result, fh, indent=2, sort_keys=True, allow_nan=False)
            with open(os.path.join(args.out_dir, "report.md"), "w", encoding="utf-8") as fh:
                fh.write(markdown)
        else:
            sys.stdout.write(markdown)
        return EXIT[result["verdict"]]
    except UsageError as exc:
        emit(f"usage error: {exc}")
        return EXIT_ERROR
    except NoDataError as exc:
        emit(f"no data: {exc}")
        return EXIT_ERROR
    except cq_manifest.ManifestError as exc:
        emit(str(exc))
        return EXIT_ERROR
    except cq_score.ConfigError as exc:
        emit(str(exc))
        return EXIT_ERROR
    except (cq_prom.PromError, OSError, ValueError, KeyError) as exc:
        emit(f"error: {exc}")
        return EXIT_ERROR
    except Exception as exc:  # noqa: BLE001
        emit(f"internal error ({type(exc).__name__}): {exc}")
        return EXIT_ERROR


if __name__ == "__main__":
    sys.exit(main())
