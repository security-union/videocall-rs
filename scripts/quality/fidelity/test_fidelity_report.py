#!/usr/bin/env python3
import contextlib
import copy
import io
import html
import html.parser
import json
import os
import sys
import tempfile
import types
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))
sys.path.insert(0, HERE)

import fidelity_local  # noqa: E402
import fidelity_report as fr  # noqa: E402
import fidelity_run  # noqa: E402
import fidelity_tolerance as ft  # noqa: E402

_TMP = tempfile.TemporaryDirectory(prefix="fidelity-report-test-")
tempfile.tempdir = _TMP.name

with open(os.path.join(HERE, "testdata", "room10-invalid.json"), encoding="utf-8") as _fh:
    FIXTURE = json.load(_fh)


def doc():
    return copy.deepcopy(FIXTURE["fidelity"])


def run_doc():
    return copy.deepcopy(FIXTURE["run"])


def run_dir(fidelity=None, run=None):
    d = tempfile.mkdtemp()
    if fidelity is not False:
        fidelity_run.write_json(os.path.join(d, "fidelity.json"), fidelity or doc())
    fidelity_run.write_json(os.path.join(d, "run.json"), run or run_doc())
    return d


def quiet_main(argv):
    with contextlib.redirect_stdout(io.StringIO()):
        code = fidelity_run.main(argv)
    while fidelity_run.HELD:
        fidelity_run.HELD.pop().close()
    return code


def by_key(cells):
    return {a.key: c for a, c in zip(fr.AXES, cells)}


class Balanced(html.parser.HTMLParser):
    VOID = {"meta", "input", "circle", "line", "polygon", "polyline"}

    def __init__(self):
        super().__init__()
        self.stack, self.errors = [], []

    def handle_starttag(self, tag, attrs):
        if tag not in self.VOID:
            self.stack.append(tag)

    def handle_endtag(self, tag):
        if tag in self.VOID:
            return
        if not self.stack or self.stack.pop() != tag:
            self.errors.append(tag)


