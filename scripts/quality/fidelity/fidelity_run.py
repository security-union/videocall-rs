#!/usr/bin/env python3
"""Bot-vs-browser fidelity harness: Rust bots and bots-app browsers in one room, measured at the relay.

  fidelity_run.py run [--scenario FILE] [--out-dir DIR] [--relay-container NAME] [--capture-iface IF]
                      [--min-free-gb 10]
  fidelity_run.py analyze RUN_DIR
  fidelity_run.py report RUN_DIR
  fidelity_run.py retest RUN_DIR_A RUN_DIR_B [--tol 0.05]
  fidelity_run.py remote

run needs a running e2e stack (docs/quality-at-scale/FIDELITY_HARNESS.md). Exit codes: 0 PASS, 1 FAIL,
2 INCOMPLETE, 3 ERROR, 4 INVALID; retest exits 0 when the runs agree, 1 when they do not and 3 when either run
is INVALID or has no result. run and analyze write report.html when they reach a result; a report failure never
changes their exit code. report exits 0, or 3 when it has no result, cannot write the page, is refused or is
interrupted.
"""

import gzip
import hashlib
import json
import os
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))
sys.path.insert(0, HERE)

import cq_local  # noqa: E402
import cq_scenario  # noqa: E402
import fidelity_measures  # noqa: E402
from fidelity_signals import STOP, Interrupted  # noqa: E402
import fidelity_tolerance  # noqa: E402
from scenario_run import _Parser  # noqa: E402
import ws_capture  # noqa: E402
from ws_capture import lock_run  # noqa: E402

DEFAULT_SCENARIO = os.path.join(HERE, "scenarios", "room10.yaml")
FIDELITY_KEYS = {"browser_simulcast_max_layers", "browser_hardware_concurrency", "browser_source", "settle",
                 "browser_min_video_fps"}
MAX_EOF_PARTIAL_BYTES = 64 * 1024
GEOMETRIES = ("640x480", "1280x720")
RUN_SCHEMA = "fidelity-run/v1"
EXIT_ERROR = 3
VERDICT_PAGES = ("table.txt", "table.md", "report.html")
HELD = []
REMOTE_GAP = """There is no remote mode: per-participant, per-kind numbers need relay counters.
Needed: relay_session_ingress_bytes_total and relay_session_ingress_packets_total, labelled {session_id, kind} with
kind one of video_l0, video_l1, video_l2, audio, heartbeat, health, keyframe_request, layer_preference, viewport,
other.
Not enough for the table: relay_room_bytes_total (per room, all kinds),
videocall_server_data_bytes_total (per session, all kinds), relay_keyframe_requests_total (per room) and the
clients' HEALTH series (no per-kind split for WebTransport, no keyframe sizes)."""


def fidelity_settings(doc):
    """(settings, errors) from the scenario's fidelity block, which cq_scenario does not know."""
    block = doc.pop("fidelity", None) if isinstance(doc, dict) else None
    if not isinstance(block, dict):
        return None, ["fidelity: a mapping is required (browser_simulcast_max_layers, browser_source, settle)"]
    errors = [f"fidelity.{k}: unknown key" for k in sorted(set(block) - FIDELITY_KEYS)]
    layers = block.get("browser_simulcast_max_layers", 3)
    if not isinstance(layers, int) or isinstance(layers, bool) or not 1 <= layers <= 3:
        errors.append("fidelity.browser_simulcast_max_layers: must be 1, 2 or 3")
    cores = block.get("browser_hardware_concurrency", 10)
    if not isinstance(cores, int) or isinstance(cores, bool) or cores < 1:
        errors.append("fidelity.browser_hardware_concurrency: must be a positive integer")
    source = block.get("browser_source", {})
    if not isinstance(source, dict) or any(v not in GEOMETRIES for v in source.values()):
        errors.append(f"fidelity.browser_source: role -> one of {', '.join(GEOMETRIES)}")
        source = {}
    min_fps = block.get("browser_min_video_fps", [])
    try:
        min_fps = [float(v) for v in min_fps if not isinstance(v, bool)] if isinstance(min_fps, list) else None
    except ValueError:
        min_fps = None
    if min_fps is None or len(min_fps) > 3 or len(min_fps) != len(block.get("browser_min_video_fps", [])) or any(
            v < 0 or v != v for v in min_fps):
        errors.append("fidelity.browser_min_video_fps: a list of up to 3 non-negative numbers, one per video layer")
        min_fps = []
    settle = cq_scenario.duration_s(block.get("settle", "30s"))
    if settle is None:
        errors.append("fidelity.settle: must be a duration such as 30s")
    return {"browser_simulcast_max_layers": layers, "browser_hardware_concurrency": cores, "browser_source": source,
            "settle_s": settle or 0, "browser_min_video_fps": min_fps}, errors


def plan_errors(plan, fid):
    errors = []
    if plan["target"] != "local":
        errors.append(f"run.target: {plan['target']}; the capture mode runs on target local only")
    if plan["events"]:
        errors.append("events: the hold must be steady, so no events")
    browser_roles = {p["role"] for p in plan["participants"] if p["fleet"] == "browser"}
    for proc in plan["processes"]:
        if proc["fleet"] == "browser" and proc["transport"] != "websocket":
            errors.append(f"{proc['proc_id']}: transport {proc['transport']}; the capture decodes WebSocket only")
    errors += [f"fidelity.browser_source.{r}: not a browser role" for r in sorted(set(fid["browser_source"])
                                                                                   - browser_roles)]
    if not browser_roles or all(p["fleet"] == "browser" for p in plan["participants"]):
        errors.append("population: needs at least one browser role and one Rust role")
    return errors


def compile_run(path, commit):
    doc, raw = cq_scenario.load_scenario(path)
    fid, errors = fidelity_settings(doc)
    if errors:
        raise cq_scenario.ScenarioError(errors)
    prefix = doc.get("run", {}).get("id_prefix") if isinstance(doc.get("run"), dict) else None
    run_id = cq_scenario.derive_run_id(prefix, time.time(), commit)
    plan = cq_scenario.compile_scenario(doc, run_id=run_id, scenario_file=os.path.basename(path),
                                        scenario_sha256=hashlib.sha256(raw).hexdigest())
    errors = plan_errors(plan, fid)
    if errors:
        raise cq_scenario.ScenarioError(errors)
    return plan, fid


def expected_layers(plan, fid):
    procs = {p["proc_id"]: p for p in plan["processes"]}
    out = {}
    for p in plan["participants"]:
        if p["publishes"]["camera"]:
            out[p["name"]] = fid["browser_simulcast_max_layers"] if p["fleet"] == "browser" \
                else procs[p["process"]]["video_layers"]
    return out


def read_json(path):
    with open(path, encoding="utf-8") as fh:
        return json.load(fh)


def write_json(path, obj):
    """Whole or not at all, so a kill mid-write cannot cost a run folder its run.json (see owns_run_folder)."""
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as fh:
        json.dump(obj, fh, indent=1)
        fh.write("\n")
    os.replace(tmp, path)


def read_samples(path):
    out = []
    try:
        with gzip.open(path, "rt", encoding="utf-8") as fh:
            for line in fh:
                r = json.loads(line)
                out.append((r["t"], fidelity_measures.parse_metrics(r["v"])))
    except (OSError, EOFError, ValueError, KeyError):
        pass
    return out


def raw_capture_names(capture_dir):
    """The relay.pcap* names in capture_dir, sorted. Unlike glob, a folder it cannot list raises OSError."""
    try:
        return sorted(n for n in os.listdir(capture_dir) if n.startswith("relay.pcap"))
    except (FileNotFoundError, NotADirectoryError):
        return []