class ScoreTest(unittest.TestCase):
    def test_closeness_is_smaller_over_larger(self):
        self.assertEqual(fr.closeness(2, 4), 50)
        self.assertEqual(fr.closeness(4, 2), 50)
        self.assertEqual(fr.closeness(0, 0), 100)
        self.assertEqual(fr.closeness(0, 1.28), 0)

    def test_room10_run_axes(self):
        s = fr.scores(doc()["result"])
        got = {k: c["score"] for k, c in by_key(s["combined"]).items()}
        want = {"audio_pkts": 94.1, "video_pkts": 50.5, "status": 66.2, "key_rate": 88.9, "audio_data": 59.1,
                "video_data": 9.7, "key_size": 3.9, "video_recv": 44.9, "resend": 0.0}
        for key, value in want.items():
            self.assertAlmostEqual(got[key], value, delta=0.1, msg=key)
        self.assertEqual(by_key(s["combined"])["bad_net"]["state"], fr.NOT_MEASURED)
        self.assertAlmostEqual(s["summary"]["overall"], 46.4, delta=0.1)
        self.assertAlmostEqual(s["summary"]["groups"]["How often"], 74.9, delta=0.1)
        self.assertAlmostEqual(s["summary"]["groups"]["How much"], 29.4, delta=0.1)
        self.assertEqual(s["summary"]["groups"]["Reacting"], 0.0)

    def test_per_role_sums_and_states(self):
        s = fr.scores(doc()["result"])
        cams = by_key(s["per_role"]["cameras (camera, n=2)"])
        self.assertAlmostEqual(cams["video_pkts"]["bot"], 7.077 + 15.029 + 29.851, delta=0.01)
        self.assertAlmostEqual(cams["video_data"]["browser"], 118.9 + 231.6 + 607.3, delta=0.5)
        self.assertAlmostEqual(cams["key_size"]["browser"], 38.1, delta=0.1)
        self.assertEqual(cams["audio_pkts"]["state"], fr.NOT_APPLICABLE)
        talk = by_key(s["per_role"]["talkers (talker, n=2)"])
        self.assertEqual(talk["video_data"]["state"], fr.NOT_MEASURED)
        self.assertIn("640x480", talk["key_size"]["note"])
        self.assertEqual(s["roles"][0], "talkers (talker, n=2)")
        self.assertEqual(s["roles"][-2:], ["viewers (receive, n=1)", "vpviewers (receive, n=1)"])

    def test_rows_on_no_axis(self):
        result = doc()["result"]
        s = fr.scores(result)
        left = {(result["rows"][i]["measure"], result["rows"][i]["kind"]) for i in s["unscored"]}
        self.assertIn(("uplink packets/s", "LAYER_PREFERENCE"), left)
        self.assertIn(("keyframe size p90", "video layer 2"), left)
        self.assertIn(("room level", "-"), left)
        self.assertNotIn(("uplink packets/s", "RTT"), left)
        self.assertNotIn(("impairment arm", "-"), left)

    def test_keyframe_size_reads_the_top_video_layer_the_room_published(self):
        def row(kind, bot):
            return {"role": "c (camera, n=1)", "reference": "b (n=1)", "measure": "keyframe size p50", "kind": kind,
                    "unit": "KB", "bot": bot, "browser": 10.0, "tol": 0.3, "diff": 0.0, "result": "PASS", "note": ""}
        for rows, bot in (([row("video layer 0", 5.0)], 5.0),
                          ([row("video layer 0", 5.0), row("video layer 1", 8.0)], 8.0),
                          ([row("video layer 1", 8.0), row("video layer 0", 5.0)], 8.0)):
            cell = by_key(fr.scores({"rows": rows})["per_role"]["c (camera, n=1)"])["key_size"]
            self.assertEqual((cell["state"], cell["bot"]), (fr.SCORED, bot))
        self.assertIn(("keyframe size p50", "video layer 0"), {(r["measure"], r["kind"]) for r in (
            doc()["result"]["rows"][i] for i in fr.scores(doc()["result"])["unscored"])})

    def test_a_not_run_row_makes_a_scored_axis_not_measured(self):
        role = "cams (camera, n=1)"

        def row(measure, kind, result, bot):
            return {"role": role, "reference": "b (n=1)", "measure": measure, "kind": kind, "unit": "kbps", "bot": bot,
                    "browser": bot, "tol": 0.25, "diff": 0.0, "result": result, "note": "not compared"}
        rows = [row("uplink bytes/s", "video layer 0", "PASS", 5.0),
                row("uplink bytes/s and keyframe size", "video", "NOT RUN", None)]
        cell = by_key(fr.scores({"rows": rows})["per_role"][role])["video_data"]
        self.assertEqual((cell["state"], cell["score"]), (fr.NOT_MEASURED, None))

    def test_all_roles_says_how_many_roles_have_no_score(self):
        cells = fr.scores(doc()["result"])["combined"]
        self.assertIn("mean of 1 role; 1 not measured", fr.axis_list(cells, doc()["result"]["rows"]))

    def test_a_role_without_browsers_is_not_measured_not_zero(self):
        result = {"rows": [{"role": "solo (camera, n=1)", "reference": "-", "measure": "every measure", "kind": "-",
                            "unit": "", "bot": None, "browser": None, "tol": None, "diff": None,
                            "result": "NOT RUN", "note": "no browser of class camera in the room"}]}
        s = fr.scores(result)
        self.assertTrue(all(c["state"] == fr.NOT_MEASURED for c in s["per_role"]["solo (camera, n=1)"]))
        self.assertIsNone(s["summary"]["overall"])

    def test_headline_names_strong_and_weak(self):
        text = fr.headline(fr.scores(doc()["result"])["combined"])
        self.assertTrue(text.startswith("Closest to the browser: audio packets (94), keyframe rate (89)."), text)
        self.assertIn("Furthest: resend requests (0), keyframe size (4), video data (10).", text)
        self.assertIn("Not measured: adapts to a bad network.", text)


class RenderTest(unittest.TestCase):
    def test_invalid_banner_in_plain_words(self):
        page = html.unescape(fr.render(doc(), run_doc()))
        self.assertIn("These numbers can't be trusted", page)
        self.assertIn("The reference browsers were starved of CPU: they sent fewer video packets per second", page)
        self.assertIn("(a quiet machine sends at least 5.7/10.7/18.6 video packets/s on video layers 0/1/2; "
                      "bcam-00 sent 5.61/8.71/11.19 on video layers 0/1/2; btalk-00 sent", page)
        self.assertIn("its CPU monitor missed readings", page)
        self.assertIn('<body class="indicative">', page)
        self.assertIn("Usable for sizing:</b> no, the run is INVALID", page)

    def test_a_valid_fail_has_no_invalid_banner(self):
        d = doc()
        d["result"].update(verdict="FAIL", problems=[])
        page = html.unescape(fr.render(d, run_doc()))
        self.assertNotIn("can't be trusted", page)
        self.assertNotIn('class="indicative"', page)
        self.assertIn("Valid run: some rows are outside the tolerance", page)
        self.assertIn("no, FAIL on downlink bytes/s; uplink bytes/s video layer 0", page)
        self.assertIn("NOT RUN: uplink bytes/s and keyframe size for talkers", page)

    def test_starved_browsers_with_different_floors(self):
        lines = fr.plain_reasons([
            "a: video layer 0 at 4.00 packets/s, under the quiet baseline 5.7 (x); the browser was starved",
            "b: video layer 1 at 9.00 packets/s, under the quiet baseline 10.7 (x); the browser was starved"])
        self.assertEqual(lines, ["The reference browsers were starved of CPU: they sent fewer video packets per second "
                                 "than on a quiet machine, so the yardstick itself is off (a sent 4.00 on video layers "
                                 "0 (quiet: at least 5.7); b sent 9.00 on video layers 1 (quiet: at least 10.7))."])

    def test_each_cause_renders_its_own_sentence(self):
        for problem, sentence in (
                ("host resources over the hold: starved", "The machine was not shown to have spare CPU"),
                ("no affirmative host resource verdict for the hold", "The machine was not shown to have spare CPU"),
                ("capture: 1 TCP reassembly gaps", "The traffic recording at the server is incomplete"),
                ("the capture holds no WebSocket messages", "The traffic recording at the server is incomplete"),
                ("the capture dropped 2 packets in the kernel", "The traffic recording at the server is incomplete"),
                ("stopping the capture failed, so the pcap may be incomplete", "The traffic recording"),
                ("tcpdump printed no capture summary, so kernel drops are unknown", "The traffic recording"),
                ("a: traffic in 90% of the hold's seconds (needs 95%)", "a had traffic in only 90% of the test."),
                ("a: published video layers [0] at >= 1 pkt/s, expected [0, 1]", "a did not send every video layer"),
                ("1 user(s) outside this run used the relay during the hold", "Someone outside the test used"),
                ("2 of the hold's 9 messages ran on connections with no decoded user id", "could not be tied"),
                ("probe-00 exited during the hold (code 1)", "probe-00 stopped during the test."),
                ("the stack runs a and the harness b; their product code differs", "may not be running the same code"),
                ("the stack checkout has uncommitted product changes", "may not be running the same code"),
                ("the relay's checkout could not be read, so one commit cannot be verified", "may not be running the"),
                ("git diff of a and b exited 128, so their product code cannot be compared", "may not be running the"),
                ("git status of the stack checkout exited 128, so uncommitted product changes", "may not be running"),
                ("Rust role x (camera) has no browser of its class to compare", "The room's mix of bots and browsers"),
                ("browser class receive has no Rust role to compare", "The room's mix of bots and browsers"),
                ("nothing was compared: the room needs at least one Rust role", "The room's mix of bots and browsers"),
                ("Rust role x and its browsers received nothing in the hold", "received nothing during the test")):
            lines = fr.plain_reasons([problem])
            self.assertEqual(len(lines), 1, problem)
            self.assertIn(sentence, lines[0], problem)

    def test_received_nothing_is_not_blamed_on_the_mix(self):
        def part(fleet, role):
            return {"fleet": fleet, "role": role, "class": "receive", "up": {"HEALTH": {"pps": 1.0, "Bps": 10.0}},
                    "keyframes": {}, "down_total": {"Bps": 0.0}}
        result = ft.evaluate({"participants": {"a": part("rust", "bots"), "b": part("browser", "probe")}})
        self.assertEqual(fr.plain_reasons(result["problems"]),
                         ["Some bots and the browsers they are compared with received nothing during the test."])

    def test_a_silent_room_is_not_blamed_on_the_mix(self):
        def part(fleet, role):
            return {"fleet": fleet, "role": role, "class": "receive", "up": {}, "keyframes": {},
                    "down_total": {"Bps": 0.0}}
        silent = ft.evaluate({"participants": {"a": part("rust", "bots"), "b": part("browser", "probe")}})
        self.assertIn("nothing was compared: the median traffic of the Rust roles and their browsers was zero",
                      silent["problems"])
        self.assertEqual(fr.plain_reasons(silent["problems"]),
                         ["Some bots and the browsers they are compared with received nothing during the test."])
        no_browser = ft.evaluate({"participants": {"a": part("rust", "bots")}})
        self.assertIn("nothing was compared: the room needs at least one Rust role and a browser of its class",
                      no_browser["problems"])
        self.assertEqual(fr.plain_reasons(no_browser["problems"]),
                         ["The room's mix of bots and browsers does not allow a like-for-like comparison."])
        self.assertEqual(fr.plain_reasons(ft.evaluate({"participants": {}})["problems"]),
                         ["The room's mix of bots and browsers does not allow a like-for-like comparison."])

    def test_a_bar_sits_on_the_side_of_its_ratio_and_the_band_is_the_tolerance(self):
        self.assertEqual(fr.ratio_pos(1, 1), 50)
        self.assertEqual(fr.ratio_pos(1, 2), 37.5)
        self.assertEqual(fr.ratio_pos(2, 1), 62.5)
        self.assertEqual((fr.ratio_pos(1, 16), fr.ratio_pos(16, 1)), (0, 100))
        self.assertEqual((fr.ratio_pos(1, 1000), fr.ratio_pos(1000, 1)), (0, 100))
        row = {"result": "FAIL", "bot": 1.0, "browser": 2.0, "tol": 0.25}
        self.assertIn('class="band" style="left:44.8%;width:9.2%"', fr.bar(row))
        self.assertIn('style="left:37.5%;width:12.5%"', fr.bar(row))
        self.assertIn("left:clamp(7px,37.5%,calc(100% - 7px))", fr.bar(row))

    def test_a_score_is_weak_by_the_number_the_page_prints(self):
        def weak(bot):
            row = {"role": "r (receive, n=1)", "reference": "b (n=1)", "measure": "downlink bytes/s", "kind": "all",
                   "unit": "kbps", "bot": bot, "browser": 100.0, "tol": 0.25, "diff": 0.0, "result": "FAIL", "note": ""}
            return bool(fr.differences(fr.scores({"rows": [row]})))
        self.assertEqual(fr.WEAK_BELOW, 70)
        self.assertEqual([weak(b) for b in (59.0, 65.0, 69.4, 69.7, 70.0, 80.0)],
                         [True, True, True, False, False, False])
        self.assertIn("Axes scoring under 70 for a role.", fr.render(doc(), run_doc()))

    def test_the_score_line_claims_nothing_rounding_can_break(self):
        page = fr.render(doc(), run_doc())
        self.assertIn("out of 100 · higher is closer to the browser", page)
        self.assertNotIn("100 means", page.split("<footer>")[0])

    def test_the_page_offers_no_verdict_that_cannot_happen(self):
        self.assertNotIn("PASS", fr.VERDICT_TEXT)
        self.assertNotIn(".banner.pass", fr.STYLE)
        self.assertNotIn("the bots behave like the browsers", fr.render(doc(), run_doc()))

    def test_row_labels_stay_readable_to_a_screen_reader(self):
        self.assertNotIn(".r .lab{display:none}", fr.STYLE)
        self.assertIn(".r .lab{position:absolute", fr.STYLE)

    def test_a_cause_is_one_line_however_many_hit_it(self):
        lines = fr.plain_reasons([
            "capture: 3 TCP reassembly gaps", "capture: 1 messages failed to decode",
            "a: traffic in 90% of the hold's seconds (needs 95%)",
            "b: traffic in 80% of the hold's seconds (needs 95%)",
            "a: published video layers [0] at >= 1 pkt/s", "b: published video layers [0] at >= 1 pkt/s",
            "x exited during the hold (code 1)", "y exited during the hold (code 1)",
            "c: video layer 0 at 4.00 packets/s, under the quiet baseline 5.7 (x); the browser was starved",
            "new one", "new two"])
        self.assertEqual(lines[1:], [
            "The traffic recording at the server is incomplete, so some counts may be short.",
            "a, b had traffic in only 90%, 80% of the test.",
            "a, b did not send every video layer they should have.", "x, y stopped during the test.",
            "new one", "new two"])
        self.assertTrue(lines[0].startswith("The reference browsers were starved"))

    def test_traffic_with_no_user_id_in_plain_words(self):
        lines = fr.plain_reasons(["2 of the hold's 1402 messages ran on connections with no decoded user id"])
        self.assertEqual(lines, ["Some traffic during the test could not be tied to anyone, so someone outside it may "
                                 "have used the server."])

    def test_a_late_joiner_is_not_read_as_a_bad_recording(self):
        lines = fr.plain_reasons(["bt-00: traffic in 90% of the hold's seconds (needs 95%); it joined late, left or "
                                  "was never captured", "capture: 3 TCP reassembly gaps"])
        self.assertEqual(lines, ["bt-00 had traffic in only 90% of the test.",
                                 "The traffic recording at the server is incomplete, so some counts may be short."])

    def test_unknown_problem_is_shown_verbatim(self):
        self.assertEqual(fr.plain_reasons(["something new broke"]), ["something new broke"])

    def test_page_is_balanced_and_self_contained(self):
        page = fr.render(doc(), run_doc())
        p = Balanced()
        p.feed(page)
        p.close()
        self.assertEqual((p.errors, p.stack), ([], []))
        for needle in ("http://", "https://", "<script", "<link", "@import", "url("):
            self.assertNotIn(needle, page)
        self.assertEqual(page.count('class="radar"'), 1 + 4)
        self.assertIn('aria-label="All roles closeness to the browser out of 100: Audio packets 94;', page)
        self.assertIn("Adapts to a bad network not measured", page)

    def test_each_measure_names_the_browsers_it_was_compared_with(self):
        page = html.unescape(fr.render(doc(), run_doc()))
        self.assertIn(">uplink packets/s · against bcam (n=1) · tolerance ±15%<", page)
        self.assertIn(">keyframe size p50 · against bcam (n=1) 1280x720 · tolerance ±30%<", page)
        self.assertIn("<span>cameras (camera, n=2)</span>", page)

    def test_a_new_reference_inside_a_measure_gets_its_own_heading(self):
        def row(kind, reference):
            return {"role": "r (talker, n=1)", "reference": reference, "measure": "uplink bytes/s", "kind": kind,
                    "unit": "kbps", "bot": 1.0, "browser": 1.0, "tol": 0.25, "diff": 0.0, "result": "PASS", "note": ""}
        page = fr.row_table([row("audio", "b (n=2)"), row("video layer 0", "b (n=1) 1280x720")], ["r (talker, n=1)"])
        self.assertIn("uplink bytes/s · against b (n=2) · tolerance", page)
        self.assertIn("uplink bytes/s · against b (n=1) 1280x720 · tolerance", page)

    def test_a_weak_axis_says_what_differs_and_which_way(self):
        lines = dict(fr.differences(fr.scores(doc()["result"])))
        self.assertEqual(lines["Keyframe size"], "cameras send smaller keyframes than browsers")
        self.assertEqual(lines["Resend requests"], "talkers, cameras, viewers, vpviewers send fewer KEYFRAME_REQUEST "
                                                   "packets than browsers")
        self.assertEqual(lines["Video received"], "talkers, cameras receive more data than browsers; viewers receive "
                                                  "less data than browsers")
        self.assertNotIn("Keyframe rate", lines)
        self.assertEqual(next(iter(lines)), "Resend requests")
        page = html.unescape(fr.render(doc(), run_doc()))
        self.assertIn("<h2>Where the bots differ", page)
        self.assertIn("<b>Keyframe size</b>: cameras send smaller keyframes than browsers.", page)

    def test_both_directions_are_named_and_a_close_axis_has_no_line(self):
        def row(role, bot):
            return {"role": role, "reference": "b (n=1)", "measure": "downlink bytes/s", "kind": "all", "unit": "kbps",
                    "bot": bot, "browser": 100.0, "tol": 0.25, "diff": 0.0, "result": "FAIL", "note": ""}
        rows = [row("up (receive, n=1)", 300.0), row("down (receive, n=1)", 20.0), row("close (receive, n=1)", 95.0)]
        self.assertEqual(fr.differences(fr.scores({"rows": rows})), [
            ("Video received", "up receive more data than browsers; down receive less data than browsers")])
        self.assertEqual(fr.differences(fr.scores({"rows": rows[2:]})), [])
        d = doc()
        d["result"]["rows"] = rows[2:]
        self.assertNotIn("Where the bots differ", fr.render(d, run_doc()))

    def test_the_fixtures_not_run_note_is_what_the_table_writes(self):
        for measure, _, note in ft.NOT_RUN_ROWS:
            row = next(r for r in doc()["result"]["rows"] if r["measure"] == measure)
            self.assertEqual(row["note"], note)

    def test_absolute_paths_stay_off_the_page(self):
        d = doc()
        d["result"].update(verdict="ERROR", interrupted="OSError: [Errno 13] denied: '/home/someone/run/conns.json'")
        d["result"]["problems"] = ["could not read /srv/runs/x/samples.gz"]
        page = fr.render(d, run_doc())
        self.assertNotIn("/home/someone", page)
        self.assertNotIn("/srv/runs", page)
        self.assertIn("denied: &lt;path&gt;", page)

    def test_relative_paths_stay_off_the_page_and_other_text_stays_whole(self):
        for path, shown in (("'./runs/x/conns.json'", "<path>"), ("'runs/x/conns.json'", "<path>"),
                            ('"runs/x/conns.json"', "<path>"),
                            ("./x/relay.pcap", "<path>"), ("../x/relay.pcap", "<path>"),
                            ("runs/x/relay.pcap", "<path>"),
                            ("/srv/x/relay.pcap", "<path>"), ("capture/relay.pcap", "capture/relay.pcap"),
                            ("5.7/10.7/18.6", "5.7/10.7/18.6"), ("0/1/2", "0/1/2"), ("packets/s", "packets/s"),
                            ("</script>", "</script>")):
            self.assertEqual(fr.no_paths(f"text {path} (more)"), f"text {shown} (more)")
        self.assertEqual(fr.no_paths("the relay's a/b/c is the hold's"), "the relay's <path> is the hold's")
        self.assertEqual(fr.no_paths("wrote ~/x/y (full)"), "wrote <path> (full)")
        self.assertEqual(fr.no_paths("kinds HEALTH/VIEWPORT/LAYER_PREFERENCE here"),
                         "kinds HEALTH/VIEWPORT/LAYER_PREFERENCE here")
        self.assertEqual(fr.no_paths("in RUNS/x/y now"), "in <path> now")
        self.assertEqual(fr.no_paths("in 123/capture/relay.pcap now"), "in <path> now")
        self.assertEqual(fr.no_paths("in ./relay.pcap now"), "in <path> now")
        self.assertEqual(fr.no_paths("in A/B/C.LOG now"), "in <path> now")
        d = doc()
        d["result"].update(verdict="ERROR", interrupted="denied: 'runs/secret-a/conns.json'")
        d["result"]["problems"] = ["could not read runs/secret-b/samples.gz"]
        page = fr.render(d, run_doc())
        self.assertNotIn("secret-a", page)
        self.assertEqual(page.count("secret-b"), 0)

    def test_a_group_with_no_score_says_so_in_the_small_style(self):
        d = doc()
        d["result"]["rows"] = [r for r in d["result"]["rows"] if r["kind"] != "KEYFRAME_REQUEST"]
        self.assertIn('Reacting</span><b class="num off">not measured</b>', fr.render(d, run_doc()))

    def test_a_row_at_the_edge_of_the_scale_keeps_its_pin_on_the_track(self):
        row = next(r for r in doc()["result"]["rows"] if r["bot"] == 0 and r["browser"])
        self.assertIn("left:clamp(7px,0.0%,calc(100% - 7px))", fr.bar(row))

    def test_the_mapping_notes_what_the_video_axes_compare(self):
        page = fr.mapping_table(doc()["result"]["rows"], [], 0.9)
        self.assertIn("(same source geometry)", page)
        self.assertIn("(video was 90% of all downlink in this run)", page)
        self.assertIn("keyframe size p50: top video layer (same source geometry)", page)
        self.assertAlmostEqual(fr.video_share(doc()["measures"]), 0.90, delta=0.01)

    def test_no_identities_and_escaped_text(self):
        d, run = doc(), run_doc()
        d["result"]["problems"].append('<script>alert("x")</script>')
        page = fr.render(d, run)
        for p in run["plan"]["participants"]:
            self.assertNotIn(p["user_id"], page)
        self.assertNotIn(run["plan"]["meeting_id"], page)
        self.assertNotIn("<script>", page)
        self.assertIn("&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt;", page)