def remove_raw_capture(capture_dir):
    """Delete the raw pcap and any file named after it; a folder that cannot be listed and files that could not be
    deleted are raised as one OSError naming them."""
    removed, failed = [], []
    try:
        names = raw_capture_names(capture_dir)
    except OSError as exc:
        raise OSError(f"could not delete the raw capture: cannot list {capture_dir} ({exc.strerror or exc})") from exc
    for name in names:
        path = os.path.join(capture_dir, name)
        try:
            os.remove(path)
            removed.append(name)
        except FileNotFoundError:
            continue
        except OSError as exc:
            failed.append(f"{path} ({exc.strerror or exc})")
    if failed:
        raise OSError("could not delete the raw capture: " + "; ".join(failed))
    return removed


def owns_run_folder(run_dir):
    """Whether run_dir holds this harness's run.json; a file it cannot read raises OSError."""
    path = os.path.join(run_dir, "run.json")
    if os.path.islink(path) or not os.path.isfile(path):
        return False
    try:
        return read_json(path).get("schema") == RUN_SCHEMA
    except (ValueError, AttributeError):
        return False


def sweep_raw_captures(out_dir, warn=lambda text: None):
    """Delete the relay.pcap* left in out_dir's run folders of this harness whose lock no live process holds. Only a
    folder with such a file is locked, since the lock creates a file. An own capture/ it cannot list raises OSError."""
    removed = []
    try:
        entries = sorted(n for n in os.listdir(out_dir) if not n.startswith("."))
    except (FileNotFoundError, NotADirectoryError):
        return removed
    for entry in entries:
        run_dir = os.path.join(out_dir, entry)
        cap = os.path.join(run_dir, "capture")
        if os.path.islink(run_dir) or os.path.islink(cap):
            continue
        if os.path.isdir(run_dir) and not os.access(run_dir, os.X_OK):
            warn(f"warning: cannot look inside {run_dir}; if a run was killed there, delete its capture/relay.pcap*")
            continue
        try:
            found, unlisted = raw_capture_names(cap), None
        except OSError as exc:
            found, unlisted = [], exc
        if not (found or unlisted) or not owns_run_folder(run_dir):
            continue
        if unlisted:
            raise OSError(f"cannot list {cap}, so it may hold a raw capture ({unlisted.strerror or unlisted})")
        held = lock_run(run_dir)
        if held is None:
            continue
        try:
            removed += [os.path.join(cap, name) for name in remove_raw_capture(cap)]
        finally:
            held.close()
    return removed


def receive_notes(plan):
    """{Rust role: its receive setting}; the browsers keep the UI's own video layer selection."""
    roles = {p["user_id"]: p["role"] for p in plan["participants"]}
    out = {}
    for proc in plan["processes"]:
        rec = proc.get("receive") or {}
        said = [f"pinned to video layer {rec['pin_video_layer']}"] if rec.get("pin_video_layer") is not None else []
        if rec.get("viewport_visible_count") is not None:
            said.append(f"viewport_visible_count {rec['viewport_visible_count']}")
        for uid in proc["participants"] if said else ():
            out[roles[uid]] = f"Rust receive {', '.join(said)}; the browsers choose their own video layers"
    return out


def cleanup_run(local, run_dir, run_id, out):
    """Remove the run's containers, then its raw capture; returns what failed, written to run.json before a
    best-effort warning. The signal handlers only record, so nothing interrupts this."""
    failures = []
    try:
        failures += local.remove_containers()
    except Exception as exc:  # noqa: BLE001 - the raw capture must still be deleted
        failures.append(f"removing the containers: {type(exc).__name__}: {exc}")
    try:
        remove_raw_capture(os.path.join(run_dir, "capture"))
    except OSError as exc:
        failures.append(str(exc))
    if failures:
        try:
            record(run_dir, cleanup_errors=failures)
        except OSError as exc:
            failures.append(f"recording the cleanup errors: {exc.strerror or exc}")
        say(out, f"warning: cleanup of {cq_local.LABEL}={run_id} incomplete: {'; '.join(failures)}")
    return failures


def say(out, text):
    try:
        out(text)
    except OSError:
        pass


def overhead_line(run_dir, window, facts, stats):
    samples = []
    try:
        with gzip.open(os.path.join(run_dir, "samples", "docker_stats.jsonl.gz"), "rt", encoding="utf-8") as fh:
            samples = [json.loads(line) for line in fh]
    except (OSError, EOFError, ValueError):
        pass
    cpu = {}
    for s in samples:
        if window[0] <= s.get("t", 0) < window[1]:
            for role, pct in (s.get("cpu_pct") or {}).items():
                cpu.setdefault(role, []).append(pct)
    mean = {k: sum(v) / len(v) for k, v in cpu.items() if v}
    facts["overhead_cpu_pct_mean"] = mean
    parts = [f"{k} {v:.0f} %" for k, v in sorted(mean.items())] or ["no docker stats samples"]
    return (f"Capture overhead: mean CPU over the hold (100 % = one core): {', '.join(parts)}; pcap "
            f"{stats.get('pcap_bytes', 0) / 1e6:.0f} MB")


def room_note(room):
    if not room:
        return "\nMixed-room relay counters: no samples\n"
    rates = room["per_s"]

    def total(name, **want):
        return sum(v for k, v in rates.items() if k.startswith(name + "{")
                   and all(f'{lk}="{lv}"' in k for lk, lv in want.items()))

    admitted = sum(v for k, v in rates.items() if k.startswith("relay_keyframe_requests_total{") and "admitted" in k)
    return (f"\nMixed-room relay counters over the hold (information, not judged): room bytes in "
            f"{total('relay_room_bytes_total', direction='inbound') * 8 / 1000:.0f} kbps, out "
            f"{total('relay_room_bytes_total', direction='outbound') * 8 / 1000:.0f} kbps; keyframe requests "
            f"admitted {admitted:.2f}/s of {total('relay_keyframe_requests_total'):.2f}/s\n")


def analyze(run_dir, out=print):
    """Parse the capture, measure the hold, evaluate the tolerance table and write fidelity.json and table.*."""
    try:
        return _analyze(run_dir, out)
    finally:
        try:
            remove_raw_capture(os.path.join(run_dir, "capture"))
        except OSError as exc:
            record(run_dir, cleanup_errors=[str(exc)])
            raise


def capture_problems(stats, window):
    problems = [] if stats.get("ws_messages") else ["the capture holds no WebSocket messages"]
    for key, what in (("decode_errors", "messages failed to decode"),
                      ("unknown_text", "messages with a frame type or tier outside the known values"),
                      ("reassembly_gaps", "TCP reassembly gaps"),
                      ("unsynced_bytes", "bytes on connections opened before the capture"),
                      ("eof_pending_segments", "TCP segments never reassembled: a gap in the capture")):
        if stats.get(key):
            problems.append(f"capture: {stats[key]} {what}")
    partial = stats.get("eof_partial_messages", [])
    inside = [n for t, n in partial if t is None or window[0] <= t < window[1]]
    if inside:
        problems.append(f"capture: {len(inside)} WebSocket message(s) never completed, last seen inside the hold")
    if stats.get("eof_partial_bytes", 0) > MAX_EOF_PARTIAL_BYTES:
        problems.append(f"capture: {stats['eof_partial_bytes']} bytes of WebSocket frames never completed")
    return problems