class WriterTest(unittest.TestCase):
    def outside_file(self):
        fd, path = tempfile.mkstemp()
        os.write(fd, b"keep me")
        os.close(fd)
        return path

    def assert_kept(self, path):
        with open(path, "rb") as fh:
            self.assertEqual(fh.read(), b"keep me")

    def test_a_symlink_at_the_old_temporary_name_is_not_followed(self):
        d, outside = run_dir(), self.outside_file()
        os.symlink(outside, os.path.join(d, "report.html.tmp"))
        fr.write(d)
        self.assert_kept(outside)

    def test_a_symlink_at_the_temporary_name_is_refused_not_followed(self):
        d, outside = run_dir(), self.outside_file()
        real = fr.secrets.token_hex
        self.addCleanup(setattr, fr.secrets, "token_hex", real)
        fr.secrets.token_hex = lambda _: "fixed"
        os.symlink(outside, os.path.join(d, "report.html.fixed.tmp"))
        with self.assertRaises(FileExistsError):
            fr.write(d)
        self.assert_kept(outside)

    def test_a_failed_replace_leaves_no_temporary_file(self):
        d = run_dir()
        real = os.replace
        self.addCleanup(setattr, os, "replace", real)

        def replace(*_):
            raise OSError("full")
        os.replace = replace
        with self.assertRaises(OSError):
            fr.write(d)
        self.assertEqual([n for n in os.listdir(d) if n.endswith(".tmp")], [])

    def test_the_page_keeps_the_usual_file_mode(self):
        old = os.umask(0o022)
        self.addCleanup(os.umask, old)
        self.assertEqual(os.stat(fr.write(run_dir())).st_mode & 0o777, 0o644)