def _analyze(run_dir, out):
    run = read_json(os.path.join(run_dir, "run.json"))
    plan, fid, window = run["plan"], run["fidelity"], run.get("window")
    if not window:
        record(run_dir, error=run.get("error") or "the run recorded no hold window, so it did not finish")
        return EXIT_ERROR
    cap = os.path.join(run_dir, "capture")
    pcap, messages = os.path.join(cap, "relay.pcap"), os.path.join(cap, "messages.csv.gz")
    stats_path = os.path.join(cap, "capture-stats.json")
    problems = list(run.get("preconditions", []))
    conns = os.path.join(cap, "conns.json")
    if not all(os.path.exists(f) for f in (stats_path, messages, conns)):
        if not os.path.exists(pcap):
            record(run_dir, error="no capture (capture/relay.pcap) to analyse")
            return EXIT_ERROR
        users = {p["user_id"] for p in plan["participants"]}
        write_json(stats_path, ws_capture.parse_pcap(pcap, cap, users=users,
                                                     check=lambda: STOP.check("parsing the capture")))
    stats = read_json(stats_path)
    problems += capture_problems(stats, window)
    summary = (run.get("facts") or {}).get("capture") or {}
    if summary.get("packets_captured") is None or summary.get("packets_dropped_by_kernel") != 0:
        problems.append("the capture has no tcpdump summary with zero dropped packets, so it may be incomplete")
    verdict = (run.get("facts") or {}).get("host_resource_verdict")
    if not isinstance(verdict, dict) or verdict.get("ok") is not True:
        problems.append("no affirmative host resource verdict for the hold, so browser starvation is not ruled out")
    m = fidelity_measures.measure_files(messages, conns, plan["participants"], tuple(window),
                                        expected_layers(plan, fid))
    problems += fidelity_measures.cadence_problems(m, fid.get("browser_min_video_fps"))
    result = fidelity_tolerance.evaluate(m, problems, receive_notes(plan))
    room = fidelity_measures.relay_deltas(read_samples(os.path.join(run_dir, "samples", "relay_metrics.jsonl.gz")),
                                          plan["meeting_id"], window)
    facts = run.get("facts", {})
    header = [
        "Bot vs browser fidelity, local capture at the relay",
        f"Run {plan['run_id']}, hold {window[1] - window[0]:.0f} s from "
        f"{time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime(window[0]))}",
        f"Commit: harness {(facts.get('harness_commit') or '?')[:12]}, stack {(facts.get('stack_commit') or '?')[:12]}",
        f"Capture: {stats.get('ws_messages', 0)} WS messages, kernel drops "
        f"{(facts.get('capture') or {}).get('packets_dropped_by_kernel', '?')}, decode errors "
        f"{stats.get('decode_errors', '?')}, messages in the hold with no user id "
        f"{m['unidentified_messages_in_window']}",
        f"Browsers: {fid['browser_simulcast_max_layers']} video layers requested, WebSocket; Rust video_mode "
        f"{plan['video_mode']}, bot DIAGNOSTICS {plan['bot_diagnostics']}",
        "Reference: the browsers of each class in this room (canvas-clock camera, not a real camera)",
        overhead_line(run_dir, window, facts, stats),
    ]
    write_json(os.path.join(run_dir, "fidelity.json"), {"schema": RUN_SCHEMA, "run_id": plan["run_id"],
                                                        "facts": facts, "capture": stats, "measures": m,
                                                        "result": result, "relay_room_per_s": room})
    text = fidelity_tolerance.render(result, header) + room_note(room)
    with open(os.path.join(run_dir, "table.txt"), "w", encoding="utf-8") as fh:
        fh.write(text)
    with open(os.path.join(run_dir, "table.md"), "w", encoding="utf-8") as fh:
        fh.write(fidelity_tolerance.render(result, [f"{h}  " for h in header], markdown=True) + room_note(room))
    remove_raw_capture(os.path.join(run_dir, "capture"))
    out(text)
    return result["exit_code"]