class HarnessWiringTest(unittest.TestCase):
    def test_report_subcommand(self):
        d = run_dir()
        self.assertEqual(quiet_main(["report", d]), 0)
        with open(os.path.join(d, "report.html"), encoding="utf-8") as fh:
            self.assertIn("fid-room10-fixture", fh.read())
        self.assertEqual(quiet_main(["report", run_dir(fidelity=False)]), 3)

    def test_analyze_writes_the_report_and_a_failure_keeps_the_exit_code(self):
        real_analyze, real_write = fidelity_run.analyze, fr.write
        self.addCleanup(setattr, fidelity_run, "analyze", real_analyze)
        self.addCleanup(setattr, fr, "write", real_write)
        fidelity_run.analyze = lambda d, out=print: 4
        d = run_dir()
        self.assertEqual(quiet_main(["analyze", d]), 4)
        self.assertTrue(os.path.exists(os.path.join(d, "report.html")))

        def broken(*_):
            raise KeyError("rows")
        fr.write = broken
        self.assertEqual(quiet_main(["analyze", d]), 4)
        self.assertEqual(fidelity_run.read_json(os.path.join(d, "run.json"))["report_error"], "KeyError: 'rows'")
        fr.write = real_write
        self.assertEqual(quiet_main(["analyze", d]), 4)
        self.assertNotIn("report_error", fidelity_run.read_json(os.path.join(d, "run.json")))

    def test_run_writes_the_report_after_the_verdict(self):
        class Local:
            def __init__(self, plan, fid, run_dir, *rest):
                pass

            def preflight(self):
                return []

            def run_containers(self):
                return []

            def remove_containers(self):
                return []

        def analysed(local, run, run_dir, args, out):
            fidelity_run.write_json(os.path.join(run_dir, "fidelity.json"), doc())
            return 1
        real_local, real_inner, real_write = fidelity_local.LocalRun, fidelity_run._run_and_analyze, fr.write
        self.addCleanup(setattr, fidelity_local, "LocalRun", real_local)
        self.addCleanup(setattr, fidelity_run, "_run_and_analyze", real_inner)
        self.addCleanup(setattr, fr, "write", real_write)
        fidelity_local.LocalRun, fidelity_run._run_and_analyze = Local, analysed
        args = types.SimpleNamespace(scenario=fidelity_run.DEFAULT_SCENARIO, relay_container=None,
                                     capture_iface="eth0", min_free_gb=0)
        out = []
        args.out_dir = tempfile.mkdtemp()
        self.assertEqual(fidelity_run.cmd_run(args, out=out.append), 1)
        [d] = [os.path.join(args.out_dir, n) for n in os.listdir(args.out_dir)]
        self.assertTrue(os.path.exists(os.path.join(d, "report.html")))
        self.assertIn(f"report: {os.path.join(d, 'report.html')}", out)

        def broken(*_):
            raise ValueError("bad page")
        fr.write = broken
        args.out_dir = tempfile.mkdtemp()
        self.assertEqual(fidelity_run.cmd_run(args, out=out.append), 1)
        self.assertIn("warning: no report.html: ValueError: bad page", out)


if __name__ == "__main__":
    unittest.main()