def cmd_run(args, out=print):
    import fidelity_local
    from cq_local import AdapterError

    root = os.path.dirname(os.path.dirname(os.path.dirname(HERE)))
    commit = subprocess.run(["git", "-C", root, "rev-parse", "HEAD"], capture_output=True, text=True,
                            check=False, timeout=cq_local.DOCKER_S).stdout.strip()
    try:
        plan, fid = compile_run(args.scenario, commit)
    except cq_scenario.ScenarioError as exc:
        out(str(exc))
        return EXIT_ERROR
    run_dir = os.path.join(os.path.abspath(args.out_dir), plan["run_id"])
    if os.path.exists(run_dir):
        out(f"{run_dir} exists; run ids change once a minute, so retry in a minute")
        return EXIT_ERROR
    local = fidelity_local.LocalRun(plan, fid, run_dir, out, args.relay_container, args.capture_iface,
                                    args.min_free_gb)
    try:
        problems = local.preflight()
    except AdapterError as exc:
        problems = [str(exc)]
    if problems:
        out("refused:\n  " + "\n  ".join(problems))
        return EXIT_ERROR
    try:
        if local.run_containers():
            out(f"refused:\n  a container labelled {cq_local.LABEL} appeared during preflight")
            return EXIT_ERROR
        swept = sweep_raw_captures(os.path.abspath(args.out_dir), lambda text: say(out, text))
    except (AdapterError, OSError) as exc:
        out(f"refused:\n  {exc}; if an earlier run was killed, delete its capture/relay.pcap* by hand")
        return EXIT_ERROR
    if swept:
        say(out, f"deleted the raw capture an earlier, killed run left: {', '.join(swept)}")
    os.makedirs(run_dir)
    held = lock_run(run_dir)
    if held is None:
        out(f"refused:\n  another process holds the new run folder {run_dir}")
        return EXIT_ERROR
    HELD.append(held)
    run = {"schema": RUN_SCHEMA, "mode": "local", "plan": plan, "fidelity": fid, "argv": sys.argv[1:]}
    write_json(os.path.join(run_dir, "run.json"), run)
    error, failures = None, []
    try:
        code = _run_and_analyze(local, run, run_dir, args, out)
    except Exception as exc:  # noqa: BLE001 - ends as ERROR in finish
        code, error = EXIT_ERROR, f"{type(exc).__name__}: {exc}"
    finally:
        failures = cleanup_run(local, run_dir, plan["run_id"], out)
    pcap = [f for f in failures if "raw capture" in f]
    return conclude(run_dir, code, out, error or (pcap[0] if pcap else None))


def _run_and_analyze(local, run, run_dir, args, out):
    from cq_local import AdapterError

    plan = run["plan"]
    try:
        local.commit_facts()
        local.run()
    except Interrupted as exc:
        run.update(error=f"interrupted ({exc}), so the capture is not analysed", facts=local.facts,
                   preconditions=local.preconditions)
        write_json(os.path.join(run_dir, "run.json"), local.ctx.scrub_obj(run))
        return EXIT_ERROR
    except AdapterError as exc:
        local.teardown_error = local.teardown_error or f"{type(exc).__name__}: {exc}"
    if local.window:
        run["window"] = list(local.window)
        if local.teardown_error:
            run["teardown_error"] = local.ctx.scrub(local.teardown_error)
            say(out, f"warning: the hold finished, then leaving failed: {run['teardown_error']}")
    elif local.teardown_error:
        run["error"] = local.ctx.scrub(local.teardown_error)
    run.update(facts=local.facts, preconditions=local.preconditions)
    write_json(os.path.join(run_dir, "run.json"), local.ctx.scrub_obj(run))
    if run.get("error"):
        say(out, f"{plan['run_id']}: ERROR {run['error']}")
        return EXIT_ERROR
    if STOP.requested():
        return EXIT_ERROR
    try:
        return analyze(run_dir, out)
    except Interrupted:
        return EXIT_ERROR


def drop_pages(run_dir, names, out):
    for name in names:
        try:
            os.remove(os.path.join(run_dir, name))
        except FileNotFoundError:
            pass
        except OSError as exc:
            say(out, f"warning: {name} still shows the earlier verdict: {exc.strerror or exc}")


def record_error(run_dir, why, out):
    try:
        record(run_dir, verdict="ERROR", error=why)
    except (OSError, ValueError, TypeError) as exc:
        say(out, f"warning: run.json does not say ERROR: {exc}")
    path = os.path.join(run_dir, "fidelity.json")
    if not os.path.exists(path):
        return
    try:
        doc = read_json(path)
        doc.setdefault("result", {}).update(verdict="ERROR", exit_code=EXIT_ERROR, interrupted=why)
        write_json(path, doc)
    except (OSError, ValueError, TypeError, AttributeError) as exc:
        say(out, f"warning: removing fidelity.json, which does not say ERROR: {exc}")
        drop_pages(run_dir, ("fidelity.json",), out)


def record_verdict(run_dir):
    """A completed analysis owns the verdict, so run.json takes it and loses an earlier error; returns what failed."""
    path = os.path.join(run_dir, "fidelity.json")
    if not os.path.exists(path):
        return None
    try:
        verdict = read_json(path)["result"]["verdict"]
        run = read_json(os.path.join(run_dir, "run.json"))
        run.pop("error", None)
        write_json(os.path.join(run_dir, "run.json"), dict(run, verdict=verdict))
    except (OSError, ValueError, TypeError, KeyError, AttributeError) as exc:
        return f"could not record the verdict in run.json: {type(exc).__name__}: {exc}"
    return None


def recorded_error(run_dir):
    try:
        return read_json(os.path.join(run_dir, "run.json")).get("error") or "the run ended with no result"
    except (OSError, ValueError, AttributeError):
        return "the run ended with no result"


def finish(run_dir, code, out, error=None):
    reasons = ([f"interrupted by {STOP.describe()}"] if STOP.requested() else []) + ([error] if error else [])
    if run_dir and not reasons:
        failed = recorded_error(run_dir) if code == EXIT_ERROR else record_verdict(run_dir)
        reasons = [failed] if failed else []
    if not reasons:
        return code
    why = "; ".join(reasons)
    if run_dir:
        drop_pages(run_dir, VERDICT_PAGES, out)
        record_error(run_dir, why, out)
    say(out, f"ERROR: {why}")
    return EXIT_ERROR


def conclude(run_dir, code, out, error=None):
    code = finish(run_dir, code, out, error)
    write_report(run_dir, out)
    if STOP.requested() and code != EXIT_ERROR:
        code = finish(run_dir, code, out)
        write_report(run_dir, out)
    return code


def write_report(run_dir, out):
    if not os.path.exists(os.path.join(run_dir, "fidelity.json")):
        return None
    try:
        import fidelity_report

        path = fidelity_report.write(run_dir)
    except Exception as exc:  # noqa: BLE001
        drop_pages(run_dir, ("report.html",), out)
        error = f"{type(exc).__name__}: {exc}"
        try:
            record(run_dir, report_error=error)
        except Exception:  # noqa: BLE001
            pass
        say(out, f"warning: no report.html: {error}")
        return None
    try:
        run = read_json(os.path.join(run_dir, "run.json"))
        if run.pop("report_error", None) is not None:
            write_json(os.path.join(run_dir, "run.json"), run)
    except (OSError, ValueError, AttributeError) as exc:
        say(out, f"warning: run.json keeps an old report_error: {exc}")
    say(out, f"report: {path}")
    return path


def cmd_report(run_dir, out=print):
    if not os.path.exists(os.path.join(run_dir, "fidelity.json")):
        out(f"{run_dir}: no fidelity.json, so there is no result to report")
        return EXIT_ERROR
    page = write_report(run_dir, out)
    if STOP.requested():
        say(out, f"interrupted by {STOP.describe()}")
    return 0 if page and not STOP.requested() else EXIT_ERROR


def hold_run_folder(run_dir):
    """Lock a folder of this harness before anything in it changes; returns what refused it."""
    if not owns_run_folder(run_dir):
        return f"{run_dir}: not a run folder of this harness, so it is left alone"
    held = lock_run(run_dir)
    if held is None:
        return f"{run_dir}: another process holds this run folder"
    HELD.append(held)
    return None


def record(run_dir, **fields):
    path = os.path.join(run_dir, "run.json")
    if os.path.exists(path):
        write_json(path, dict(read_json(path), **fields))


def cmd_retest(args, out=print):
    docs = []
    for d in (args.a, args.b):
        path = os.path.join(d, "fidelity.json")
        doc = read_json(path) if os.path.exists(path) else None
        verdict = ((doc or {}).get("result") or {}).get("verdict")
        if verdict in (None, "INVALID", "ERROR"):
            out(f"{d}: verdict {verdict or 'missing'}; a retest needs two analysed, valid runs")
            return EXIT_ERROR
        docs.append(doc["measures"])
    a, b = docs
    res = fidelity_tolerance.retest(a, b, args.tol)
    for r in res["rows"]:
        out(f"{r['participant']:14s} {r['direction']:4s} {r['kind']:16s} {r['a']:10.2f} {r['b']:10.2f} {r['unit']:5s} "
            f"{fidelity_tolerance._pct(r['diff']):>6s} {r['result']}")
    if res["missing"]:
        out(f"participants in only one run: {', '.join(res['missing'])}")
    if res["silent"]:
        out(f"participants with no traffic in a direction: {', '.join(res['silent'])}")
    out(f"retest at ±{args.tol:.0%}: {'AGREE' if res['agree'] else 'DIFFER'}")
    return 0 if res["agree"] else 1


def build_parser():
    p = _Parser(description=__doc__.splitlines()[0])
    sub = p.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run")
    r.add_argument("--scenario", default=DEFAULT_SCENARIO)
    r.add_argument("--out-dir", default=os.path.join(
        os.environ.get("CQ_RUNS_DIR") or os.path.join(os.path.expanduser("~"), "videocall-scale-runs"), "fidelity"))
    r.add_argument("--relay-container")
    r.add_argument("--capture-iface", default="eth0")
    r.add_argument("--min-free-gb", type=float, default=10.0)
    a = sub.add_parser("analyze")
    a.add_argument("run_dir")
    sub.add_parser("report").add_argument("run_dir")
    t = sub.add_parser("retest")
    t.add_argument("a")
    t.add_argument("b")
    t.add_argument("--tol", type=float, default=0.05)
    sub.add_parser("remote")
    return p


def main(argv=None):
    args = build_parser().parse_args(argv)
    try:
        STOP.install()
        if args.cmd == "remote":
            print(REMOTE_GAP)
            return EXIT_ERROR
        if args.cmd == "run":
            return cmd_run(args)
        if args.cmd in ("analyze", "report"):
            refused = hold_run_folder(args.run_dir)
            if refused:
                print(refused, file=sys.stderr)
                return EXIT_ERROR
        if args.cmd == "analyze":
            error = None
            try:
                code = analyze(args.run_dir)
            except Interrupted:
                code = EXIT_ERROR
            except Exception as exc:  # noqa: BLE001 - ends as ERROR in finish
                code, error = EXIT_ERROR, f"{type(exc).__name__}: {exc}"
            return conclude(args.run_dir, code, print, error)
        if args.cmd == "report":
            return cmd_report(args.run_dir)
        return finish(None, cmd_retest(args), print)
    except Exception as exc:  # noqa: BLE001 - an uncaught error would exit 1, which reads as FAIL
        print(f"ERROR: {type(exc).__name__}: {exc}", file=sys.stderr)
        return EXIT_ERROR


if __name__ == "__main__":
    sys.exit(main())
