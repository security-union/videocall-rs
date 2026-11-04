#!/usr/bin/env python3

import contextlib
import csv
import errno
import functools
import gzip
import io
import json
import os
import re
import shutil
import signal
import struct
import sys
import subprocess
import tempfile
import threading
import time
import types
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.dirname(HERE))
sys.path.insert(0, HERE)

import fidelity_local  # noqa: E402
import fidelity_measures as fm  # noqa: E402
import fidelity_report  # noqa: E402
import fidelity_run  # noqa: E402
import fidelity_signals  # noqa: E402
import fidelity_tolerance as ft  # noqa: E402
import ws_capture  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(HERE)))
_TMP = tempfile.TemporaryDirectory(prefix="fidelity-test-")
tempfile.tempdir = _TMP.name
TOKEN = ".".join(["eyJhbGciOiJIUzI1NiJ9", "eyJzdWIiOiJ0ZXN0In0", "c2lnbmF0dXJl"])
RELAY, CLIENT = bytes([10, 0, 0, 2]), bytes([10, 0, 0, 9])


def varint(n):
    out = bytearray()
    while True:
        out.append((n & 0x7F) | (0x80 if n > 0x7F else 0))
        n >>= 7
        if not n:
            return bytes(out)


def pb(*fields):
    """Protobuf message from (field number, value): int as varint, bytes or str length-delimited, float as double."""
    out = b""
    for fn, v in fields:
        if isinstance(v, bool) or isinstance(v, int):
            out += varint(fn << 3) + varint(int(v))
        elif isinstance(v, float):
            out += varint(fn << 3 | 1) + struct.pack("<d", v)
        else:
            v = v.encode() if isinstance(v, str) else v
            out += varint(fn << 3 | 2) + varint(len(v)) + v
    return out


def vp9_key(width, height):
    """A VP9 profile-0 keyframe header carrying width x height."""
    bits = "10" + "00" + "0" + "0" + "11" + "".join(f"{b:08b}" for b in (0x49, 0x83, 0x42)) + "001" + "0"
    bits += f"{width - 1:016b}{height - 1:016b}"
    bits += "0" * (-len(bits) % 8)
    return bytes(int(bits[i:i + 8], 2) for i in range(0, len(bits), 8)) + b"\0" * 8


def video(user, layer, key, size=0, w=640, h=480):
    data = vp9_key(w, h) + b"\x55" * size if key else b"\x11" * (size or 10)
    media = pb((1, 1), (2, user), (3, data), (4, "key" if key else "delta"), (5, 1.5), (8, pb((1, 7), (3, w), (4, h))))
    return pb((1, 3), (2, user), (3, media), (4, 42), (5, layer), (6, 1))


def audio(user, seq):
    return pb((1, 3), (2, user), (3, pb((1, 2), (2, user), (3, b"\x01" * 40), (7, pb((5, seq))))))


def health(user):
    return pb((1, 6), (2, user), (3, b"\x08\x01"))


def ws_frame(payload, op=2, mask=None, fin=True):
    n = len(payload)
    head = bytes([(0x80 if fin else 0) | op])
    mbit = 0x80 if mask else 0
    if n < 126:
        head += bytes([mbit | n])
    elif n < 65536:
        head += bytes([mbit | 126]) + struct.pack(">H", n)
    else:
        head += bytes([mbit | 127]) + struct.pack(">Q", n)
    if mask:
        payload = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
        head += mask
    return head + payload


class Pcap:
    """A classic little-endian Ethernet pcap of one TCP connection, client port 40000 to relay port 8080."""

    def __init__(self, cport=40000):
        self.records, self.cport = [], cport
        self.seq = {"u": 1000, "d": 5000}

    def seg(self, t, d, payload=b"", flags=0x18, seq=None):
        s = self.seq[d] if seq is None else seq
        src, dst, sp, dp = (CLIENT, RELAY, self.cport, 8080) if d == "u" else (RELAY, CLIENT, 8080, self.cport)
        tcp = struct.pack(">HHIIBBHHH", sp, dp, s, 0, 5 << 4, flags, 65535, 0, 0) + payload
        ip = struct.pack(">BBHHHBBH4s4s", 0x45, 0, 20 + len(tcp), 0, 0, 64, 6, 0, src, dst) + tcp
        self.records.append((t, b"\0" * 12 + b"\x08\x00" + ip))
        if seq is None:
            self.seq[d] = s + len(payload) + (1 if flags & 0x02 else 0)
        return s

    def send(self, t, d, data, mss=1400):
        for i in range(0, len(data), mss):
            self.seg(t, d, data[i:i + mss])

    def open_ws(self, t):
        self.seg(t, "u", flags=0x02)
        self.seg(t, "d", flags=0x12)
        self.seg(t, "u", b"GET /lobby/u/room?token=SECRETTOKEN HTTP/1.1\r\nUpgrade: websocket\r\nCookie: session="
                 + TOKEN.encode() + b"\r\n\r\n")
        self.seg(t, "d", b"HTTP/1.1 101 Switching Protocols\r\n\r\n")

    def write(self, path):
        with open(path, "wb") as fh:
            fh.write(struct.pack("<IHHiIII", 0xA1B2C3D4, 2, 4, 0, 0, 65535, 1))
            for t, frame in self.records:
                fh.write(struct.pack("<IIII", int(t), int(round((t % 1) * 1e6)), len(frame), len(frame)) + frame)


def parse(pcap):
    d = tempfile.mkdtemp()
    path = os.path.join(d, "x.pcap")
    pcap.write(path)
    stats = ws_capture.parse_pcap(path, d)
    with gzip.open(os.path.join(d, "messages.csv.gz"), "rt", newline="") as fh:
        rows = list(csv.DictReader(fh))
    with open(os.path.join(d, "conns.json"), encoding="utf-8") as fh:
        conns = json.load(fh)
    return stats, rows, conns


MASK = b"\x01\x02\x03\x04"


class CaptureTest(unittest.TestCase):
    def test_decodes_frames_across_reordered_segments(self):
        p = Pcap()
        p.open_ws(100.0)
        key = ws_frame(video("u1@x", 2, True, size=200), mask=MASK)
        first, second = key[:30], key[30:]
        s1 = p.seq["u"]
        p.seg(100.2, "u", second, seq=s1 + len(first))
        p.seg(100.3, "u", first, seq=s1)
        p.seq["u"] = s1 + len(key)
        p.seg(100.4, "d", ws_frame(audio("u2@x", 9)))
        p.seg(100.5, "u", ws_frame(b"\x08\x06", op=9, mask=MASK))
        frag = health("u1@x")
        p.seg(100.6, "u", ws_frame(frag[:3], mask=MASK, fin=False) + ws_frame(frag[3:], op=0, mask=MASK))
        stats, rows, conns = parse(p)
        self.assertEqual((stats["decode_errors"], stats["reassembly_gaps"], stats["unsynced_bytes"]), (0, 0, 0))
        kinds = [(r["dir"], r["op"], r["ptype"], r["mtype"], r["layer"], r["frag"]) for r in rows]
        self.assertEqual(kinds, [("u", "binary", "MEDIA", "VIDEO", "2", "0"),
                                 ("d", "binary", "MEDIA", "AUDIO", "0", "0"),
                                 ("u", "ping", "", "", "", "0"), ("u", "binary", "HEALTH", "", "0", "1")])
        v = rows[0]
        self.assertEqual((v["ftype"], v["w"], v["h"], v["wuser"], v["ws_bytes"]), ("key", "640", "480", "u1@x",
                                                                                  str(len(key))))
        self.assertEqual(rows[1]["seq"], "9")
        self.assertEqual(conns[0]["users"], {"u1@x": 2})
        self.assertNotIn("SECRETTOKEN", json.dumps(conns))
        self.assertEqual(conns[0]["http_path"], "/lobby")

    def test_users_outside_the_run_are_not_written(self):
        p = Pcap()
        p.open_ws(1.0)
        p.seg(1.1, "u", ws_frame(audio("stranger@corp.test", 1), mask=MASK))
        p.seg(1.2, "d", ws_frame(audio("u2@x", 2)))
        p.seg(1.3, "d", ws_frame(audio("u2@elsewhere.test", 3)))
        d = tempfile.mkdtemp()
        p.write(os.path.join(d, "x.pcap"))
        ws_capture.parse_pcap(os.path.join(d, "x.pcap"), d, users={"u2@x"})
        with gzip.open(os.path.join(d, "messages.csv.gz"), "rt") as fh:
            text = fh.read()
        with open(os.path.join(d, "conns.json"), encoding="utf-8") as fh:
            text += fh.read()
        self.assertNotIn("stranger", text)
        self.assertNotIn("elsewhere", text)
        self.assertIn("u2@x", text)
        self.assertIn('"other"', text)

    def test_the_session_cookie_never_reaches_the_outputs(self):
        p = Pcap()
        p.open_ws(1.0)
        p.seg(1.1, "u", ws_frame(audio("u1@x", 1), mask=MASK))
        d = tempfile.mkdtemp()
        p.write(os.path.join(d, "x.pcap"))
        ws_capture.parse_pcap(os.path.join(d, "x.pcap"), d)
        with gzip.open(os.path.join(d, "messages.csv.gz"), "rt") as fh:
            text = fh.read()
        with open(os.path.join(d, "conns.json"), encoding="utf-8") as fh:
            text += fh.read()
        self.assertNotIn(TOKEN, text)
        self.assertNotIn("session=", text)

    def test_a_fragmented_message_counts_every_frame(self):
        p = Pcap()
        p.open_ws(1.0)
        msg = audio("u", 1)
        first, rest = ws_frame(msg[:5], mask=MASK, fin=False), ws_frame(msg[5:], op=0, mask=MASK)
        p.seg(1.1, "u", first + ws_frame(b"", op=9, mask=MASK) + rest)
        _, rows, _ = parse(p)
        whole = [r for r in rows if r["frag"] == "1"]
        self.assertEqual([(r["ws_bytes"], r["plen"], r["ptype"]) for r in whole],
                         [(str(len(first) + len(rest)), str(len(msg)), "MEDIA")])

    def test_a_syn_more_than_1_s_later_on_the_same_port_is_a_new_connection(self):
        p = Pcap()
        p.seg(1.0, "u", flags=0x02)
        p.seg(1.5, "u", flags=0x02, seq=1000)
        p.seg(1.5, "d", flags=0x12)
        p.seg(1.6, "u", b"GET /lobby/u/room HTTP/1.1\r\n\r\n")
        p.seg(1.6, "d", b"HTTP/1.1 101 Switching Protocols\r\n\r\n")
        p.seg(1.7, "u", ws_frame(audio("u", 1), mask=MASK))
        p.seq = {"u": 70000, "d": 90000}
        p.open_ws(3.0)
        p.seg(3.1, "u", ws_frame(audio("u", 2), mask=MASK))
        _, rows, conns = parse(p)
        self.assertEqual(len(conns), 2)
        self.assertEqual(sorted(r["conn"] for r in rows), ["0", "1"])

    def test_keep_user(self):
        self.assertEqual(ws_capture.keep_user(b"", {"a"}), "")
        self.assertEqual(ws_capture.keep_user(b"a@x", {"a@x", "r"}), "a@x")
        self.assertEqual(ws_capture.keep_user(b"r", {"a@x", "r"}), "r")
        for wire in (b"a@elsewhere", b"r@elsewhere", b"b@x", b"r-other", b"r\0", b"\0r", b"r\xff", b"\0"):
            self.assertEqual(ws_capture.keep_user(wire, {"a@x", "r"}), "other", wire)
        long_id = "u" * 5000
        self.assertEqual(ws_capture.keep_user(long_id.encode(), {long_id}), long_id)
        self.assertEqual(ws_capture.keep_user(long_id.encode() + b"x", {long_id}), "other")
        self.assertEqual(ws_capture.keep_user(b"b@x", None), "b@x")

    def test_the_pending_queue_is_bounded_by_bytes(self):
        stats = {"unsynced_bytes": 0, "reassembly_gaps": 0}
        limit = ws_capture.MAX_PENDING_BYTES
        ws_capture.MAX_PENDING_BYTES = 100
        try:
            h = ws_capture.Half({"id": 0}, "u", stats)
            h.segment(0.0, 999, 0x02, b"", None)
            h.segment(0.1, 1100, 0x18, b"x" * 60, None)
            self.assertFalse(h.lost)
            h.segment(0.1, 1000, 0x18, b"z" * 100, lambda *a: None)
            self.assertEqual((h.queued_bytes, h.pos), (0, 160))
            h.segment(0.1, 1100, 0x18, b"x" * 60, None)
            self.assertFalse(h.lost)
            h.segment(0.1, 1000 + 160 + 100, 0x18, b"x" * 60, None)
            h.segment(0.1, 1000 + 160 + 200, 0x18, b"y" * 41, None)
            self.assertEqual((h.lost, stats["reassembly_gaps"], h.queued_bytes), (True, 1, 0))
        finally:
            ws_capture.MAX_PENDING_BYTES = limit

    def test_the_capture_image_base_is_pinned_by_digest(self):
        with open(fidelity_local.CAPTURE_DOCKERFILE, encoding="utf-8") as fh:
            base = next(line for line in fh if line.startswith("FROM "))
        self.assertRegex(base, r"^FROM \S+@sha256:[0-9a-f]{64}\s*$")

    def parse_for(self, payload, users=("u1@x",)):
        """(stats, the only row, every parsed output as text) of one uplink message parsed for these run users."""
        p = Pcap()
        p.open_ws(1.0)
        p.send(1.1, "u", ws_frame(payload, mask=MASK))
        d = tempfile.mkdtemp()
        p.write(os.path.join(d, "x.pcap"))
        stats = ws_capture.parse_pcap(os.path.join(d, "x.pcap"), d, users=set(users))
        with gzip.open(os.path.join(d, "messages.csv.gz"), "rt", newline="") as fh:
            text = fh.read()
        [row] = list(csv.DictReader(io.StringIO(text)))
        with open(os.path.join(d, "conns.json"), encoding="utf-8") as fh:
            text += fh.read()
        return stats, row, text

    def test_a_message_that_fails_to_decode_keeps_none_of_its_fields(self):
        meta = pb((1, 7), (3, 640), (4, 480), (6, "low"))
        for what, ftype, rest in (("a varint timestamp", "delta", [(5, 5)]),
                                  ("a cut video metadata", "delta", [(1, 1), (5, 1.5), (8, b"\x0a\xff")]),
                                  ("a cut audio metadata", "key", [(1, 2), (5, 1.5), (7, b"\x28\xff")]),
                                  ("key data after populated metadata", "key", [(1, 1), (5, 1.5), (8, meta), (3, 7)])):
            with self.subTest(what):
                media = pb((2, "u1@x"), (4, ftype), *rest)
                stats, row, _ = self.parse_for(pb((1, 3), (2, "u1@x"), (3, media), (4, 42), (5, 2), (6, 1)))
                self.assertEqual((stats["decode_errors"], row["ptype"]), (1, "DECODE_ERROR"))
                framing = {"t", "conn", "dir", "ws_bytes", "op", "plen", "frag", "ptype"}
                self.assertEqual({c: v for c, v in row.items() if c not in framing and v}, {})

    def test_a_diagnostics_target_outside_the_run_is_not_written(self):
        inner = pb((3, "stranger@corp.test"), (5, 1))
        _, row, text = self.parse_for(pb((1, 5), (2, "u1@x"), (3, inner)))
        self.assertEqual((row["ptype"], row["iuser"]), ("DIAGNOSTICS", "other"))
        self.assertNotIn("stranger", text)

    def test_a_huge_varint_timestamp_is_a_decode_error(self):
        media = pb((1, 1), (2, "u1@x"), (5, 2 ** 62))
        stats, row, _ = self.parse_for(pb((1, 3), (2, "u1@x"), (3, media)))
        self.assertEqual((stats["decode_errors"], row["ptype"]), (1, "DECODE_ERROR"))

    def test_session_ids_sent_as_bytes_are_not_written(self):
        for wrapper in (pb((1, 6), (2, "u1@x"), (4, "stranger@corp.test")),
                        pb((1, 3), (2, "u1@x"), (3, pb((1, 6), (2, "u1@x"), (10, "stranger@corp.test"))))):
            stats, row, text = self.parse_for(wrapper)
            self.assertEqual((stats["decode_errors"], row["ptype"]), (1, "DECODE_ERROR"))
            self.assertNotIn("stranger", text)

    def test_a_varint_over_ten_bytes_is_a_decode_error(self):
        stats, row, _ = self.parse_for(pb((1, 6)) + varint(4 << 3) + b"\xff" * 5000 + b"\x01")
        self.assertEqual((stats["decode_errors"], row["ptype"], row["wsess"]), (1, "DECODE_ERROR", ""))
        _, row, _ = self.parse_for(pb((1, 6)) + varint(4 << 3) + b"\xff" * 9 + b"\x01")
        self.assertEqual((row["ptype"], row["wsess"]), ("HEALTH", str(2 ** 64 - 1)))
        stats, row, _ = self.parse_for(pb((1, 6)) + varint(4 << 3) + b"\xff" * 10 + b"\x01")
        self.assertEqual((stats["decode_errors"], row["ptype"]), (1, "DECODE_ERROR"))

    def test_a_message_that_fails_to_decode_does_not_count_for_its_user(self):
        p = Pcap()
        p.open_ws(1.0)
        p.seg(1.1, "u", ws_frame(audio("u1@x", 1), mask=MASK))
        p.seg(1.2, "u", ws_frame(pb((1, 3), (2, "u1@x"), (3, pb((5, 5)))), mask=MASK))
        stats, _, conns = parse(p)
        self.assertEqual((stats["decode_errors"], conns[0]["users"]), (1, {"u1@x": 1}))

    def test_an_id_parsed_without_a_run_list_can_be_written_and_read_back(self):
        for sent, kept in (("u\0x", "ux"), ("k" * 200000, "k" * ws_capture.MAX_UNCHECKED_ID_BYTES)):
            p = Pcap()
            p.open_ws(1.0)
            p.send(1.1, "u", ws_frame(pb((1, 6), (2, sent)), mask=MASK))
            stats, rows, _ = parse(p)
            self.assertEqual((stats["decode_errors"], rows[0]["wuser"]), (0, kept))

    def test_an_id_that_differs_from_a_run_id_at_each_site_is_other(self):
        alias = "u1@x\0"
        for what, payload, column in (("wrapper", pb((1, 6), (2, alias)), "wuser"),
                                      ("media", pb((1, 3), (2, "u1@x"), (3, pb((1, 2), (2, alias)))), "iuser"),
                                      ("diagnostics", pb((1, 5), (2, "u1@x"), (3, pb((3, alias), (5, 1)))), "iuser")):
            with self.subTest(what):
                _, row, text = self.parse_for(payload)
                self.assertEqual(row[column], "other")
                self.assertNotIn("\0", text)

    def test_a_frame_type_is_compared_whole(self):
        for sent in ("ke\0y", "key ", "KEY", "key\xff", "k" * 2000):
            with self.subTest(sent=sent):
                media = pb((1, 1), (2, "u1@x"), (3, vp9_key(640, 480)), (4, sent))
                stats, row, _ = self.parse_for(pb((1, 3), (2, "u1@x"), (3, media)))
                self.assertEqual((stats["decode_errors"], stats["unknown_text"]), (0, 1))
                self.assertEqual((row["ftype"], row["w"]), ("unknown", ""))

    def test_a_tier_and_a_keyframe_target_are_compared_whole(self):
        for what, inner, column in (("tier", pb((1, 1), (4, "delta"), (8, pb((6, "lo\0w")))), "tier"),
                                    ("keyframe target", pb((1, 6), (3, "VI\0DEO")), "ftype")):
            with self.subTest(what):
                stats, row, _ = self.parse_for(pb((1, 3), (2, "u1@x"), (3, inner)))
                self.assertEqual((row[column], stats["unknown_text"]), ("unknown", 1))

    def test_an_unknown_tier_counts_when_the_frame_type_is_known(self):
        media = pb((1, 1), (4, "delta"), (8, pb((6, "not-a-tier"))))
        stats, row, _ = self.parse_for(pb((1, 3), (2, "u1@x"), (3, media)))
        self.assertEqual((row["ftype"], row["tier"], stats["unknown_text"]), ("delta", "unknown", 1))

    def test_only_a_known_frame_type_or_tier_is_written(self):
        stranger = "stranger@corp.test"

        def media(media_type, sender, target):
            inner = pb((1, media_type), (2, target), (3, stranger), (4, stranger), (8, pb((6, TOKEN))))
            return pb((1, 3), (2, sender), (3, inner))

        for sender, target in (("", ""), ("u1@x", ""), ("u1@x", "u1@x"), (stranger, "u1@x"), ("", stranger)):
            for media_type in (1, 3, 6):
                with self.subTest(sender=sender, target=target, media_type=media_type):
                    stats, row, text = self.parse_for(media(media_type, sender, target))
                    self.assertEqual((row["ptype"], row["ftype"], stats["unknown_text"]), ("MEDIA", "unknown", 1))
                    self.assertEqual(row["tier"], "unknown" if media_type != 6 else "")
                    self.assertNotIn(stranger, text)
                    self.assertNotIn(TOKEN, text)

    def test_every_value_real_clients_send_is_kept(self):
        with open(os.path.join(ROOT, "videocall-aq", "src", "constants.rs"), encoding="utf-8") as fh:
            product_tiers = set(re.findall(r'label: "(\w+)"', fh.read()))
        self.assertIn("emergency", product_tiers)

        def rows_for(*cases):
            for inner, column, want in cases:
                stats, row, _ = self.parse_for(pb((1, 3), (2, "u1@x"), (3, inner)))
                self.assertEqual((row[column], stats["unknown_text"]), (want, 0), inner)

        rows_for((pb((1, 1), (4, "key"), (3, vp9_key(640, 480))), "ftype", "key"),
                 (pb((1, 1), (4, "delta")), "ftype", "delta"),
                 (pb((1, 3), (4, "key")), "ftype", "key"), (pb((1, 2), (4, "key")), "ftype", "key"),
                 (pb((1, 2), (4, "delta")), "ftype", "delta"), (pb((1, 4)), "ftype", ""), (pb((1, 5)), "ftype", ""),
                 (pb((1, 6), (3, "VIDEO")), "ftype", "VIDEO"), (pb((1, 6), (3, "SCREEN")), "ftype", "SCREEN"),
                 (pb((1, 1)), "tier", ""), *[(pb((1, 1), (8, pb((6, t)))), "tier", t) for t in product_tiers])

    def test_a_keyframe_keeps_its_size_and_dimensions(self):
        stats, row, _ = self.parse_for(video("u1@x", 2, True, size=50, w=1280, h=720))
        self.assertEqual((row["ftype"], row["w"], row["h"], stats["unknown_text"]), ("key", "1280", "720", 0))

    def http_conn(self, request, response):
        p = Pcap()
        p.seg(1.0, "u", flags=0x02)
        p.seg(1.0, "d", flags=0x12)
        p.seg(1.0, "u", request.encode() + b"\r\n\r\n")
        p.seg(1.0, "d", response.encode() + b"\r\n\r\n")
        return parse(p)[2][0]

    def test_of_the_http_exchange_only_a_known_route_and_the_status_number_are_kept(self):
        for path, route in (("/lobby?token=" + TOKEN, "/lobby"), ("/lobby/u/room", "/lobby"), ("/metrics", "/metrics"),
                            ("/version", "/version"), ("/", "/"), ("/stranger@corp.test?x=1", "unknown"),
                            ("/lobby\0", "unknown"), (TOKEN, "unknown")):
            with self.subTest(path=path):
                conn = self.http_conn(f"GET {path} HTTP/1.1", "HTTP/1.1 101 Switching Protocols")
                self.assertEqual((conn["http_path"], conn["http_status"], conn.get("not_ws")), (route, 101, None))
        for line, status in (("HTTP/1.1 401 Unauthorized " + TOKEN, 401), ("HTTP/1.1 404 Not Found", 404),
                             ("HTTP/1.1 200", 200), ("HTTP/1.1 400 101 x", 400), ("nonsense " + TOKEN, None)):
            with self.subTest(line=line):
                conn = self.http_conn("GET /lobby HTTP/1.1", line)
                self.assertEqual((conn["http_status"], conn["not_ws"]), (status, True))
                self.assertNotIn(TOKEN, json.dumps(conn))

    def test_extended_lengths(self):
        p = Pcap()
        p.open_ws(1.0)
        for size in (300, 70000):
            p.send(1.1, "d", ws_frame(video("u", 0, False, size=size)))
        _, rows, _ = parse(p)
        self.assertEqual([r["ptype"] for r in rows], ["MEDIA", "MEDIA"])
        self.assertGreater(int(rows[1]["ws_bytes"]), 70000)

    def test_connection_without_syn_is_counted_not_decoded(self):
        p = Pcap()
        p.seg(1.0, "u", ws_frame(audio("u", 1), mask=MASK))
        stats, rows, _ = parse(p)
        self.assertEqual(rows, [])
        self.assertGreater(stats["unsynced_bytes"], 0)

    def test_garbage_binary_is_a_decode_error(self):
        p = Pcap()
        p.open_ws(1.0)
        p.seg(1.1, "u", ws_frame(b"\x0f\xff\xff", mask=MASK))
        stats, rows, _ = parse(p)
        self.assertEqual((stats["decode_errors"], rows[0]["ptype"]), (1, "DECODE_ERROR"))

    def test_gap_and_cut_frame_are_counted_at_eof(self):
        p = Pcap()
        p.open_ws(1.0)
        frame = ws_frame(video("u", 0, False, size=3000), mask=MASK)
        s = p.seq["u"]
        p.seg(1.1, "u", frame[:1000], seq=s)
        p.seg(1.3, "u", frame[2000:], seq=s + 2000)
        stats, rows, _ = parse(p)
        self.assertEqual((rows, stats["eof_pending_segments"]), ([], 1))
        p = Pcap()
        p.open_ws(1.0)
        p.seg(1.1, "u", ws_frame(audio("u", 1), mask=MASK) + frame[:500])
        stats, rows, _ = parse(p)
        self.assertEqual((len(rows), stats["eof_partial_bytes"]), (1, 500))

    def test_retransmission_covering_a_pending_segment_is_not_a_gap(self):
        p = Pcap()
        p.open_ws(1.0)
        frame = ws_frame(video("u", 0, False, size=3000), mask=MASK)
        s0 = p.seq["u"]
        p.seg(1.1, "u", frame[1000:2000], seq=s0 + 1000)
        p.seg(1.2, "u", frame[:2500], seq=s0)
        p.seg(1.3, "u", frame[2500:], seq=s0 + 2500)
        stats, rows, _ = parse(p)
        self.assertEqual((stats["eof_pending_segments"], len(rows)), (0, 1))

    def test_partially_overlapping_pending_segment_is_fed(self):
        p = Pcap()
        p.open_ws(1.0)
        frame = ws_frame(video("u", 0, False, size=3000), mask=MASK)
        s0 = p.seq["u"]
        p.seg(1.1, "u", frame[1000:], seq=s0 + 1000)
        p.seg(1.2, "u", frame[:1500], seq=s0)
        stats, rows, _ = parse(p)
        self.assertEqual((stats["eof_pending_segments"], stats["eof_partial_bytes"], len(rows)), (0, 0, 1))

    def test_control_frames_do_not_hide_an_unfinished_fragmented_message(self):
        p = Pcap()
        p.open_ws(1.0)
        p.seg(5.0, "u", ws_frame(audio("u", 1)[:5], mask=MASK, fin=False))
        p.seg(6.0, "u", ws_frame(audio("u", 1)[5:9], op=0, mask=MASK, fin=False))
        p.seg(11.0, "u", ws_frame(b"", op=9, mask=MASK))
        p.seg(11.5, "u", ws_frame(b"\x03\xe8", op=8, mask=MASK))
        stats, rows, _ = parse(p)
        self.assertEqual(stats["eof_partial_messages"], [[6.0, 9]])
        p = Pcap()
        p.open_ws(1.0)
        p.seg(5.0, "u", ws_frame(audio("u", 1)[:5], mask=MASK, fin=False))
        p.seg(11.0, "u", ws_frame(b"", op=9, mask=MASK))
        stats, rows, _ = parse(p)
        self.assertEqual(stats["eof_partial_messages"], [[5.0, 5]])
        self.assertIn("inside the hold", fidelity_run.capture_problems(stats, (0, 10))[0])

    def test_duplicate_pending_segments_do_not_exhaust_the_queue(self):
        p = Pcap()
        p.open_ws(1.0)
        frame = ws_frame(audio("u", 1), mask=MASK)
        s0 = p.seq["u"]
        for _ in range(ws_capture.MAX_PENDING_SEGMENTS + 1):
            p.seg(1.1, "u", frame[10:], seq=s0 + 10)
        p.seg(1.1, "u", frame[10:12], seq=s0 + 10)
        p.seg(1.2, "u", frame[:10], seq=s0)
        stats, rows, _ = parse(p)
        self.assertEqual((stats["reassembly_gaps"], stats["eof_pending_segments"], len(rows)), (0, 0, 1))

    def test_pending_segments_are_kept_once_per_offset(self):
        stats = {"unsynced_bytes": 0, "reassembly_gaps": 0}
        limit = ws_capture.MAX_PENDING_SEGMENTS
        ws_capture.MAX_PENDING_SEGMENTS = 5
        try:
            h = ws_capture.Half({"id": 0}, "u", stats)
            h.segment(0.0, 999, 0x02, b"", None)
            for _ in range(10):
                h.segment(0.1, 1010, 0x18, b"abcd", None)
            self.assertEqual((len(h.pending), len(h.queued)), (1, 1))
            for n in range(5, 12):
                h.segment(0.2, 1010, 0x18, b"x" * n, None)
            self.assertEqual((h.lost, stats["reassembly_gaps"], len(h.queued), len(h.pending)), (False, 0, 1, 1))
            h.segment(0.3, 1010, 0x18, b"y" * 3, None)
            self.assertEqual((len(h.pending), h.queued[1010 - 1000]), (1, b"x" * 11))
        finally:
            ws_capture.MAX_PENDING_SEGMENTS = limit

    def test_a_gap_with_two_copies_counts_once(self):
        p = Pcap()
        p.open_ws(1.0)
        frame = ws_frame(audio("u", 1), mask=MASK)
        s0 = p.seq["u"]
        p.seg(1.1, "u", frame[10:12], seq=s0 + 10)
        p.seg(1.2, "u", frame[10:20], seq=s0 + 10)
        stats, rows, _ = parse(p)
        self.assertEqual((stats["eof_pending_segments"], rows), (1, []))

    def test_covered_segments_are_dropped_above_256_pending(self):
        p = Pcap()
        p.open_ws(1.0)
        frame = ws_frame(video("u", 0, False, size=4000), mask=MASK)
        s0 = p.seq["u"]
        for k in range(1, 300):
            p.seg(1.1, "u", frame[k * 10:k * 10 + 10], seq=s0 + k * 10)
        p.seg(1.2, "u", frame, seq=s0)
        stats, rows, _ = parse(p)
        self.assertEqual((stats["eof_pending_segments"], stats["reassembly_gaps"], len(rows)), (0, 0, 1))

    def test_incomplete_message_is_timed(self):
        p = Pcap()
        p.open_ws(1.0)
        p.seg(5.0, "u", ws_frame(audio("u", 1), mask=MASK)[:20])
        p.seg(5.1, "u", flags=0x11)
        stats, rows, _ = parse(p)
        self.assertEqual(stats["eof_partial_messages"], [[5.0, 20]])

    def test_not_a_pcap(self):
        with tempfile.NamedTemporaryFile(suffix=".pcap") as fh:
            fh.write(b"nope" * 10)
            fh.flush()
            with self.assertRaises(ValueError):
                list(ws_capture.read_pcap(fh.name))

    def test_vp9_dims(self):
        self.assertEqual(ws_capture.vp9_key_dims(vp9_key(1280, 720)), (1280, 720))
        self.assertEqual(ws_capture.vp9_key_dims(b"\x00" * 16), (0, 0))


class ProtoDriftTest(unittest.TestCase):
    """The parser's field numbers are read from the .proto files, so a renumbering fails here."""

    def source(self, proto):
        with open(os.path.join(ROOT, "protobuf", "types", proto), encoding="utf-8") as fh:
            return re.sub(r"//[^\n]*", "", fh.read())

    def fields(self, proto, message):
        text = self.source(proto)
        body = text[text.index(f"message {message} {{"):]
        body = body[:body.index("\n}")]
        return {m.group(1): int(m.group(2)) for m in re.finditer(r"^\s*(?:optional\s+)?[\w.]+\s+(\w+)\s*=\s*(\d+);",
                                                                 body, re.M)}

    def enum(self, proto, name):
        text = self.source(proto)
        body = text[text.index(f"enum {name} {{"):]
        return {int(m.group(2)): m.group(1) for m in re.finditer(r"^\s*(\w+)\s*=\s*(\d+);", body[:body.index("}")],
                                                                 re.M)}

    def test_field_numbers(self):
        for consts, proto, msg in ((ws_capture.WRAPPER_FIELDS, "packet_wrapper.proto", "PacketWrapper"),
                                   (ws_capture.MEDIA_FIELDS, "media_packet.proto", "MediaPacket"),
                                   (ws_capture.VIDEO_META_FIELDS, "media_packet.proto", "VideoMetadata"),
                                   (ws_capture.AUDIO_META_FIELDS, "media_packet.proto", "AudioMetadata"),
                                   (ws_capture.HEARTBEAT_FIELDS, "media_packet.proto", "HeartbeatMetadata"),
                                   (ws_capture.DIAGNOSTICS_FIELDS, "diagnostics_packet.proto", "DiagnosticsPacket")):
            got = self.fields(proto, msg)
            for name, number in consts.items():
                self.assertEqual(got.get(name), number, f"{msg}.{name}")

    def proto_wire(self, proto, message, field):
        text = self.source(proto)
        body = text[text.index(f"message {message} {{"):]
        kind = re.search(rf"^\s*(?:optional\s+)?([\w.]+)\s+{field}\s*=", body, re.M).group(1).rsplit(".", 1)[-1]
        if kind in ("uint32", "uint64", "bool") or f"enum {kind} " in text + self.source("media_packet.proto"):
            return ws_capture.VARINT
        if kind == "double":
            return ws_capture.FIXED64
        self.assertTrue(kind in ("string", "bytes") or f"message {kind} " in text, f"{message}.{field} is {kind}")
        return ws_capture.LENGTH

    def test_every_field_read_has_the_protos_wire_type_and_any_other_is_a_decode_error(self):
        def encoded(number, wire, varint_value=1):
            value = {ws_capture.VARINT: varint(varint_value), ws_capture.FIXED64: b"\0" * 8,
                     ws_capture.LENGTH: varint(0)}[wire]
            return varint(number << 3 | wire) + value

        read_when = {"audio_metadata": 2, "video_metadata": 1, "heartbeat_metadata": 4, "frame_type": 1}
        cases = (("packet_wrapper.proto", "PacketWrapper", ws_capture.WRAPPER_FIELDS, lambda f, _: f),
                 ("media_packet.proto", "MediaPacket", ws_capture.MEDIA_FIELDS,
                  lambda f, field: pb((1, 3), (3, f + pb((1, read_when.get(field, 6)))))),
                 ("media_packet.proto", "VideoMetadata", ws_capture.VIDEO_META_FIELDS,
                  lambda f, _: pb((1, 3), (3, pb((1, 1), (8, f))))),
                 ("media_packet.proto", "AudioMetadata", ws_capture.AUDIO_META_FIELDS,
                  lambda f, _: pb((1, 3), (3, pb((1, 2), (7, f))))),
                 ("media_packet.proto", "HeartbeatMetadata", ws_capture.HEARTBEAT_FIELDS,
                  lambda f, _: pb((1, 3), (3, pb((1, 4), (9, f))))),
                 ("diagnostics_packet.proto", "DiagnosticsPacket", ws_capture.DIAGNOSTICS_FIELDS,
                  lambda f, _: pb((1, 5), (3, f))))
        for proto, message, table, wrap in cases:
            for field, number in table.items():
                right = self.proto_wire(proto, message, field)
                ws_capture.decode_wrapper(wrap(encoded(number, right), field), {})
                for wrong in {ws_capture.VARINT, ws_capture.FIXED64, ws_capture.LENGTH} - {right}:
                    with self.subTest(message=message, field=field, wire=wrong):
                        with self.assertRaises(ValueError):
                            ws_capture.decode_wrapper(wrap(encoded(number, wrong), field), {})
                        if wrong == ws_capture.VARINT:
                            with self.assertRaises(ValueError):
                                ws_capture.decode_wrapper(wrap(encoded(number, wrong, 0), field), {})

    def product(self, *path):
        with open(os.path.join(ROOT, *path), encoding="utf-8") as fh:
            return fh.read()

    def test_the_known_values_cover_what_the_product_sends(self):
        wrappers = self.product("videocall-client", "src", "wrappers.rs")
        frames = set(re.findall(r'write!\(f, "(\w+)"\)', wrappers))
        frames |= set(re.findall(r'frame_type: (?:if is_key \{ )?"(\w+)"(?: \} else \{ "\w+" \})?', self.product(
            "bot", "src", "audio_producer.rs")))
        frames |= set(re.findall(r'"(\w+)"', re.search(r"frame_type: if is_key (.*?)\.to_string", self.product(
            "bot", "src", "video_producer.rs")).group(1)))
        self.assertEqual(ws_capture.FRAME_TYPES, {b""} | {f.encode() for f in frames},
                         "must match the product's values")
        targets = set(re.findall(r'b"([A-Z]+)"\.to_vec\(\)', self.product("videocall-client", "src", "decode",
                                                                           "peer_decode_manager.rs")))
        targets |= set(re.findall(r'data: b"([A-Z]+)"\.to_vec\(\)',
                                  self.product("bot", "src", "keyframe_requester.rs")))
        self.assertEqual(ws_capture.KEYFRAME_TARGETS, {b""} | {t.encode() for t in targets},
                         "must match the product's values")
        tiers = set(re.findall(r'label: "(\w+)"', self.product("videocall-aq", "src", "constants.rs")))
        self.assertIn("emergency", tiers)
        self.assertEqual(ws_capture.TIERS, {b""} | {t.encode() for t in tiers}, "must match the product's values")
        routes = re.findall(r'#\[get\("(/[^"]*)"\)\]|\.route\("(/[^"]*)"', self.product(
            "actix-api", "src", "bin", "websocket_server.rs") + self.product("actix-api", "src", "lobby.rs"))
        first_segments = {"/" + (a or b).lstrip("/").split("/", 1)[0] for a, b in routes}
        self.assertIn("/lobby", first_segments)
        self.assertEqual(ws_capture.ROUTES, {"/"} | first_segments, "must match the relay's routes")

    def test_enums(self):
        wrapper = self.enum("packet_wrapper.proto", "PacketType")
        for n, name in ws_capture.PTYPE.items():
            if n:
                self.assertEqual(wrapper.get(n), name)
        media = self.enum("media_packet.proto", "MediaType")
        for n, name in ws_capture.MTYPE.items():
            if n:
                self.assertEqual(media.get(n), name)


def part(name, fleet, role, talker=False, camera=False):
    return {"user_id": f"r-{name}" + ("@bots-app.local" if fleet == "browser" else ""), "name": name, "fleet": fleet,
            "role": role, "talker": talker, "publishes": {"camera": camera, "mic": talker, "screen": False}}


def row(t, conn, d, nbytes, ptype="MEDIA", mtype="", layer=0, ftype="", op="binary", wuser=""):
    return {"t": str(t), "conn": str(conn), "dir": d, "ws_bytes": str(nbytes), "op": op, "ptype": ptype,
            "mtype": mtype, "layer": str(layer), "ftype": ftype, "wuser": wuser}


class MeasuresTest(unittest.TestCase):
    def test_room_lines_keep_this_room_and_the_series_without_a_room(self):
        lines = ['relay_room_bytes_total{room="mine",direction="inbound"} 5', 'relay_nats_events_total 7',
                 'relay_session_drops_total{kind="a,room=\\"their\\"",room="mine"} 1',
                 'relay_room_bytes_total{room="their-meeting",direction="inbound"} 9', "# HELP relay_x y",
                 'other_total{room="mine"} 1', 'relay_room_bytes_total{room="their-meeting",bedroom="mine"} 9',
                 'relay_room_bytes_total{bedroom="mine",room="their-meeting"} 9',
                 'relay_room_bytes_total{room="mine",room="their-meeting"} 9',
                 'relay_room_bytes_total{room="mine-2"} 9',
                 'relay_room_bytes_total{note="room=\\"mine\\"",room="x"} 9',
                 'relay_room_bytes_total{not parsed} 9', 'relay_room_bytes_total{room="mine"} 9 {room="their"}',
                 'relay_room_bytes_total{room="mine"}{room="their"} 9', 'relay_room_bytes_total 9 {room="their"}',
                 'relay_room_bytes_total{room="mine"} 9 1700000000 extra']
        kept = ['relay_room_bytes_total{room="mine"} 9 1700000000', 'relay_room_bytes_total{room="mine",} +Inf']
        self.assertEqual(fm.room_lines(lines + kept, "mine"), lines[:3] + kept)

    def test_a_series_with_no_metric_name_is_not_in_the_room(self):
        self.assertFalse(fm.in_room('{room="mine"} 1', "mine"))

    def test_a_video_layer_above_the_ladder_counts_as_other(self):
        video = {"op": "binary", "ptype": "MEDIA", "mtype": "VIDEO"}
        self.assertEqual(fm.kind_of(dict(video, layer="2")), "video_L2")
        self.assertEqual(fm.kind_of(dict(video, layer="3")), "other")

    def test_exactly_the_limit_of_traffic_with_no_user_id_is_not_judged(self):
        rows = self.rows(0, 100)[:999] + [row(1, 4, "d", 100)]
        m = fm.measure(rows, self.CONNS + [{"id": 4}], self.PARTS, (0, 100), {})
        self.assertEqual(m["unidentified_messages_in_window"], 1)
        self.assertFalse(any("no decoded user id" in p for p in m["problems"]), m["problems"])

    def test_traffic_with_no_user_id_is_counted_and_judged_over_its_share(self):
        conns = self.CONNS + [{"id": 4}]
        rows = self.rows(0, 100)
        self.assertEqual(len(rows), 1400)
        one, two = [row(1, 4, "d", 100)], [row(1, 4, "d", 100), row(2, 4, "d", 100)]
        late = [row(500, 4, "d", 100)] * 5
        m = fm.measure(rows + one + late, conns, self.PARTS, (0, 100), {})
        self.assertEqual((m["unidentified_messages_in_window"], m["problems"]), (1, []))
        m = fm.measure(rows + two, conns, self.PARTS, (0, 100), {})
        self.assertEqual(m["unidentified_messages_in_window"], 2)
        self.assertEqual(m["problems"], ["2 of the hold's 1402 messages ran on connections with no decoded user id: "
                                         "over 0.1%, so traffic outside the run is not ruled out"])
        m = fm.measure(rows + two + self.rows(200, 100), conns, self.PARTS, (0, 100), {})
        self.assertEqual(len(m["problems"]), 1, "messages outside the hold are not in the share")
        m = fm.measure(rows + [row(1, 2, "u", 100)], conns, self.PARTS, (0, 100), {})
        self.assertEqual((m["unidentified_messages_in_window"], m["unmapped_messages_in_window"]), (0, 1))

    def test_relay_deltas_match_the_room_label_exactly(self):
        own, crafted = 'relay_room_bytes_total{room="mine"}', 'relay_room_bytes_total{room="their",bedroom="mine"}'
        samples = [(0.0, {own: 0.0, crafted: 0.0}), (10.0, {own: 10.0, crafted: 10.0})]
        res = fm.relay_deltas(samples, "mine", (0.0, 10.0))
        self.assertEqual(res["per_s"], {own: 1.0})

    PARTS = [part("talk-00", "rust", "talk", True, True), part("bt-00", "browser", "bt", True, True)]
    CONNS = [{"id": 0, "users": {"r-talk-00": 5}}, {"id": 1, "users": {"r-bt-00@x.test": 5}},
             {"id": 2, "users": {"someone-else": 3}}, {"id": 3, "not_ws": True}]

    def rows(self, t0=0, secs=10):
        out = []
        for s in range(t0, t0 + secs):
            for conn in (0, 1):
                out += [row(s + 0.1, conn, "u", 1000, mtype="VIDEO", layer=layer, ftype="key" if layer == 2 else "")
                        for layer in (0, 1, 2)]
                out += [row(s + 0.2, conn, "u", 100, mtype="AUDIO"), row(s + 0.3, conn, "d", 500, mtype="AUDIO"),
                        row(s + 0.4, conn, "u", 50, ptype="HEALTH"), row(s + 0.5, conn, "u", 6, op="pong")]
        return out

    def test_rates_per_kind_and_window(self):
        rows = self.rows() + [row(50, 0, "u", 99999, mtype="VIDEO")]
        m = fm.measure(rows, self.CONNS, self.PARTS, (0, 10), {"talk-00": 3, "bt-00": 3})
        t = m["participants"]["talk-00"]
        self.assertEqual(m["problems"], [])
        self.assertAlmostEqual(t["up"]["video_L2"]["pps"], 1.0)
        self.assertAlmostEqual(t["up"]["audio"]["Bps"], 100.0)
        self.assertAlmostEqual(t["up"]["other"]["pps"], 1.0)
        self.assertAlmostEqual(t["up_total"]["pps"], 6.0)
        self.assertAlmostEqual(t["down_total"]["Bps"], 500.0)
        self.assertEqual(t["keyframes"]["2"]["sizes"], [1000] * 10)
        self.assertEqual((t["video_layers"], t["class"], t["coverage"]), ([0, 1, 2], "talker", 1.0))

    def test_expected_video_layers_are_matched_by_index(self):
        rows = [r for r in self.rows() if not (r["conn"] == "1" and r["layer"] == "0" and r["mtype"] == "VIDEO")]
        m = fm.measure(rows, self.CONNS, self.PARTS, (0, 10), {"bt-00": 2})
        self.assertIn("bt-00: published video layers [1, 2] at >= 1 pkt/s, expected [0, 1]", m["problems"])

    def test_coverage_boundary(self):
        def run(seconds):
            rows = [r for r in self.rows(0, 100) if r["conn"] == "0" or float(r["t"]) < seconds]
            return fm.measure(rows, self.CONNS, self.PARTS, (0, 100), {})["problems"]
        self.assertFalse(any("bt-00: traffic" in p for p in run(95)))
        self.assertTrue(any("bt-00: traffic in 94%" in p for p in run(94)))

    def test_problems(self):
        rows = [r for r in self.rows() if not (r["conn"] == "1" and r["layer"] == "2" and r["mtype"] == "VIDEO")]
        rows = [r for r in rows if not (r["conn"] == "0" and float(r["t"]) >= 5)]
        rows.append(row(3, 2, "u", 10, mtype="AUDIO"))
        m = fm.measure(rows, self.CONNS, self.PARTS, (0, 10), {"talk-00": 3, "bt-00": 3})
        text = " | ".join(m["problems"])
        self.assertIn("talk-00: traffic in 50%", text)
        self.assertIn("bt-00: published video layers [0, 1]", text)
        self.assertIn("1 user(s) outside this run", text)
        self.assertNotIn("someone-else", text)

    def test_class_mapping(self):
        self.assertEqual(fm.class_of(part("a", "rust", "r", talker=True, camera=False)), "talker")
        self.assertEqual(fm.class_of(part("a", "rust", "r", camera=True)), "camera")
        self.assertEqual(fm.class_of(part("a", "rust", "r")), "receive")

    def test_downlink_keyframes_and_minority_users_are_not_counted(self):
        rows = self.rows() + [row(2.5, 0, "d", 77777, mtype="VIDEO", layer=2, ftype="key")]
        conns = [{"id": 0, "users": {"r-talk-00": 50, "r-bt-00": 1}}, {"id": 1, "users": {"r-talk-00": 1,
                                                                                       "x": 9}}]
        m = fm.measure(rows, conns, self.PARTS, (0, 10), {})
        self.assertNotIn(77777, m["participants"]["talk-00"]["keyframes"]["2"]["sizes"])
        self.assertEqual(m["participants"]["bt-00"]["up_total"]["pps"], 0)

    def test_geometry(self):
        rows = [dict(row(1, 0, "u", 900, mtype="VIDEO", layer=2, ftype="key"), w="1280", h="720"),
                dict(row(1, 1, "u", 900, mtype="VIDEO", layer=0, ftype="key"), w="240", h="180", src_w="640",
                     src_h="480")]
        m = fm.measure(rows, self.CONNS, self.PARTS, (0, 10), {})
        self.assertEqual(m["participants"]["talk-00"]["geometry"], "1280x720")
        self.assertEqual(m["participants"]["bt-00"]["geometry"], "640x480")

    def test_geometry_without_a_source_size_is_the_top_video_layer(self):
        rows = [dict(row(1, 0, "u", 900, mtype="VIDEO", layer=layer, ftype="key"), w=w, h=h)
                for layer, w, h in ((0, "320", "180"), (1, "640", "360"), (2, "1280", "720"))]
        m = fm.measure(rows, self.CONNS, self.PARTS, (0, 10), {})
        self.assertEqual(m["participants"]["talk-00"]["geometry"], "1280x720")

    def test_cadence(self):
        m = {"participants": {"bt": pm("browser", "bt", "talker", kinds(video_L0=(6.0, 1), video_L2=(15.0, 1))),
                              "rt": pm("rust", "t", "talker", kinds(video_L2=(1.0, 1))),
                              "pr": pm("browser", "p", "receive")}}
        m["participants"]["bt"]["video_layers"] = [0, 1, 2]
        m["participants"]["rt"]["video_layers"] = [0, 1, 2]
        m["participants"]["pr"]["video_layers"] = []
        got = fm.cadence_problems(m, [5.7, 10.7, 18.6])
        self.assertEqual([g.split(":")[0] + g.split(":")[1][:16] for g in got],
                         ["bt video layer 1 a", "bt video layer 2 a"])
        self.assertEqual(fm.cadence_problems(m, []), [])

    def test_cadence_floor_is_inclusive(self):
        m = {"participants": {"bt": pm("browser", "bt", "talker", kinds(video_L0=(18.6, 1)))}}
        m["participants"]["bt"]["video_layers"] = [0]
        self.assertEqual(fm.cadence_problems(m, [18.6]), [])
        self.assertEqual(len(fm.cadence_problems(m, [18.61])), 1)

    def test_empty_window_refused(self):
        with self.assertRaises(ValueError):
            fm.measure([], [], self.PARTS, (5, 5), {})

    def test_percentile_nearest_rank(self):
        self.assertEqual(fm.percentile([5, 1, 3, 2, 4], 0.5), 3)
        self.assertEqual(fm.percentile(list(range(1, 11)), 0.9), 9)
        self.assertEqual(fm.percentile([7], 0.9), 7)

    def test_relay_deltas_keep_this_room(self):
        a = {'relay_room_bytes_total{direction="inbound",room="r1"}': 100.0,
             'relay_room_bytes_total{direction="inbound",room="r2"}': 0.0, "relay_nats_in_messages_total": 10.0}
        b = {'relay_room_bytes_total{direction="inbound",room="r1"}': 1100.0,
             'relay_room_bytes_total{direction="inbound",room="r2"}': 500.0, "relay_nats_in_messages_total": 30.0}
        d = fm.relay_deltas([(0.0, a), (10.0, b)], "r1", (0, 10))["per_s"]
        self.assertEqual(d, {'relay_room_bytes_total{direction="inbound",room="r1"}': 100.0,
                             "relay_nats_in_messages_total": 2.0})
        self.assertIsNone(fm.relay_deltas([(0.0, a)], "r1", (0, 10)))


def pm(fleet, role, cls, up=None, down_bps=0.0, keys=None, geometry="1280x720"):
    up = up or {}
    return {"fleet": fleet, "role": role, "class": cls, "up": up, "down": {}, "geometry": geometry,
            "up_total": {"pps": sum(v.get("pps", 0) for v in up.values()), "Bps": sum(v.get("Bps", 0)
                                                                                      for v in up.values())},
            "down_total": {"pps": 100.0 if down_bps else 0.0, "Bps": down_bps}, "keyframes": keys or {}}


def kinds(**rates):
    return {k: {"pps": v[0], "Bps": v[1]} for k, v in rates.items()}


class ToleranceTest(unittest.TestCase):
    def rows(self, result, measure, kind):
        return [r for r in result["rows"] if r["measure"] == measure and r["kind"] == kind]

    def test_judge_boundaries(self):
        self.assertEqual(ft.judge(115, 100, ft.PKT_TOL)[1], ft.PASS)
        self.assertEqual(ft.judge(85, 100, ft.PKT_TOL)[1], ft.PASS)
        self.assertEqual(ft.judge(115.2, 100, ft.PKT_TOL)[1], ft.FAIL)
        self.assertEqual(ft.judge(0, 0, ft.PKT_TOL), (0.0, ft.PASS))
        self.assertEqual(ft.judge(1, 0, ft.PKT_TOL), (None, ft.FAIL))
        self.assertEqual(ft.judge(0, 1, ft.PKT_TOL, minor=True)[1], ft.EXEMPT)

    def room(self, bot_up, ref_up, bot_down=1000.0, ref_down=1000.0, bot_keys=None, ref_keys=None,
             ref_geometry="1280x720"):
        return {"participants": {"b": pm("rust", "talk", "talker", bot_up, bot_down, bot_keys),
                                 "r": pm("browser", "bt", "talker", ref_up, ref_down, ref_keys, ref_geometry)},
                "problems": []}

    def test_all_compared_rows_pass_is_incomplete_not_pass(self):
        up = kinds(audio=(50, 5000), video_L0=(7, 1000))
        res = ft.evaluate(self.room(up, up))
        self.assertEqual(res["verdict"], "INCOMPLETE")
        self.assertEqual(res["counts"][ft.FAIL], 0)
        self.assertEqual(res["counts"][ft.NOT_RUN], 2)
        self.assertEqual(res["exit_code"], 2)

    def test_minor_kind_exempt_but_keyframe_request_never(self):
        bot = kinds(audio=(100, 5000), heartbeat=(0.1, 10), KEYFRAME_REQUEST=(0, 0))
        ref = kinds(audio=(100, 5000), heartbeat=(2, 10), KEYFRAME_REQUEST=(1, 90))
        res = ft.evaluate(self.room(bot, ref))
        self.assertEqual(self.rows(res, "uplink packets/s", "heartbeat")[0]["result"], ft.EXEMPT)
        self.assertEqual(self.rows(res, "uplink packets/s", "KEYFRAME_REQUEST")[0]["result"], ft.FAIL)
        self.assertEqual(res["verdict"], "FAIL")
        self.assertIn("uplink packets/s KEYFRAME_REQUEST", res["sizing_blockers"])

    def test_major_kind_outside_tolerance_fails(self):
        res = ft.evaluate(self.room(kinds(audio=(50, 8000)), kinds(audio=(50, 5000))))
        self.assertEqual(self.rows(res, "uplink bytes/s", "audio")[0]["result"], ft.FAIL)
        self.assertEqual(self.rows(res, "uplink packets/s", "audio")[0]["result"], ft.PASS)

    def test_bytes_rows_are_media_only_and_never_exempt(self):
        bot = kinds(video_L2=(30, 200000), audio=(50, 2000), heartbeat=(1, 10))
        ref = kinds(video_L2=(30, 200000), audio=(50, 4000), heartbeat=(1, 90))
        res = ft.evaluate(self.room(bot, ref))
        self.assertEqual(self.rows(res, "uplink bytes/s", "audio")[0]["result"], ft.FAIL)
        self.assertEqual(self.rows(res, "uplink bytes/s", "heartbeat"), [])
        self.assertEqual(self.rows(res, "uplink bytes/s", "video layer 2")[0]["result"], ft.PASS)

    def test_video_rows_need_a_browser_of_the_same_source_geometry(self):
        up = kinds(video_L2=(30, 200000), audio=(50, 5000))
        keys = {"2": {"per_s": 0.5, "sizes": [50000]}}
        res = ft.evaluate(self.room(up, up, bot_keys=keys, ref_keys=keys, ref_geometry="640x480"))
        self.assertEqual(self.rows(res, "uplink bytes/s", "video layer 2"), [])
        self.assertEqual(self.rows(res, "keyframe size p50", "video layer 2"), [])
        skipped = self.rows(res, "uplink bytes/s and keyframe size", "video")[0]
        self.assertEqual(skipped["result"], ft.NOT_RUN)
        self.assertIn("1280x720", skipped["note"])
        self.assertEqual(self.rows(res, "keyframe rate", "video layer 2")[0]["result"], ft.PASS)

    def test_keyframe_sizes_compare_with_browsers_of_the_same_geometry_only(self):
        up = kinds(video_L2=(30, 200000), audio=(50, 5000))
        parts = {"b": pm("rust", "cam", "camera", up, 1000.0, {"2": {"per_s": 0.5, "sizes": [50000] * 4}}),
                 "big": pm("browser", "bcam", "camera", up, 1000.0, {"2": {"per_s": 0.5, "sizes": [50000] * 4}}),
                 "small": pm("browser", "bsmall", "camera", up, 1000.0, {"2": {"per_s": 0.5, "sizes": [9000] * 4}},
                             "640x480")}
        res = ft.evaluate({"participants": parts, "problems": []})
        for label in ("keyframe size p50", "keyframe size p90"):
            [r] = self.rows(res, label, "video layer 2")
            self.assertEqual((r["result"], r["browser"], r["reference"]), (ft.PASS, 50.0, "bcam (n=1) 1280x720"))

    def test_video_bytes_compare_with_browsers_of_the_same_geometry_only(self):
        same, small = kinds(video_L2=(30, 200000)), kinds(video_L2=(30, 20000))
        parts = {"b": pm("rust", "cam", "camera", same, 1000.0),
                 "big": pm("browser", "bcam", "camera", same, 1000.0),
                 "small": pm("browser", "bsmall", "camera", small, 1000.0, geometry="640x480")}
        [r] = self.rows(ft.evaluate({"participants": parts, "problems": []}), "uplink bytes/s", "video layer 2")
        self.assertEqual((r["result"], r["browser"], r["reference"]), (ft.PASS, 1600.0, "bcam (n=1) 1280x720"))

    def test_an_unknown_geometry_on_both_sides_is_not_the_same_geometry(self):
        up = kinds(video_L2=(30, 200000))
        keys = {"2": {"per_s": 0.5, "sizes": [50000]}}
        res = ft.evaluate({"participants": {"b": pm("rust", "cam", "camera", up, 1000.0, keys, None),
                                            "r": pm("browser", "bcam", "camera", up, 1000.0, keys, None)},
                           "problems": []})
        self.assertEqual(self.rows(res, "uplink bytes/s", "video layer 2"), [])
        self.assertEqual(self.rows(res, "keyframe size p50", "video layer 2"), [])
        self.assertEqual(self.rows(res, "uplink bytes/s and keyframe size", "video")[0]["result"], ft.NOT_RUN)

    def test_a_receive_setting_is_named_on_the_downlink_row(self):
        up = kinds(audio=(50, 5000))
        res = ft.evaluate(self.room(up, up, bot_down=300.0), receive={"talk": "Rust receive pinned to video layer 0"})
        [r] = self.rows(res, "downlink bytes/s", "all")
        self.assertEqual((r["result"], r["note"]), (ft.FAIL, "Rust receive pinned to video layer 0"))

    def test_the_receive_note_is_on_its_own_role_only(self):
        plan, _ = fidelity_run.compile_run(fidelity_run.DEFAULT_SCENARIO, "0123456789abcdef")
        up = kinds(audio=(50, 5000))
        parts = {f"{role}-00": pm(fleet, role, cls, up, 1000.0) for fleet, role, cls in (
            ("rust", "talkers", "talker"), ("rust", "cameras", "camera"), ("rust", "viewers", "receive"),
            ("rust", "vpviewers", "receive"), ("browser", "btalk", "talker"), ("browser", "bcam", "camera"),
            ("browser", "probe", "receive"))}
        res = ft.evaluate({"participants": parts, "problems": []}, receive=fidelity_run.receive_notes(plan))
        notes = {r["role"].split(" ")[0]: r["note"] for r in res["rows"] if r["measure"] == "downlink bytes/s"}
        self.assertEqual(notes, {"talkers": "", "cameras": "",
                                 "viewers": "Rust receive pinned to video layer 0; the browsers choose their own "
                                            "video layers",
                                 "vpviewers": "Rust receive viewport_visible_count 4; the browsers choose their own "
                                              "video layers"})

    def test_role_value_is_the_median(self):
        parts = {f"b{i}": pm("rust", "talk", "talker", kinds(audio=(50, v)), 1000.0)
                 for i, v in enumerate((1000, 2000, 10000))}
        parts["r"] = pm("browser", "bt", "talker", kinds(audio=(50, 2000)), 1000.0)
        res = ft.evaluate({"participants": parts, "problems": []})
        self.assertEqual(self.rows(res, "uplink bytes/s", "audio")[0]["result"], ft.PASS)

    def test_keyframe_threshold_side(self):
        up = kinds(video_L2=(20, 100000))
        res = ft.evaluate(self.room(up, up, bot_keys={"2": {"per_s": 0.5, "sizes": [33000] * 4}},
                                    ref_keys={"2": {"per_s": 0.5, "sizes": [32000] * 4}}))
        p50 = self.rows(res, "keyframe size p50", "video layer 2")[0]
        self.assertAlmostEqual(p50["diff"], 1000 / 32000)
        self.assertEqual(p50["result"], ft.FAIL)
        self.assertIn("32 KiB", p50["note"])
        self.assertEqual(self.rows(res, "keyframe rate", "video layer 2")[0]["result"], ft.PASS)

    def test_no_downlink_on_either_side_is_invalid(self):
        up = kinds(audio=(50, 5000))
        res = ft.evaluate(self.room(up, up, bot_down=0.0, ref_down=0.0))
        row = self.rows(res, "downlink bytes/s", "all")[0]
        self.assertEqual((row["result"], res["verdict"]), (ft.NOT_RUN, "INVALID"))
        self.assertIn("received nothing", " ".join(res["problems"]))

    def test_tolerance_boundaries(self):
        def row(res, measure, kind):
            return self.rows(res, measure, kind)[0]["result"]

        base = kinds(audio=(50, 4000), video_L2=(30, 100000))
        for bot_audio, want in ((5000, ft.PASS), (5001, ft.FAIL)):
            res = ft.evaluate(self.room(kinds(audio=(50, bot_audio), video_L2=(30, 100000)), base))
            self.assertEqual(row(res, "uplink bytes/s", "audio"), want, bot_audio)
        for size, want in ((13000, ft.PASS), (13001, ft.FAIL)):
            res = ft.evaluate(self.room(base, base, bot_keys={"2": {"per_s": 0.5, "sizes": [size]}},
                                        ref_keys={"2": {"per_s": 0.5, "sizes": [10000]}}))
            self.assertEqual(row(res, "keyframe size p50", "video layer 2"), want, size)
        for down, want in ((1250.0, ft.PASS), (1250.1, ft.FAIL)):
            res = ft.evaluate(self.room(base, base, bot_down=down, ref_down=1000.0))
            self.assertEqual(row(res, "downlink bytes/s", "all"), want, down)
        ref = kinds(audio=(96, 4000), heartbeat=(4.0, 10))
        for hb, want in ((5.0, ft.FAIL), (4.9, ft.EXEMPT)):
            res = ft.evaluate(self.room(kinds(audio=(100 - hb, 4000), heartbeat=(hb, 10)), ref))
            self.assertEqual(row(res, "uplink packets/s", "heartbeat"), want, hb)

    def test_32_kib_is_on_the_small_side(self):
        up = kinds(video_L2=(20, 100000))
        res = ft.evaluate(self.room(up, up, bot_keys={"2": {"per_s": 0.5, "sizes": [32768]}},
                                    ref_keys={"2": {"per_s": 0.5, "sizes": [32769]}}))
        p50 = self.rows(res, "keyframe size p50", "video layer 2")[0]
        self.assertEqual(p50["result"], ft.FAIL)
        self.assertIn("32 KiB", p50["note"])

    def test_downlink(self):
        up = kinds(audio=(50, 5000))
        res = ft.evaluate(self.room(up, up, bot_down=1300.0, ref_down=1000.0))
        self.assertEqual(self.rows(res, "downlink bytes/s", "all")[0]["result"], ft.FAIL)
        self.assertEqual(res["sizing_blockers"], ["downlink bytes/s"])

    def test_sizing_line_does_not_clear_a_role_whose_video_bytes_were_not_run(self):
        up = kinds(audio=(50, 5000), video_L0=(7, 1000))
        clean = ft.evaluate(self.room(up, up))
        self.assertEqual(ft.sizing_line(clean), "not blocked by video bytes, downlink or KEYFRAME_REQUEST")
        res = ft.evaluate(self.room(up, up, ref_geometry="640x480"))
        self.assertEqual(res["verdict"], "INCOMPLETE")
        gap = "NOT RUN: uplink bytes/s and keyframe size for talk"
        self.assertEqual(ft.sizing_line(res), f"not shown, {gap}")
        self.assertIn(f"Usable for sizing: not shown, {gap}", ft.render(res, []))
        res = ft.evaluate(self.room(up, up, bot_down=9000.0, ref_geometry="640x480"))
        self.assertEqual(ft.sizing_line(res), f"no, FAIL on downlink bytes/s; {gap}")
        self.assertEqual(ft.sizing_line(dict(res, verdict="ERROR")), "no, the run is ERROR")

    def test_a_kind_on_one_side_only_is_a_row_not_a_skip(self):
        keys = {"0": {"per_s": 0.2, "sizes": [9000, 9000]}}
        sides = {"bot": {"bot_keys": keys}, "browser": {"ref_keys": keys}}
        for side, key_arg in sides.items():
            has, none = kinds(audio=(50, 5000), video_L0=(7, 1000)), kinds(audio=(50, 5000))
            room = self.room(has, none, **key_arg) if side == "bot" else self.room(none, has, **key_arg)
            res = ft.evaluate(room)
            for measure in ("uplink packets/s", "uplink bytes/s", "keyframe rate", "keyframe size p50"):
                self.assertEqual(self.rows(res, measure, "video layer 0")[0]["result"], ft.FAIL, (side, measure))
        for bot_down, ref_down in ((1000.0, 0.0), (0.0, 1000.0)):
            row_ = self.rows(ft.evaluate(self.room(kinds(), kinds(), bot_down, ref_down)), "downlink bytes/s", "all")[0]
            self.assertEqual(row_["result"], ft.FAIL)

    def test_problems_make_invalid_even_with_fails(self):
        room = self.room(kinds(audio=(10, 5000)), kinds(audio=(50, 5000)))
        res = ft.evaluate(room, ["capture dropped 3 packets"])
        self.assertEqual((res["verdict"], res["exit_code"]), ("INVALID", 4))
        text = ft.render(res, [])
        self.assertIn("Why the run is INVALID", text)
        self.assertIn("Usable for sizing: no, the run is INVALID", text)

    def test_rust_role_without_a_browser_of_its_class_is_invalid(self):
        up = kinds(audio=(50, 5000))
        room = self.room(up, up)
        room["participants"]["v"] = pm("rust", "viewers", "receive", kinds(HEALTH=(0.2, 40)), 99999.0)
        res = ft.evaluate(room)
        self.assertEqual(res["verdict"], "INVALID")
        self.assertIn("Rust role viewers (receive) has no browser", " ".join(res["problems"]))

    def test_browser_class_without_a_rust_role_is_invalid(self):
        up = kinds(audio=(50, 5000))
        room = self.room(up, up)
        room["participants"]["p"] = pm("browser", "probe", "receive", kinds(HEALTH=(0.2, 40)), 1000.0)
        res = ft.evaluate(room)
        self.assertEqual(res["verdict"], "INVALID")
        self.assertIn("browser class receive has no Rust role", " ".join(res["problems"]))

    def test_nothing_compared_is_invalid(self):
        res = ft.evaluate({"participants": {"b": pm("rust", "talk", "talker")}, "problems": []})
        self.assertEqual(res["verdict"], "INVALID")
        self.assertEqual(self.rows(res, "every measure", "-")[0]["result"], ft.NOT_RUN)
        empty = ft.evaluate({"participants": {}, "problems": []})
        self.assertEqual(empty["verdict"], "INVALID")
        self.assertIn("nothing was compared", " ".join(empty["problems"]))

    def test_render_never_says_pass_for_incomplete(self):
        up = kinds(audio=(50, 5000))
        text = ft.render(ft.evaluate(self.room(up, up)), ["h"])
        self.assertIn("Verdict: INCOMPLETE", text)
        self.assertNotIn("Verdict: PASS", text)
        self.assertEqual(len(ft.table_lines(ft.evaluate(self.room(up, up)), markdown=True)[2].split("|")), 13)

    def test_retest(self):
        a = {"participants": {"x": pm("rust", "t", "talker", kinds(audio=(50, 5000)), 1000.0)}}
        same = ft.retest(a, json.loads(json.dumps(a)))
        self.assertTrue(same["agree"])
        b = json.loads(json.dumps(a))
        b["participants"]["x"]["down_total"]["Bps"] = 1060.0
        self.assertFalse(ft.retest(a, b)["agree"])
        self.assertFalse(ft.retest(a, {"participants": {}})["agree"])
        self.assertFalse(ft.retest({"participants": {}}, {"participants": {}})["agree"])
        quiet = {"participants": {"x": pm("rust", "t", "talker", kinds(audio=(0, 0)), 0.0)}}
        self.assertFalse(ft.retest(quiet, json.loads(json.dumps(quiet)))["agree"])

    def test_retest_never_agrees_when_one_direction_is_silent(self):
        deaf = {"participants": {"x": pm("rust", "t", "talker", kinds(audio=(50, 5000)), 0.0)}}
        res = ft.retest(deaf, json.loads(json.dumps(deaf)))
        self.assertEqual((res["agree"], res["silent"]), (False, ["x", "x"]))

    def test_retest_never_agrees_when_the_uplink_is_silent(self):
        mute = {"participants": {"x": pm("rust", "t", "talker", {}, 1000.0)}}
        res = ft.retest(mute, json.loads(json.dumps(mute)))
        self.assertEqual((res["agree"], res["silent"]), (False, ["x", "x"]))


class LocalHelpersTest(unittest.TestCase):
    def test_bot_indices(self):
        parts = {f"u{i}": {"role": r} for i, r in enumerate(["bt", "bt", "bcam", "probe", "bt", "bt", "bt"])}
        procs = [{"proc_id": f"p{i}", "participants": [f"u{i}"]} for i in range(7)]
        got = fidelity_local.bot_indices(procs, parts, {"bcam": "1280x720"})
        self.assertEqual([got[f"p{i}"] for i in range(7)], [2, 3, 1, 4, 5, 6, 8])

    def test_capture_stats(self):
        log = "listening on eth0\n1200 packets captured\n1300 packets received by filter\n0 packets dropped by kernel\n"
        self.assertEqual(fidelity_local.capture_stats(log), (1200, 0))
        self.assertEqual(fidelity_local.capture_stats(log + "7 packets dropped by interface\n"), (1200, 7))
        self.assertEqual(fidelity_local.capture_stats("3 packets dropped by interface"), (None, None))
        self.assertEqual(fidelity_local.capture_stats("killed"), (None, None))

    def test_manifest_path_deps_reads_every_form(self):
        text = """[lib]
path = "src/lib.rs"
[[bin]]
path = "src/bin/x.rs"
[dependencies]
inline = { version = "1", path = "../inline" }
dotted.path = "../dotted"
plain = "1"
[target.'cfg(unix)'.dev-dependencies]
cfg = { path = "../cfg" }
[dependencies.table]
version = "1"
path = "../table"
[patch.crates-io]
patched = { path = "vendor/patched" }
[patch."https://example.com/x.git".gitdep]
path = "vendor/gitdep"
"""
        self.assertEqual(sorted(manifest_path_deps(text)),
                         ["../cfg", "../dotted", "../inline", "../table", "vendor/gitdep", "vendor/patched"])

    def test_every_path_dependency_of_the_product_is_a_product_path(self):
        entries = [e for e in fidelity_local.PRODUCT_PATHS if not e.startswith(":")]
        manifests = ["Cargo.toml"] + [os.path.join(e, "Cargo.toml") for e in entries
                                      if os.path.isfile(os.path.join(ROOT, e, "Cargo.toml"))]
        deps = set()
        for m in manifests:
            with open(os.path.join(ROOT, m), encoding="utf-8") as fh:
                for dep in manifest_path_deps(fh.read()):
                    deps.add(os.path.relpath(os.path.normpath(os.path.join(ROOT, os.path.dirname(m), dep)), ROOT))
        self.assertTrue({"vendor/quinn-proto", "videocall-types"} <= deps, deps)
        self.assertEqual(sorted(d for d in deps if not any(d == e or d.startswith(e + "/") for e in entries)), [])


DEP_TABLE = r'(?:^|\.)(?:dev-|build-)?dependencies|^patch\.(?:"[^"]*"|[\w-]+)'


def manifest_path_deps(text):
    """Every path of a Cargo dependency or patch, written inline, as a dotted key or as its own table."""
    deps, table = [], ""
    for line in text.splitlines():
        head = re.fullmatch(r"\s*\[\[?\s*(.+?)\s*\]\]?\s*(?:#.*)?", line)
        if head:
            table = head.group(1)
            continue
        m = re.match(r'\s*([\w.-]+)\s*=\s*(.*)', line)
        if not m:
            continue
        key, value = m.groups()
        dep_table = re.fullmatch(rf".*?(?:{DEP_TABLE})", table)
        dep_entry = re.fullmatch(rf".*?(?:{DEP_TABLE})\.[\w-]+", table)
        path = None
        if value.startswith("{") and dep_table:
            path = re.search(r'\bpath\s*=\s*"([^"]+)"', value)
        elif key.endswith(".path") and dep_table or key == "path" and dep_entry:
            path = re.match(r'"([^"]+)"', value)
        if path:
            deps.append(path.group(1))
    return deps


def proc(stdout="", returncode=0, stderr=""):
    return types.SimpleNamespace(stdout=stdout, stderr=stderr, returncode=returncode)


class LocalRunTest(unittest.TestCase):
    def local(self):
        plan, fid = fidelity_run.compile_run(fidelity_run.DEFAULT_SCENARIO, "0123456789abcdef")
        lr = fidelity_local.LocalRun(plan, fid, tempfile.mkdtemp(), lambda *_: None, relay_container="relay")
        lr.ctx.commit = "harness"
        return lr

    def test_media_problems(self):
        lr = self.local()
        browsers = [p for p in lr.plan["processes"] if p["fleet"] == "browser"]
        self.assertEqual(len(lr.fleet.media_problems(browsers)), len(browsers))
        for proc in browsers:
            uid = proc["participants"][0]
            folder = lr.ctx.path("probes", str(proc["probe_index"]), "participants")
            os.makedirs(folder)
            publishes = dict(lr.fleet.parts[uid]["publishes"])
            if proc["proc_id"] == "probe-00":
                publishes["camera"] = False
            fidelity_run.write_json(os.path.join(folder, "bot.json"),
                                    {"participant": {"user_id": uid, "publishes": publishes}})
        self.assertEqual(lr.fleet.media_problems(browsers),
                         ["probe-00: camera is off after joining, the scenario has it on"])

    def test_stamps(self):
        lr = self.local()
        self.assertIn("could not be found", lr.stamp_problems()[0])
        lr.stack_root = tempfile.mkdtemp()
        os.makedirs(os.path.join(lr.stack_root, fidelity_local.STAMPS))
        fast = {"settle_s": 0.05, "build_s": 0.05, "poll_s": 0.01}
        started = time.monotonic()
        self.assertEqual(lr.stamp_problems(**fast), [])
        self.assertGreaterEqual(time.monotonic() - started, 0.05)
        self.assertEqual(lr.facts["build_stamps"], {})

        def stamp(name, build):
            with open(os.path.join(lr.stack_root, fidelity_local.STAMPS, name), "w", encoding="utf-8") as fh:
                json.dump({"build": build}, fh)

        stamp("websocket_server.json", "ok")
        stamp("meeting-api.json", "building")
        started = time.monotonic()
        problems = lr.stamp_problems(**fast)
        self.assertGreaterEqual(time.monotonic() - started, 0.05)
        self.assertEqual(problems, [f"{fidelity_local.STAMPS}/meeting-api.json: build 'building'; the stack did "
                                    "not finish building"])
        stamp("meeting-api.json", "failed")
        self.assertIn("'failed'", lr.stamp_problems(**fast)[0])
        stamp("meeting-api.json", "ok")
        self.assertEqual(lr.stamp_problems(settle_s=5, build_s=5, poll_s=0.01), [])

    def commit_facts(self, stack_head, diff_rc, dirty="", status_rc=None):
        lr = self.local()
        lr.stack_root = tempfile.mkdtemp()
        calls = []

        def sh(argv, timeout, check=True, env=None):
            calls.append(argv)
            if "rev-parse" in argv:
                return proc(stack_head + "\n")
            if "status" in argv:
                harness = argv[2] == lr.ctx.root
                return proc(dirty if harness else "", (status_rc or {}).get("harness" if harness else "stack", 0))
            if "diff" in argv:
                return proc(returncode=diff_rc)
            raise AssertionError(argv)

        lr.ctx.sh = sh
        lr.commit_facts()
        return lr, calls

    def test_commit_facts(self):
        lr, calls = self.commit_facts("harness", 1)
        self.assertEqual(lr.preconditions, [])
        self.assertFalse(any("diff" in c for c in calls))
        lr, _ = self.commit_facts("stack", 1)
        self.assertIn("their product code differs", lr.preconditions[0])
        lr, _ = self.commit_facts("stack", 0)
        self.assertEqual((lr.preconditions, lr.facts["stack_commit"]), ([], "stack"))
        lr, calls = self.commit_facts("harness", 0, dirty=" M bot/src/main.rs\n")
        self.assertIn("uncommitted product changes", lr.preconditions[0])
        self.assertIn("--untracked-files=all", next(c for c in calls if "status" in c))

    def test_every_commit_problem_reads_as_the_commits_cause(self):
        import fidelity_report
        cases = [self.commit_facts("stack", 1), self.commit_facts("stack", 128),
                 self.commit_facts("harness", 0, dirty=" M bot/src/main.rs\n"),
                 self.commit_facts("harness", 0, status_rc={"stack": 128})]
        for lr, _ in cases:
            self.assertEqual(fidelity_report.plain_reasons(lr.preconditions),
                             ["The server and the test tool may not be running the same code."], lr.preconditions)

    def test_a_git_status_that_fails_is_not_a_clean_tree(self):
        for which in ("harness", "stack"):
            lr, _ = self.commit_facts("harness", 0, status_rc={which: 128})
            self.assertEqual(lr.preconditions, [f"git status of the {which} checkout exited 128, so uncommitted "
                                                "product changes cannot be ruled out"])

    def stop_capture(self, log, wait=None):
        lr = self.local()
        lr.capture_log = os.path.join(lr.run_dir, "tcpdump.log")
        os.makedirs(lr.run_dir, exist_ok=True)
        with open(lr.capture_log, "w", encoding="utf-8") as fh:
            fh.write(log)
        lr.capture_proc = FakeProc(wait)
        lr.ctx.sh = lambda argv, timeout, check=True, env=None: proc("")
        lr.stop_capture()
        return lr

    def provenance(self, version, head="abc12345ffff", moved="HEAD@{1000}", status=200):
        lr = self.local()
        lr.stack_root = tempfile.mkdtemp()
        lr.ctx.http = lambda url, timeout=10: (status, json.dumps(version).encode())

        def sh(argv, timeout, check=True, env=None):
            if "rev-parse" in argv:
                return proc(head + "\n")
            if "reflog" in argv:
                return proc(moved + "\n")
            raise AssertionError(argv)

        lr.ctx.sh = sh
        return lr.provenance_problems()

    def test_relay_provenance(self):
        unknown = {"git_sha": "unknown"}
        self.assertEqual(self.provenance(dict(unknown, build_timestamp="1970-01-01T00:16:42Z")), [])
        self.assertEqual(self.provenance(dict(unknown, build_timestamp="1970-01-01T00:20:00Z")), [])
        for ts in ("1970-01-01T00:16:40Z", "1970-01-01T00:16:41Z", "1970-01-01T00:10:00Z"):
            refusal = self.provenance(dict(unknown, build_timestamp=ts))[0]
            self.assertIn("not at least 2 s after", refusal, ts)
            self.assertIn("touch actix-api/build.rs", refusal, ts)
        self.assertEqual(self.provenance({"git_sha": "abc12345"}), [])
        wrong = self.provenance({"git_sha": "fff12345", "build_timestamp": "1970-01-01T00:20:00Z"})
        self.assertIn("reports git_sha fff12345", wrong[0])
        self.assertIn("touch actix-api/build.rs", wrong[0])
        self.assertIn("/version answered 404", self.provenance(unknown, status=404)[0])
        self.assertIn("is unknown", self.provenance(dict(unknown, build_timestamp="1970-01-01T00:20:00Z"),
                                                    moved="no reflog")[0])
        self.assertIn("is unknown", self.provenance(dict(unknown, build_timestamp="unknown"))[0])

    def test_a_container_that_exits_during_the_hold_is_invalid(self):
        lr = self.local()
        exits = [[fidelity_local.cq_local.Exit("rust-pub", None, 137)]]
        lr.fleet.poll = lambda: exits.pop() if exits else []
        hold_poll = fidelity_local.HOLD_POLL_S
        fidelity_local.HOLD_POLL_S = 0.01
        try:
            lr.hold(0.05)
        finally:
            fidelity_local.HOLD_POLL_S = hold_poll
        self.assertEqual(lr.preconditions, ["rust-pub exited during the hold (code 137)"])

    def test_an_unreadable_diff_is_not_same_code(self):
        lr, _ = self.commit_facts("stack", 128)
        self.assertIn("exited 128, so their product code cannot be compared", lr.preconditions[0])

    def test_remove_containers_force_removes_by_label(self):
        lr = self.local()
        lr.capture_proc = FakeProc(subprocess.TimeoutExpired("x", 1))
        calls = []

        def sh(argv, timeout, check=True, env=None):
            calls.append(argv)
            return proc("abc\ndef\n" if "ps" in argv else "")

        lr.ctx.sh = sh
        lr.remove_containers()
        self.assertTrue(lr.capture_proc.killed)
        self.assertIn(f"label={fidelity_local.cq_local.LABEL}={lr.ctx.run_id}", calls[0])
        self.assertEqual(calls[1][1:], ["rm", "-f", "abc", "def"])

    def test_remove_containers_reports_docker_failures(self):
        lr = self.local()
        lr.ctx.sh = lambda argv, timeout, check=True, env=None: proc(returncode=1, stderr="daemon gone")
        self.assertEqual(lr.remove_containers(), ["docker ps exited 1: daemon gone"])
        lr.ctx.sh = lambda argv, timeout, check=True, env=None: (
            proc("abc\n") if "ps" in argv else proc(returncode=1, stderr="busy"))
        self.assertEqual(lr.remove_containers(), ["docker rm -f exited 1: busy"])

        def broken(argv, timeout, check=True, env=None):
            raise fidelity_local.AdapterError("docker ps timed out after 60 s")

        lr.ctx.sh = broken
        self.assertEqual(lr.remove_containers(), ["docker ps timed out after 60 s"])

    def test_the_capture_client_runs_in_its_own_session(self):
        lr = self.local()
        stub = os.path.join(tempfile.mkdtemp(), "docker")
        with open(stub, "w", encoding="utf-8") as fh:
            fh.write("#!/bin/sh\necho listening on eth0 >&2\nexec sleep 30\n")
        os.chmod(stub, 0o700)
        lr.ctx.docker = stub
        lr.start_capture("img")
        self.addCleanup(lr.capture_proc.wait)
        self.addCleanup(lr.capture_proc.kill)
        self.assertEqual(os.getsid(lr.capture_proc.pid), lr.capture_proc.pid)
        self.assertNotEqual(os.getsid(lr.capture_proc.pid), os.getsid(0))

    def test_the_relay_samples_keep_no_other_room(self):
        lr = self.local()
        room = lr.plan["meeting_id"]
        body = (f'relay_room_bytes_total{{room="{room}",direction="inbound"}} 5\n'
                'relay_room_bytes_total{room="their-meeting",direction="inbound"} 9\n'
                f'relay_room_bytes_total{{room="their-meeting",bedroom="{room}"}} 9\n').encode()
        polled = threading.Event()

        def http(url, timeout=None):
            polled.set()
            return 200, body

        lr.ctx.http = http
        lr.ctx.sh = lambda *a, **k: proc()
        os.makedirs(lr.ctx.path("samples"), exist_ok=True)
        threads = lr.sampler()
        self.assertTrue(polled.wait(5))
        lr.stop.set()
        for th in threads:
            th.join(5)
        with gzip.open(lr.ctx.path("samples", "relay_metrics.jsonl.gz"), "rt", encoding="utf-8") as fh:
            text = fh.read()
        self.assertIn(room, text)
        self.assertNotIn("their-meeting", text)

    def test_the_capture_sidecar_is_locked_down_and_its_files_private(self):
        lr = self.local()
        lr.relay_container = "relay"
        seen = {}

        class Popen:
            def __init__(self, argv, stdout, stderr, stdin, start_new_session):
                seen["argv"] = argv
                stderr.write(b"listening on eth0\n")
                stderr.flush()

            def poll(self):
                return None

        real = fidelity_local.subprocess.Popen
        fidelity_local.subprocess.Popen = Popen
        try:
            lr.start_capture("img")
        finally:
            fidelity_local.subprocess.Popen = real
        argv = seen["argv"]
        for flag in (["--cap-drop", "ALL"], ["--cap-add", "NET_RAW"], ["--security-opt", "no-new-privileges"]):
            i = argv.index(flag[0])
            self.assertEqual(argv[i:i + 2], flag)
        self.assertIn("--read-only", argv)
        self.assertEqual(argv[argv.index("--log-driver") + 1], "none")
        self.assertEqual(argv[argv.index("--ulimit") + 1], "core=0")
        self.assertLess(argv.index("--ulimit"), argv.index("img"))
        self.assertNotIn("-v", argv)
        image = argv.index("img")
        self.assertEqual(argv[image - 2:image], ["--entrypoint", "timeout"])
        self.assertEqual(argv[image + 1:image + 3], [str(lr.fleet._ttl_s()), "tcpdump"])
        self.assertEqual(argv[argv.index("-w") + 1], "-")
        cap = lr.ctx.path("capture")
        self.assertEqual(os.stat(cap).st_mode & 0o777, 0o700)
        self.assertEqual(os.stat(os.path.join(cap, "relay.pcap")).st_mode & 0o777, 0o600)

    def test_the_capture_log_is_saved_after_the_container_logs(self):
        lr = self.local()
        lr.capture_log = os.path.join(lr.run_dir, "tcpdump.log")
        os.makedirs(lr.run_dir, exist_ok=True)
        with open(lr.capture_log, "w", encoding="utf-8") as fh:
            fh.write("listening on eth0\n")
        lr.capture_image = lambda: "img"
        lr.start_capture = lambda image: None
        lr.sampler = lambda: []
        lr.fleet._start_sampler = lr.fleet._stop_sampler = lambda: None

        def save_containers():
            lr.ctx.write_log(os.path.join("logs", "capture.log.gz"), "configured logging driver does not support "
                             "reading")

        def launch(p):
            raise fidelity_local.AdapterError("launch failed")

        def sh(argv, timeout, check=True, env=None):
            raise fidelity_local.AdapterError("docker stop timed out after 60 s")

        lr.fleet._save_containers, lr.fleet.start, lr.ctx.sh = save_containers, launch, sh
        image = fidelity_local.cq_local.BotImage
        fidelity_local.cq_local.BotImage = lambda: types.SimpleNamespace(_bot_image=lambda: None)
        self.addCleanup(setattr, fidelity_local.cq_local, "BotImage", image)
        with self.assertRaises(fidelity_local.AdapterError):
            lr.run()
        with gzip.open(lr.ctx.path("logs", "capture.log.gz"), "rt", encoding="utf-8") as fh:
            self.assertEqual(fh.read(), "listening on eth0\n")
        self.assertIn("stopping the capture failed", lr.preconditions[0])
        unstarted = self.local()
        unstarted.save_capture_log()
        self.assertFalse(os.path.exists(unstarted.ctx.path("logs", "capture.log.gz")))

    def test_capture_stop_failure_is_invalid(self):
        lr = self.local()

        def sh(argv, timeout, check=True, env=None):
            raise fidelity_local.AdapterError("docker stop timed out after 60 s")

        lr.ctx.sh = sh
        lr.stop_capture()
        self.assertIn("stopping the capture failed", lr.preconditions[0])

    def test_host_verdict(self):
        lr = self.local()
        lr.fleet.generator_verdicts = lambda a, b: [{"ok": True}]
        lr.host_verdict(0, 1)
        self.assertEqual(lr.preconditions, [])
        lr.fleet.generator_verdicts = lambda a, b: [{"ok": False, "starved": True, "reasons": ["cpu 97 %"]}]
        lr.host_verdict(0, 1)
        self.assertIn("cpu 97 %", lr.preconditions[0])

        def broken(a, b):
            raise fidelity_local.AdapterError("resource-verdict exited 2")

        lr.fleet.generator_verdicts = broken
        lr.host_verdict(0, 1)
        self.assertIn("no host resource verdict", lr.preconditions[1])

    def test_preflight_refuses_low_disk_and_untraceable_relay(self):
        lr = self.local()
        lr.min_free_gb = 1e12
        stack = tempfile.mkdtemp()
        os.makedirs(os.path.join(stack, "docker"))
        lr.ctx.sh = lambda argv, timeout, check=True, env=None: proc(
            f"videocall-e2e\twebsocket-api\t{os.path.join(stack, 'docker')}\n" if "inspect" in argv else "")
        lr.ctx.http = lambda url, timeout=10: (200, b"")
        lr.stamp_problems = lambda: []
        problems = lr.preflight()
        self.assertTrue(any("GiB free" in p for p in problems))
        self.assertTrue(any("/version answered 200" in p for p in problems), problems)

    def test_preflight_names_a_left_run_and_how_to_remove_it(self):
        lr = self.local()
        ps = []

        def sh(argv, timeout, check=True, env=None):
            if "ps" not in argv:
                return proc("")
            ps.append(argv)
            return proc("fid-old\nfid-old\n" if "--format" in argv else "3f2a9c1b7d0e\n8e1d4b6a2c5f\n")

        lr.ctx.sh = sh
        lr.ctx.http = lambda url, timeout=10: (200, b"")
        lr.stamp_problems = lr.provenance_problems = lambda: []
        problems = lr.preflight()
        self.assertEqual([a[a.index("--format") + 1] for a in ps if "--format" in a], ['{{.Label "cq.run_id"}}'])
        self.assertIn("2 container(s) of run fid-old exist; another run may be going. If it was killed: docker rm -f "
                      "$(docker ps -aq --filter label=cq.run_id=fid-old)", problems)

    def test_preflight_refuses_a_relay_container_outside_the_e2e_stack(self):
        stack = tempfile.mkdtemp()
        os.makedirs(os.path.join(stack, "docker"))
        for labels, ok in (("videocall-e2e\twebsocket-api", True), ("other-stack\twebsocket-api", False),
                           ("videocall-e2e\tmeeting-api", False), ("", False)):
            lr = self.local()
            lr.ctx.sh = lambda argv, timeout, check=True, env=None, out=f"{labels}\t{stack}/docker\n": proc(
                out if "inspect" in argv else "")
            lr.ctx.http = lambda url, timeout=10: (200, b"")
            lr.stamp_problems = lambda: []
            lr.provenance_problems = lambda: []
            refused = [p for p in lr.preflight() if p.startswith("relay is service")]
            self.assertEqual((bool(refused), lr.stack_root), (False, stack) if ok else (True, None), labels)

    def test_preflight_refuses_an_empty_relay_container(self):
        lr = self.local()
        lr.relay_container = ""
        calls = []
        lr.ctx.sh = lambda argv, timeout, check=True, env=None: calls.append(argv) or proc("")
        lr.ctx.http = lambda url, timeout=10: (200, b"")
        lr.stamp_problems = lr.provenance_problems = lambda: []
        self.assertIn("--relay-container is empty: name the e2e stack's websocket-api container or leave it out",
                      lr.preflight())
        self.assertFalse([a for a in calls if "inspect" in a and "" in a])

    def test_stop_capture(self):
        ok = "10 packets captured\n10 packets received by filter\n0 packets dropped by kernel\n"
        self.assertEqual(self.stop_capture(ok).preconditions, [])
        self.assertIn("dropped 4 packets", self.stop_capture(ok.replace("0 packets dropped", "4 packets dropped"))
                      .preconditions[0])
        self.assertIn("no capture summary", self.stop_capture("Killed").preconditions[0])
        self.assertIn("no capture summary", self.stop_capture("0 packets dropped by kernel\n").preconditions[0])
        late = self.stop_capture(ok, wait=subprocess.TimeoutExpired("docker run", 30))
        self.assertIn("stopping the capture failed", late.preconditions[0])
        self.assertTrue(late.capture_proc.killed)


class FakeProc:
    def __init__(self, wait_error=None):
        self.wait_error, self.killed = wait_error, False

    def wait(self, timeout=None):
        if self.wait_error is not None:
            raise self.wait_error
        return 0

    def poll(self):
        return None if self.wait_error is not None and not self.killed else 0

    def kill(self):
        self.killed = True


class FakeLocal:
    """Stands in for fidelity_local.LocalRun: writes a raw pcap with a token, then fails like an aborted run."""
    fail, ran, removed, window, cleanup_failures, labelled, sweep_mid_run = None, 0, 0, None, [], [], None
    survived = []

    def __init__(self, plan, fid, run_dir, log, *rest):
        self.run_dir, self.window, self.teardown_error = run_dir, None, None
        self.facts, self.preconditions = {}, []
        self.ctx = types.SimpleNamespace(scrub=lambda s: s, scrub_obj=lambda o: o)

    def preflight(self):
        return []

    def run_containers(self):
        return list(getattr(self, "labelled", []))

    def commit_facts(self):
        pass

    def remove_containers(self):
        FakeLocal.removed += 1
        return list(FakeLocal.cleanup_failures)

    def run(self):
        FakeLocal.ran += 1
        p = Pcap()
        p.open_ws(1.0)
        os.makedirs(os.path.join(self.run_dir, "capture"))
        p.write(os.path.join(self.run_dir, "capture", "relay.pcap"))
        p.write(os.path.join(self.run_dir, "capture", "relay.pcap.sanitizing"))
        if FakeLocal.sweep_mid_run is not None:
            fidelity_run.sweep_raw_captures(FakeLocal.sweep_mid_run)
            FakeLocal.survived.append(os.path.exists(os.path.join(self.run_dir, "capture", "relay.pcap")))
        self.window = FakeLocal.window
        if FakeLocal.fail is not None:
            raise FakeLocal.fail


SIGNAL_SCRIPT = r"""
import ctypes, glob, json, os, signal, sys, threading, time, types
HERE = sys.argv[1]
sys.path[:0] = [HERE, os.path.dirname(HERE)]
import fidelity_local, fidelity_run, ws_capture
import test_fidelity as T
REAL = fidelity_local.LocalRun
from fidelity_signals import STOP

phase, names, out_dir = sys.argv[2], sys.argv[3].split(","), sys.argv[4]
events, read = [], [0]


def send():
    for name in names:
        os.kill(os.getpid(), getattr(signal, name))
        time.sleep(0.02)
    events.append("sent")


def no_core_dump():
    # A piped core_pattern ignores RLIMIT_CORE; this keeps a test's deliberate fault out of the host's crash handler.
    try:
        ctypes.CDLL(None).prctl(4, 0, 0, 0, 0)
    except (OSError, AttributeError):
        pass


real_read = ws_capture.read_pcap


def reader(path):
    for i, rec in enumerate(real_read(path)):
        read[0] = i + 1
        if i == 10:
            send()
        yield rec


if phase in ("analysis", "analyze"):
    ws_capture.read_pcap, ws_capture.CHECK_EVERY_RECORDS = reader, 5


class Fake:
    def __init__(self, plan, fid, run_dir, log, *rest):
        self.run_dir, self.window, self.teardown_error, self.preconditions = run_dir, None, None, []
        self.facts = {"capture": {"packets_captured": 1, "packets_dropped_by_kernel": 0},
                      "host_resource_verdict": {"ok": True}}
        self.ctx = types.SimpleNamespace(scrub=lambda s: s, scrub_obj=lambda o: o)
        self.fleet = types.SimpleNamespace(poll=lambda: [])

    def preflight(self):
        return []

    def run_containers(self):
        return list(getattr(self, "labelled", []))

    def commit_facts(self):
        pass

    def run(self):
        cap = os.path.join(self.run_dir, "capture")
        os.makedirs(cap)
        T.AnalyzeTest.two_party_pcap(None).write(os.path.join(cap, "relay.pcap"))
        started = time.monotonic()
        try:
            if phase == "setup":
                threading.Timer(0.3, send).start()
                REAL.wait(self, "Rust bots joining", lambda: False, 60)
            if phase == "hold":
                threading.Timer(0.3, send).start()
                REAL.hold(self, 60)
            if phase == "teardown":
                send()
            if phase == "fault":
                no_core_dump()
                send()
                time.sleep(30)
        finally:
            events.append(["stopped after", round(time.monotonic() - started, 1)])
        self.window = (0.0, 10.0)

    def remove_containers(self):
        if phase == "cleanup":
            send()
        events.append("removed")
        return []


if phase == "retest":
    dirs = [os.path.join(out_dir, n) for n in ("a", "b")]
    doc = {"result": {"verdict": "FAIL"},
           "measures": {"participants": {"x": T.pm("rust", "t", "talker", T.kinds(audio=(50, 5000)), 1000.0)}}}
    for d in dirs:
        os.makedirs(d)
        fidelity_run.write_json(os.path.join(d, "fidelity.json"), doc)
    real_retest = fidelity_run.fidelity_tolerance.retest

    def retest(*args, **kwargs):
        send()
        return real_retest(*args, **kwargs)

    fidelity_run.fidelity_tolerance.retest = retest
    code = fidelity_run.main(["retest", *dirs])
    print(json.dumps({"code": code, "files": sorted(os.listdir(dirs[0])), "first": STOP.describe()}))
    sys.exit(0)
if phase == "analyze":
    run_dir = T.AnalyzeTest.run_dir(None, T.AnalyzeTest.two_party_pcap(None))
    code = fidelity_run.main(["analyze", run_dir])
else:
    fidelity_local.LocalRun = Fake
    code = fidelity_run.main(["run", "--out-dir", out_dir, "--min-free-gb", "0"])
    [run_dir] = glob.glob(os.path.join(out_dir, "*"))
run = fidelity_run.read_json(os.path.join(run_dir, "run.json"))
fid = os.path.join(run_dir, "fidelity.json")
print(json.dumps({"code": code, "events": events, "count": len(STOP.seen), "read": read[0],
                  "pcaps": glob.glob(os.path.join(run_dir, "capture", "relay.pcap*")),
                  "verdict": run.get("verdict"), "error": run.get("error"),
                  "fidelity_verdict": (fidelity_run.read_json(fid)["result"]["verdict"]
                                       if os.path.exists(fid) else None)}))
"""


EVERY_SIGNAL_SCRIPT = r"""
import json, os, resource, signal, sys, time
sys.path.insert(0, sys.argv[1])
from fidelity_signals import STOP, signal_name
unsent, unrecorded, no_handler_info = json.loads(sys.argv[2]), json.loads(sys.argv[3]), json.loads(sys.argv[4])
real_getsignal = signal.getsignal
signal.getsignal = lambda sig: None if int(sig) in no_handler_info else real_getsignal(sig)
hard = resource.getrlimit(resource.RLIMIT_CORE)[1]
resource.setrlimit(resource.RLIMIT_CORE, (hard if hard != resource.RLIM_INFINITY else 4096, hard))
STOP.install()
recorded, quiet, default = [], [], []
for sig in sorted(signal.valid_signals()):
    if int(sig) in unsent:
        default.append([signal_name(sig), signal.getsignal(sig) in (signal.SIG_DFL, None)])
        continue
    STOP.seen.clear()
    os.kill(os.getpid(), sig)
    end = time.monotonic() + (0.05 if int(sig) in unrecorded else 2)
    while not STOP.seen and time.monotonic() < end:
        time.sleep(0.001)
    (recorded if STOP.seen == [sig] else quiet).append(int(sig))
print(json.dumps({"recorded": recorded, "quiet": quiet, "default": default,
                  "core_limit": resource.getrlimit(resource.RLIMIT_CORE)[0]}))
"""


class RealSignalTest(unittest.TestCase):
    """Real signals sent to a child process running fidelity_run.main; only LocalRun's docker work is faked."""

    NEVER_SENT = ("SIGKILL", "SIGSTOP", "SIGTSTP", "SIGTTIN", "SIGTTOU", "SIGSEGV", "SIGBUS", "SIGILL", "SIGFPE",
                  "SIGABRT", "SIGSYS", "SIGTRAP", "SIGEMT")
    NOT_RECORDED = ("SIGPIPE", "SIGXFSZ", "SIGCHLD", "SIGCONT", "SIGURG", "SIGWINCH", "SIGINFO")

    def numbers(self, names):
        return [int(getattr(signal, n)) for n in names if hasattr(signal, n)]

    def test_every_signal_that_would_end_the_process_is_recorded_and_it_survives(self):
        self.check_every_signal([])

    def test_a_platform_that_reports_no_handler_for_kill_and_stop_passes(self):
        self.check_every_signal(self.numbers(("SIGKILL", "SIGSTOP")))

    def check_every_signal(self, no_handler_info):
        unsent, unrecorded = self.numbers(self.NEVER_SENT), self.numbers(self.NOT_RECORDED)
        p = subprocess.run([sys.executable, "-c", EVERY_SIGNAL_SCRIPT, HERE, json.dumps(unsent),
                            json.dumps(unrecorded), json.dumps(no_handler_info)], capture_output=True, text=True,
                           timeout=120, env=dict(os.environ, PYTHONDONTWRITEBYTECODE="1"))
        self.assertEqual(p.returncode, 0, p.stderr[-2000:])
        got = json.loads(p.stdout)
        self.assertEqual(sorted(got["quiet"]), sorted(unrecorded))
        self.assertEqual(sorted(got["recorded"]),
                         sorted(int(s) for s in signal.valid_signals() if int(s) not in unsent + unrecorded))
        self.assertIn(int(signal.SIGQUIT), got["recorded"])
        self.assertIn(int(signal.SIGUSR1), got["recorded"])
        for name, untouched in got["default"]:
            self.assertTrue(untouched, name)
        self.assertEqual(got["core_limit"], 0)

    def test_a_signal_python_has_no_name_for_is_described_by_number(self):
        stop = fidelity_signals.Stop()
        stop.handler(40, None)
        self.assertEqual(stop.describe(), "signal 40")
        with self.assertRaises(fidelity_signals.Interrupted):
            stop.check("x")

    def test_sigquit_and_the_user_and_alarm_signals_end_a_run_as_an_interrupt(self):
        for name in ("SIGQUIT", "SIGUSR1", "SIGUSR2", "SIGALRM"):
            for phase in ("hold", "analysis", "cleanup"):
                with self.subTest(name=name, phase=phase):
                    self.run_child(phase, [name])

    def test_a_kill_or_a_fatal_fault_leaves_the_capture_for_the_next_run_to_delete(self):
        for name in ("SIGKILL", "SIGSEGV", "SIGABRT"):
            with self.subTest(name=name):
                out_dir, cwd = tempfile.mkdtemp(), tempfile.mkdtemp()
                p = subprocess.run([sys.executable, "-c", SIGNAL_SCRIPT, HERE, "fault", name, out_dir],
                                   capture_output=True, text=True, timeout=120, cwd=cwd,
                                   env=dict(os.environ, PYTHONDONTWRITEBYTECODE="1", TMPDIR=tempfile.gettempdir()))
                self.assertEqual(p.returncode, -getattr(signal, name), p.stderr[-2000:])
                self.assertEqual(os.listdir(cwd), [])
                [run] = os.listdir(out_dir)
                pcap = os.path.join(out_dir, run, "capture", "relay.pcap")
                self.assertTrue(os.path.exists(pcap))
                self.assertEqual(fidelity_run.sweep_raw_captures(out_dir), [pcap])

    def run_child(self, phase, names):
        out_dir = tempfile.mkdtemp()
        p = subprocess.run([sys.executable, "-c", SIGNAL_SCRIPT, HERE, phase, ",".join(names), out_dir],
                           capture_output=True, text=True, timeout=120,
                           env=dict(os.environ, PYTHONDONTWRITEBYTECODE="1", TMPDIR=tempfile.gettempdir()))
        self.assertEqual(p.returncode, 0, p.stderr[-2000:])
        got = json.loads(p.stdout.strip().splitlines()[-1])
        self.assertEqual(got["code"], 3, got)
        self.assertEqual(got["pcaps"], [], got)
        self.assertEqual(got["verdict"], "ERROR", got)
        self.assertTrue(got["error"].startswith(f"interrupted by {names[0]}"), got)
        self.assertGreaterEqual(got["count"], 1)
        if phase != "analyze":
            self.assertIn("removed", got["events"], got)
        return got

    def test_a_signal_in_each_phase(self):
        for phase in ("setup", "hold", "analysis", "cleanup"):
            with self.subTest(phase=phase):
                got = self.run_child(phase, ["SIGTERM"])
                if phase in ("setup", "hold"):
                    stopped = next(e for e in got["events"] if isinstance(e, list))
                    self.assertLess(stopped[1], 5, got)
                if phase == "cleanup":
                    self.assertEqual(got["fidelity_verdict"], "ERROR", got)
                    self.assertEqual(got["events"][-2:], ["sent", "removed"], got)
                if phase == "analysis":
                    self.assertLess(got["read"], 20, got)

    def test_a_signal_while_the_run_tears_down_skips_the_analysis(self):
        got = self.run_child("teardown", ["SIGTERM"])
        self.assertIsNone(got["fidelity_verdict"], got)

    def test_mixed_pairs_and_a_burst(self):
        for names in (["SIGHUP", "SIGTERM"], ["SIGHUP", "SIGINT"], ["SIGTERM", "SIGINT"],
                      ["SIGTERM", "SIGINT", "SIGHUP", "SIGTERM", "SIGINT"]):
            for phase in ("hold", "cleanup"):
                with self.subTest(names=names, phase=phase):
                    self.run_child(phase, names)

    def test_retest_interrupted(self):
        out_dir = tempfile.mkdtemp()
        p = subprocess.run([sys.executable, "-c", SIGNAL_SCRIPT, HERE, "retest", "SIGTERM", out_dir],
                           capture_output=True, text=True, timeout=120,
                           env=dict(os.environ, PYTHONDONTWRITEBYTECODE="1", TMPDIR=tempfile.gettempdir()))
        got = json.loads(p.stdout.strip().splitlines()[-1])
        self.assertEqual((got["code"], got["files"], got["first"]), (3, ["fidelity.json"], "SIGTERM"), got)

    def test_standalone_analyze_interrupted(self):
        for names in (["SIGTERM"], ["SIGINT", "SIGHUP"]):
            with self.subTest(names=names):
                got = self.run_child("analyze", names)
                self.assertLess(got["read"], 20, got)


class CleanupRunTest(unittest.TestCase):
    def flagged(self):
        fidelity_signals.STOP.seen.append(signal.SIGTERM)
        self.addCleanup(fidelity_signals.STOP.seen.clear)

    def failing_unlink(self):
        real = os.remove

        def remove(path):
            if path.endswith("relay.pcap"):
                raise PermissionError(13, "Permission denied", path)
            real(path)

        os.remove = remove
        self.addCleanup(setattr, os, "remove", real)

    def test_a_signal_and_a_failed_unlink_in_analyze_end_as_error(self):
        d = AnalyzeTest.run_dir(None, AnalyzeTest.two_party_pcap(None))
        self.flagged()
        self.failing_unlink()
        self.assertEqual(fidelity_run.main(["analyze", d]), 3)
        run = fidelity_run.read_json(os.path.join(d, "run.json"))
        self.assertEqual(run["verdict"], "ERROR")
        self.assertTrue(run["error"].startswith("interrupted by SIGTERM; OSError: could not delete"), run["error"])
        self.assertIn("relay.pcap (Permission denied)", run["cleanup_errors"][0])
        self.assertEqual(fidelity_run.read_json(os.path.join(d, "fidelity.json"))["result"]["verdict"], "ERROR")

    def test_an_error_after_the_tables_leaves_no_page_with_the_earlier_verdict(self):
        d = AnalyzeTest.run_dir(None, AnalyzeTest.two_party_pcap(None))
        self.failing_unlink()
        self.assertEqual(fidelity_run.main(["analyze", d]), 3)
        self.assertFalse(os.path.exists(os.path.join(d, "table.txt")))
        self.assertFalse(os.path.exists(os.path.join(d, "table.md")))
        with open(os.path.join(d, "report.html"), encoding="utf-8") as fh:
            page = fh.read()
        self.assertIn("This run did not finish cleanly", page)
        self.assertIn("could not delete the raw capture", page)

    def test_a_signal_after_the_tables_leaves_no_page_with_the_earlier_verdict(self):
        d = AnalyzeTest.run_dir(None, AnalyzeTest.two_party_pcap(None))
        self.assertEqual(fidelity_run.analyze(d, out=lambda *_: None), 2)
        fidelity_run.write_report(d, lambda *_: None)
        self.flagged()
        self.assertEqual(fidelity_run.finish(d, 2, lambda *_: None), 3)
        for name in ("table.txt", "table.md", "report.html"):
            self.assertFalse(os.path.exists(os.path.join(d, name)), name)
        self.assertEqual(fidelity_run.read_json(os.path.join(d, "fidelity.json"))["result"]["verdict"], "ERROR")

    def test_a_table_that_cannot_be_removed_is_named(self):
        d = AnalyzeTest.run_dir(None, AnalyzeTest.two_party_pcap(None))
        fidelity_run.analyze(d, out=lambda *_: None)
        real = os.remove

        def remove(path):
            if path.endswith("table.txt"):
                raise PermissionError(13, "Permission denied", path)
            real(path)

        os.remove = remove
        self.addCleanup(setattr, os, "remove", real)
        out = []
        self.assertEqual(fidelity_run.finish(d, 2, out.append, "RuntimeError: boom"), 3)
        self.assertIn("warning: table.txt still shows the earlier verdict: Permission denied", out)
        self.assertFalse(os.path.exists(os.path.join(d, "table.md")))

    def test_a_failed_unlink_after_a_run_ends_as_error(self):
        FakeLocal.fail, FakeLocal.ran = fidelity_local.AdapterError("relay vanished"), 0
        real = fidelity_local.LocalRun
        fidelity_local.LocalRun = FakeLocal
        self.addCleanup(setattr, fidelity_local, "LocalRun", real)
        self.failing_unlink()
        out_dir = tempfile.mkdtemp()
        args = types.SimpleNamespace(scenario=fidelity_run.DEFAULT_SCENARIO, out_dir=out_dir, relay_container=None,
                                     capture_iface="eth0", min_free_gb=0)
        self.assertEqual(fidelity_run.cmd_run(args, out=lambda *_: None), 3)
        [run_dir] = [os.path.join(out_dir, n) for n in os.listdir(out_dir)]
        run = fidelity_run.read_json(os.path.join(run_dir, "run.json"))
        self.assertEqual(run["verdict"], "ERROR")
        self.assertIn("could not delete the raw capture", run["error"])
        self.assertIn("relay.pcap (Permission denied)", run["cleanup_errors"][0])

    def test_the_raw_capture_is_gone_before_the_table_prints(self):
        d = AnalyzeTest.run_dir(None, AnalyzeTest.two_party_pcap(None))
        seen = []
        fidelity_run.analyze(d, out=lambda text: seen.append(os.path.exists(os.path.join(d, "capture", "relay.pcap"))))
        self.assertEqual(seen, [False])

    def test_the_first_git_call_has_a_timeout(self):
        calls = []
        real = fidelity_run.subprocess.run

        def run(argv, **kwargs):
            calls.append(kwargs.get("timeout"))
            raise subprocess.TimeoutExpired(argv, kwargs.get("timeout"))

        fidelity_run.subprocess.run = run
        self.addCleanup(setattr, fidelity_run.subprocess, "run", real)
        with self.assertRaises(subprocess.TimeoutExpired):
            fidelity_run.cmd_run(types.SimpleNamespace(scenario=fidelity_run.DEFAULT_SCENARIO), out=lambda *_: None)
        self.assertEqual(calls, [fidelity_local.cq_local.DOCKER_S])

    def test_the_first_signal_is_kept_and_the_record_is_bounded(self):
        stop = fidelity_signals.Stop()
        self.assertEqual((stop.requested(), stop.describe()), (False, "no signal"))
        for sig in [signal.SIGTERM, signal.SIGINT] * 20:
            stop.handler(sig, None)
        self.assertEqual((stop.first, len(stop.seen), stop.describe()),
                         (signal.SIGTERM, fidelity_signals.MAX_RECORDED, "SIGTERM and later signals"))
        with self.assertRaises(fidelity_signals.Interrupted):
            stop.check("x")

    def test_a_usage_error_exits_3_not_incomplete(self):
        for argv in (["nosuch"], ["run", "--nosuch"], ["analyze"], ["retest", "a"]):
            with self.subTest(argv=argv), contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaises(SystemExit) as ctx:
                    fidelity_run.main(argv)
                self.assertEqual(ctx.exception.code, 3)

    def test_handlers_that_cannot_be_installed_end_as_error(self):
        def fail():
            raise OSError(22, "Invalid argument")

        fidelity_run.STOP.install = fail
        self.addCleanup(delattr, fidelity_run.STOP, "install")
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            self.assertEqual(fidelity_run.main(["retest", "a", "b"]), 3)
        self.assertIn("Invalid argument", err.getvalue())

    def test_a_failed_write_keeps_the_previous_file_whole(self):
        path = os.path.join(tempfile.mkdtemp(), "run.json")
        fidelity_run.write_json(path, {"schema": "kept"})
        real = fidelity_run.json.dump

        def partial(obj, fh, **kwargs):
            fh.write('{"schema": "cu')
            fh.flush()
            raise OSError(28, "No space left on device")

        fidelity_run.json.dump = partial
        self.addCleanup(setattr, fidelity_run.json, "dump", real)
        with self.assertRaises(OSError):
            fidelity_run.write_json(path, {"schema": "new"})
        fidelity_run.json.dump = real
        self.assertEqual(fidelity_run.read_json(path), {"schema": "kept"})

    def test_failures_are_recorded_before_a_failing_print(self):
        d = tempfile.mkdtemp()
        os.makedirs(os.path.join(d, "capture"))
        fidelity_run.write_json(os.path.join(d, "run.json"), {"plan": {}})

        def out(text):
            raise BrokenPipeError(32, "Broken pipe")

        local = types.SimpleNamespace(remove_containers=lambda: ["docker rm -f exited 1"])
        fidelity_run.cleanup_run(local, d, "r", out)
        self.assertEqual(fidelity_run.read_json(os.path.join(d, "run.json"))["cleanup_errors"],
                         ["docker rm -f exited 1"])

class CmdRunTest(unittest.TestCase):
    def setUp(self):
        self.real = fidelity_local.LocalRun
        fidelity_local.LocalRun = FakeLocal
        self.addCleanup(setattr, fidelity_local, "LocalRun", self.real)

    def args(self, out_dir):
        return types.SimpleNamespace(scenario=fidelity_run.DEFAULT_SCENARIO, out_dir=out_dir, relay_container=None,
                                     capture_iface="eth0", min_free_gb=0)

    def pcaps(self, out_dir):
        return [os.path.join(dp, f) for dp, _, fs in os.walk(out_dir) for f in fs if ".pcap" in f]

    def test_aborted_runs_leave_no_raw_pcap(self):
        for fail in (fidelity_local.AdapterError("relay vanished"), fidelity_signals.Interrupted("SIGTERM"),
                     KeyboardInterrupt()):
            FakeLocal.fail, FakeLocal.ran, FakeLocal.removed = fail, 0, 0
            out_dir = tempfile.mkdtemp()
            if isinstance(fail, KeyboardInterrupt):
                with self.assertRaises(KeyboardInterrupt):
                    fidelity_run.cmd_run(self.args(out_dir), out=lambda *_: None)
            else:
                self.assertEqual(fidelity_run.cmd_run(self.args(out_dir), out=lambda *_: None), 3)
            self.assertEqual((FakeLocal.ran, FakeLocal.removed), (1, 1))
            self.assertEqual(self.pcaps(out_dir), [], repr(fail))

    def test_an_interrupt_after_the_hold_is_error_and_not_analysed(self):
        for fail in (fidelity_signals.Interrupted("SIGTERM during leaving"),):
            FakeLocal.fail, FakeLocal.ran, FakeLocal.window = fail, 0, (0.0, 10.0)
            out_dir = tempfile.mkdtemp()
            try:
                self.assertEqual(fidelity_run.cmd_run(self.args(out_dir), out=lambda *_: None), 3)
            finally:
                FakeLocal.window = None
            [run_dir] = [os.path.join(out_dir, n) for n in os.listdir(out_dir)]
            self.assertIn("interrupted", fidelity_run.read_json(os.path.join(run_dir, "run.json"))["error"])
            self.assertFalse(os.path.exists(os.path.join(run_dir, "fidelity.json")))
            self.assertEqual(self.pcaps(out_dir), [])

    def test_an_unexpected_error_is_recorded_as_error(self):
        FakeLocal.fail, FakeLocal.ran, FakeLocal.removed = RuntimeError("boom"), 0, 0
        out_dir = tempfile.mkdtemp()
        self.assertEqual(fidelity_run.cmd_run(self.args(out_dir), out=lambda *_: None), 3)
        [run_dir] = [os.path.join(out_dir, n) for n in os.listdir(out_dir)]
        run = fidelity_run.read_json(os.path.join(run_dir, "run.json"))
        self.assertEqual((run["verdict"], run["error"], FakeLocal.removed), ("ERROR", "RuntimeError: boom", 1))
        self.assertEqual(self.pcaps(out_dir), [])

    def killed_run(self, out_dir):
        cap = os.path.join(out_dir, "fid-killed", "capture")
        os.makedirs(cap)
        fidelity_run.write_json(os.path.join(out_dir, "fid-killed", "run.json"), {"schema": fidelity_run.RUN_SCHEMA})
        for name in ("relay.pcap", "relay.pcap.part", "messages.csv.gz"):
            open(os.path.join(cap, name), "wb").close()
        return cap

    def test_a_raw_capture_left_by_a_killed_run_is_deleted_at_the_next_start(self):
        FakeLocal.fail, FakeLocal.ran = RuntimeError("boom"), 0
        out_dir = tempfile.mkdtemp()
        cap = self.killed_run(out_dir)
        out = []
        fidelity_run.cmd_run(self.args(out_dir), out=out.append)
        self.assertEqual((self.pcaps(out_dir), os.listdir(cap), FakeLocal.ran), ([], ["messages.csv.gz"], 1))
        self.assertIn(os.path.join(cap, "relay.pcap"), out[0])

    def test_a_live_run_keeps_its_capture_when_another_run_sweeps(self):
        out_dir = tempfile.mkdtemp()
        FakeLocal.fail, FakeLocal.sweep_mid_run, FakeLocal.survived = None, out_dir, []
        self.addCleanup(setattr, FakeLocal, "sweep_mid_run", None)
        fidelity_run.cmd_run(self.args(out_dir), out=lambda *_: None)
        self.assertEqual((FakeLocal.survived, self.pcaps(out_dir)), ([True], []))

    def test_a_run_keeps_its_capture_when_another_run_sweeps_mid_parse(self):
        out_dir = tempfile.mkdtemp()
        FakeLocal.fail, FakeLocal.window = None, (0.0, 10.0)
        self.addCleanup(setattr, FakeLocal, "window", None)
        real, swept = ws_capture.read_pcap, []

        def read_and_race(path):
            for i, rec in enumerate(real(path)):
                if i == 1:
                    swept.extend(fidelity_run.sweep_raw_captures(out_dir))
                yield rec

        ws_capture.read_pcap = read_and_race
        self.addCleanup(setattr, ws_capture, "read_pcap", real)
        self.assertEqual(fidelity_run.cmd_run(self.args(out_dir), out=lambda *_: None), 4)
        self.assertEqual((swept, self.pcaps(out_dir)), ([], []))

    def test_a_locked_run_folder_is_not_swept_and_an_unlocked_one_is(self):
        out_dir = tempfile.mkdtemp()
        cap = self.killed_run(out_dir)
        nested = os.path.join(out_dir, "folder", "fid-deeper", "capture")
        os.makedirs(nested)
        open(os.path.join(nested, "relay.pcap"), "wb").close()
        held = fidelity_run.lock_run(os.path.dirname(cap))
        self.assertEqual(fidelity_run.sweep_raw_captures(out_dir), [])
        held.close()
        self.assertEqual(fidelity_run.sweep_raw_captures(out_dir),
                         [os.path.join(cap, "relay.pcap"), os.path.join(cap, "relay.pcap.part")])
        self.assertEqual(os.listdir(nested), ["relay.pcap"])

    def test_the_sweep_lists_a_run_folders_files_in_name_order_whatever_the_directory_order(self):
        real = os.listdir
        self.addCleanup(setattr, os, "listdir", real)
        for order in (sorted, lambda xs: sorted(xs, reverse=True)):
            cap = self.killed_run(tempfile.mkdtemp())
            os.listdir = functools.partial(lambda order, path: order(real(path)), order)
            self.assertEqual(fidelity_run.sweep_raw_captures(os.path.dirname(os.path.dirname(cap))),
                             [os.path.join(cap, "relay.pcap"), os.path.join(cap, "relay.pcap.part")])
            os.listdir = real

    def test_the_sweep_skips_a_symlinked_run_folder(self):
        cap = self.killed_run(tempfile.mkdtemp())
        out_dir = tempfile.mkdtemp()
        os.symlink(os.path.dirname(cap), os.path.join(out_dir, "fid-link"))
        self.assertEqual(fidelity_run.sweep_raw_captures(out_dir), [])
        self.assertTrue(os.path.exists(os.path.join(cap, "relay.pcap")))

    def test_the_sweep_skips_a_symlinked_capture_folder(self):
        cap = self.killed_run(tempfile.mkdtemp())
        out_dir = tempfile.mkdtemp()
        os.makedirs(os.path.join(out_dir, "fid-link"))
        fidelity_run.write_json(os.path.join(out_dir, "fid-link", "run.json"), {"schema": fidelity_run.RUN_SCHEMA})
        os.symlink(cap, os.path.join(out_dir, "fid-link", "capture"))
        self.assertEqual(fidelity_run.sweep_raw_captures(out_dir), [])
        self.assertTrue(os.path.exists(os.path.join(cap, "relay.pcap")))

    def test_a_folder_without_a_raw_capture_is_not_locked(self):
        out_dir = tempfile.mkdtemp()
        cap = self.killed_run(out_dir)
        os.remove(os.path.join(cap, "relay.pcap"))
        os.remove(os.path.join(cap, "relay.pcap.part"))
        self.assertEqual(fidelity_run.sweep_raw_captures(out_dir), [])
        self.assertEqual(sorted(os.listdir(os.path.dirname(cap))), ["capture", "run.json"])

    def test_a_read_only_old_run_without_a_raw_capture_does_not_refuse_the_run(self):
        FakeLocal.fail, FakeLocal.ran = None, 0
        out_dir = tempfile.mkdtemp()
        cap = self.killed_run(out_dir)
        for name in ("relay.pcap", "relay.pcap.part"):
            os.remove(os.path.join(cap, name))
        old = os.path.dirname(cap)
        os.chmod(old, 0o555)
        self.addCleanup(os.chmod, old, 0o755)
        out = []
        fidelity_run.cmd_run(self.args(out_dir), out=out.append)
        self.assertEqual(FakeLocal.ran, 1, out)

    def chmod_for_test(self, path, mode):
        os.chmod(path, mode)
        self.addCleanup(os.chmod, path, 0o755)

    @unittest.skipIf(os.geteuid() == 0, "root ignores the file modes")
    def test_an_own_capture_folder_it_cannot_list_refuses_the_run(self):
        for mode, keep_pcap in ((0, True), (0o300, True), (0, False)):
            with self.subTest(mode=oct(mode), keep_pcap=keep_pcap):
                FakeLocal.ran = 0
                out_dir = tempfile.mkdtemp()
                cap = self.killed_run(out_dir)
                if not keep_pcap:
                    os.remove(os.path.join(cap, "relay.pcap"))
                    os.remove(os.path.join(cap, "relay.pcap.part"))
                self.chmod_for_test(cap, mode)
                out = []
                self.assertEqual(fidelity_run.cmd_run(self.args(out_dir), out=out.append), 3)
                self.assertEqual(FakeLocal.ran, 0)
                self.assertIn("refused", out[0])
                self.assertIn(f"cannot list {cap}", out[0])

    @unittest.skipIf(os.geteuid() == 0, "root ignores the file modes")
    def test_a_folder_that_is_not_an_own_unlistable_capture_does_not_refuse_the_run(self):
        FakeLocal.fail = None
        cases = (("no run.json", False, 0), ("another schema", "scale-run/v1", 0))
        for what, schema, mode in cases:
            with self.subTest(what):
                FakeLocal.ran = 0
                out_dir = tempfile.mkdtemp()
                cap = self.killed_run(out_dir)
                path = os.path.join(os.path.dirname(cap), "run.json")
                os.remove(path)
                if schema:
                    fidelity_run.write_json(path, {"schema": schema})
                self.chmod_for_test(cap, mode)
                fidelity_run.cmd_run(self.args(out_dir), out=lambda *_: None)
                self.assertEqual(FakeLocal.ran, 1)
        with self.subTest("own, read-only capture folder with no raw capture"):
            FakeLocal.ran = 0
            out_dir = tempfile.mkdtemp()
            cap = self.killed_run(out_dir)
            os.remove(os.path.join(cap, "relay.pcap"))
            os.remove(os.path.join(cap, "relay.pcap.part"))
            self.chmod_for_test(cap, 0o500)
            self.chmod_for_test(os.path.dirname(cap), 0o500)
            fidelity_run.cmd_run(self.args(out_dir), out=lambda *_: None)
            self.assertEqual(FakeLocal.ran, 1)

    @unittest.skipIf(os.geteuid() == 0, "root ignores the file modes")
    def test_a_run_folder_it_cannot_enter_is_named_in_a_warning(self):
        FakeLocal.fail, FakeLocal.ran = None, 0
        out_dir = tempfile.mkdtemp()
        run_dir = os.path.dirname(self.killed_run(out_dir))
        self.chmod_for_test(run_dir, 0)
        out = []
        fidelity_run.cmd_run(self.args(out_dir), out=out.append)
        self.assertEqual(FakeLocal.ran, 1)
        self.assertIn(f"cannot look inside {run_dir}", out[0])

    @unittest.skipIf(os.geteuid() == 0, "root ignores the file modes")
    def test_an_out_dir_it_cannot_list_refuses_the_run(self):
        FakeLocal.ran = 0
        out_dir = tempfile.mkdtemp()
        self.chmod_for_test(out_dir, 0o300)
        out = []
        self.assertEqual(fidelity_run.cmd_run(self.args(out_dir), out=out.append), 3)
        self.assertEqual(FakeLocal.ran, 0)
        self.assertIn("refused", out[0])

    @unittest.skipIf(os.geteuid() == 0, "root ignores the file modes")
    def test_cleanup_reports_a_capture_folder_it_cannot_list(self):
        d = tempfile.mkdtemp()
        cap = os.path.join(d, "capture")
        os.makedirs(cap)
        open(os.path.join(cap, "relay.pcap"), "wb").close()
        fidelity_run.write_json(os.path.join(d, "run.json"), {"plan": {}})
        self.chmod_for_test(cap, 0)
        local = types.SimpleNamespace(remove_containers=lambda: [])
        failures = fidelity_run.cleanup_run(local, d, "r", lambda *_: None)
        self.assertEqual(len(failures), 1)
        self.assertIn(f"could not delete the raw capture: cannot list {cap}", failures[0])

    @unittest.skipIf(os.geteuid() == 0, "root ignores the file modes")
    def test_an_unreadable_run_json_refuses_only_where_a_raw_capture_may_be(self):
        for what, cap_mode, keep_pcap, refused in (("raw capture", 0o700, True, True), ("unlistable", 0, False, True),
                                                   ("empty", 0o700, False, False)):
            with self.subTest(what):
                FakeLocal.fail, FakeLocal.ran = None, 0
                out_dir = tempfile.mkdtemp()
                cap = self.killed_run(out_dir)
                if not keep_pcap:
                    os.remove(os.path.join(cap, "relay.pcap"))
                    os.remove(os.path.join(cap, "relay.pcap.part"))
                self.chmod_for_test(os.path.join(os.path.dirname(cap), "run.json"), 0)
                self.chmod_for_test(cap, cap_mode)
                out = []
                code = fidelity_run.cmd_run(self.args(out_dir), out=out.append)
                self.assertEqual((code == 3 and "refused" in out[0], FakeLocal.ran), (refused, 0 if refused else 1))

    @unittest.skipIf(os.geteuid() == 0, "root ignores the file modes")
    def test_a_raw_capture_in_a_folder_it_cannot_lock_or_read_refuses_the_run(self):
        for what in ("the folder", "run.json"):
            with self.subTest(what):
                FakeLocal.ran = 0
                out_dir = tempfile.mkdtemp()
                cap = self.killed_run(out_dir)
                path = os.path.dirname(cap) if what == "the folder" else os.path.join(os.path.dirname(cap), "run.json")
                os.chmod(path, 0o555 if what == "the folder" else 0)
                self.addCleanup(os.chmod, path, 0o755)
                out = []
                self.assertEqual(fidelity_run.cmd_run(self.args(out_dir), out=out.append), 3)
                self.assertEqual(FakeLocal.ran, 0)
                self.assertIn("refused", out[0])

    def bounded(self, fn):
        """fn's result; fails the test if it blocks."""
        result = []
        th = threading.Thread(target=lambda: result.append(fn()), daemon=True)
        th.start()
        th.join(5)
        self.assertFalse(th.is_alive(), "blocked")
        return result[0]

    def write_bytes(self, path, data):
        with open(path, "wb") as fh:
            fh.write(data)

    def test_the_sweep_leaves_another_tools_folder_alone(self):
        def write(path, text):
            with open(path, "w", encoding="utf-8") as fh:
                fh.write(text)

        def symlink(path):
            target = os.path.join(tempfile.mkdtemp(), "run.json")
            write(target, json.dumps({"schema": fidelity_run.RUN_SCHEMA}))
            os.symlink(target, path)

        for what, make in (("no run.json", lambda path: None),
                           ("another schema", lambda path: write(path, '{"schema": "scale-run/v1"}')),
                           ("no schema", lambda path: write(path, "{}")),
                           ("not an object", lambda path: write(path, "[1]")),
                           ("not JSON", lambda path: write(path, '{"sche')),
                           ("not UTF-8", lambda path: self.write_bytes(path, b"\xff\xfe")),
                           ("a FIFO", os.mkfifo),
                           ("a symlink", symlink)):
            with self.subTest(what):
                out_dir = tempfile.mkdtemp()
                cap = self.killed_run(out_dir)
                path = os.path.join(os.path.dirname(cap), "run.json")
                os.remove(path)
                make(path)
                self.assertEqual(self.bounded(functools.partial(fidelity_run.sweep_raw_captures, out_dir)), [])
                self.assertTrue(os.path.exists(os.path.join(cap, "relay.pcap")))
                self.assertFalse(os.path.exists(os.path.join(os.path.dirname(cap), ".lock")))

    def test_a_fifo_named_like_the_lock_does_not_block(self):
        d = tempfile.mkdtemp()
        os.mkfifo(os.path.join(d, ".lock"))

        def lock():
            try:
                return fidelity_run.lock_run(d)
            except OSError as exc:
                return exc

        self.assertIsInstance(self.bounded(lock), OSError)

    def test_the_run_lock_does_not_follow_a_symlink(self):
        d, elsewhere = tempfile.mkdtemp(), os.path.join(tempfile.mkdtemp(), "target")
        os.symlink(elsewhere, os.path.join(d, ".lock"))
        with self.assertRaises(OSError):
            fidelity_run.lock_run(d)
        self.assertFalse(os.path.exists(elsewhere))

    def test_a_run_refuses_a_run_folder_it_cannot_lock(self):
        FakeLocal.ran = 0
        self.addCleanup(setattr, fidelity_run, "lock_run", fidelity_run.lock_run)
        fidelity_run.lock_run = lambda run_dir: None
        out = []
        self.assertEqual(fidelity_run.cmd_run(self.args(tempfile.mkdtemp()), out=out.append), 3)
        self.assertIn("another process holds the new run folder", out[-1])
        self.assertEqual(FakeLocal.ran, 0)

    def test_no_sweep_while_a_run_may_be_live(self):
        out_dir = tempfile.mkdtemp()
        cap = self.killed_run(out_dir)
        FakeLocal.ran, FakeLocal.labelled = 0, ["abc"]
        self.addCleanup(setattr, FakeLocal, "labelled", [])
        out = []
        self.assertEqual(fidelity_run.cmd_run(self.args(out_dir), out=out.append), 3)
        self.assertIn("appeared during preflight", out[0])
        FakeLocal.labelled = []
        refusal = ["1 container(s) labelled cq.run_id exist; another run may be going"]
        FakeLocal.preflight = lambda self: refusal
        self.addCleanup(setattr, FakeLocal, "preflight", lambda self: [])
        self.assertEqual(fidelity_run.cmd_run(self.args(out_dir), out=out.append), 3)
        self.assertEqual((sorted(os.listdir(cap)), FakeLocal.ran),
                         (["messages.csv.gz", "relay.pcap", "relay.pcap.part"], 0))

    def test_a_raw_capture_that_cannot_be_swept_refuses_the_run(self):
        FakeLocal.ran = 0
        out_dir = tempfile.mkdtemp()
        self.killed_run(out_dir)
        CleanupRunTest.failing_unlink(self)
        out = []
        self.assertEqual(fidelity_run.cmd_run(self.args(out_dir), out=out.append), 3)
        self.assertEqual((FakeLocal.ran, sorted(os.listdir(out_dir))), (0, ["fid-killed"]))
        self.assertIn("relay.pcap (Permission denied)", out[0])

    def test_relative_out_dir_that_does_not_exist(self):
        found = []
        th = threading.Thread(target=lambda: found.append(fidelity_local.existing_parent("no/such/dir/yet")),
                              daemon=True)
        th.start()
        th.join(5)
        self.assertFalse(th.is_alive(), "existing_parent did not return")
        self.assertEqual(found, [os.getcwd()])


class VerdictAgreementTest(unittest.TestCase):
    PAGES = ("table.txt", "table.md", "report.html")

    def setUp(self):
        self.addCleanup(fidelity_signals.STOP.seen.clear)
        self.output = io.StringIO()

    def main(self, *argv):
        with contextlib.redirect_stdout(self.output), contextlib.redirect_stderr(self.output):
            code = fidelity_run.main(list(argv))
        while fidelity_run.HELD:
            fidelity_run.HELD.pop().close()
        return code

    def analysed(self):
        d = AnalyzeTest.run_dir(None, AnalyzeTest.two_party_pcap(None))
        self.assertEqual(self.main("analyze", d), 2)
        return d

    def patch(self, owner, name, value):
        self.addCleanup(setattr, owner, name, getattr(owner, name))
        setattr(owner, name, value)

    def fail_writes(self, name, when=lambda: True):
        real = fidelity_run.write_json

        def write_json(path, obj):
            if path.endswith(name) and when():
                raise OSError(errno.ENOSPC, "No space left on device")
            real(path, obj)
        self.patch(fidelity_run, "write_json", write_json)

    def says(self, d):
        """{artifact: the verdict it states}, for the artifacts that exist."""
        out = {}
        for name, read in (("run.json", lambda t: json.loads(t).get("verdict")),
                           ("fidelity.json", lambda t: json.loads(t)["result"]["verdict"]),
                           ("table.txt", lambda t: re.search(r"^Verdict: ([A-Z]+)", t, re.M).group(1)),
                           ("table.md", lambda t: re.search(r"^Verdict: ([A-Z]+)", t, re.M).group(1)),
                           ("report.html", lambda t: re.search(r'class="verdict"><span class="chip [a-z-]+"><i></i>'
                                                               r"([A-Z]+)<", t).group(1))):
            path = os.path.join(d, name)
            if os.path.exists(path):
                with open(path, encoding="utf-8") as fh:
                    out[name] = read(fh.read())
        return out

    def assert_error(self, d, code, run_json=True, fidelity=True):
        self.assertEqual(code, 3)
        want = ({"run.json": "ERROR"} if run_json else {})
        want.update({"fidelity.json": "ERROR", "report.html": "ERROR"} if fidelity else {})
        said = self.says(d)
        if not run_json:
            said.pop("run.json", None)
        self.assertEqual(said, want)

    def test_a_clean_analysis_agrees_everywhere(self):
        d = self.analysed()
        self.assertEqual(self.says(d), dict.fromkeys(("run.json", "fidelity.json") + self.PAGES, "INCOMPLETE"))

    def test_a_completed_analysis_replaces_an_earlier_error(self):
        d = self.analysed()
        self.assertEqual(fidelity_run.finish(d, 2, lambda *_: None, "RuntimeError: boom"), 3)
        self.assertEqual(self.main("analyze", d), 2)
        self.assertEqual(self.says(d), dict.fromkeys(("run.json", "fidelity.json") + self.PAGES, "INCOMPLETE"))
        self.assertNotIn("error", fidelity_run.read_json(os.path.join(d, "run.json")))

    def test_a_report_that_cannot_be_written_leaves_no_earlier_page(self):
        d = self.analysed()

        def full(*_):
            raise OSError(errno.ENOSPC, "No space left on device")
        self.patch(fidelity_report, "write", full)
        self.assertEqual(self.main("analyze", d), 2)
        self.assertNotIn("report.html", self.says(d))
        self.assertIn("No space left", fidelity_run.read_json(os.path.join(d, "run.json"))["report_error"])

    def test_a_malformed_result_leaves_report_with_no_page(self):
        d = self.analysed()
        with open(os.path.join(d, "fidelity.json"), "w", encoding="utf-8") as fh:
            fh.write("{")
        self.assertEqual(self.main("report", d), 3)
        self.assertFalse(os.path.exists(os.path.join(d, "report.html")))

    def test_report_refuses_a_folder_another_process_holds(self):
        d = self.analysed()
        held = fidelity_run.lock_run(d)
        self.addCleanup(held.close)
        real = fidelity_report.write
        calls = []
        self.patch(fidelity_report, "write", lambda *a: calls.append(a) or real(*a))
        self.assertEqual(self.main("report", d), 3)
        self.assertEqual(calls, [])

    def test_report_holds_the_lock_while_it_writes(self):
        d = self.analysed()
        real = fidelity_report.write
        locked = []

        def write(*a):
            locked.append(fidelity_run.lock_run(d))
            return real(*a)
        self.patch(fidelity_report, "write", write)
        self.assertEqual(self.main("report", d), 0)
        self.assertEqual(locked, [None])

    def signal_while_rendering(self):
        real = fidelity_report.render
        sent = []

        def render(*a):
            if not sent:
                sent.append(1)
                fidelity_signals.STOP.handler(signal.SIGTERM, None)
            return real(*a)
        self.patch(fidelity_report, "render", render)

    def test_a_signal_in_the_renderer_ends_analyze_as_error(self):
        d = AnalyzeTest.run_dir(None, AnalyzeTest.two_party_pcap(None))
        self.signal_while_rendering()
        self.assert_error(d, self.main("analyze", d))

    def test_a_signal_in_the_renderer_ends_run_as_error(self):
        FakeLocal.fail, FakeLocal.ran = None, 0
        self.patch(fidelity_local, "LocalRun", FakeLocal)
        done = self.analysed()
        self.signal_while_rendering()

        def analysed(local, run, run_dir, args, out):
            shutil.copytree(done, run_dir, dirs_exist_ok=True)
            return 2
        self.patch(fidelity_run, "_run_and_analyze", analysed)
        out_dir = tempfile.mkdtemp()
        args = types.SimpleNamespace(scenario=fidelity_run.DEFAULT_SCENARIO, out_dir=out_dir, relay_container=None,
                                     capture_iface="eth0", min_free_gb=0)
        code = fidelity_run.cmd_run(args, out=lambda *_: None)
        [d] = [os.path.join(out_dir, n) for n in os.listdir(out_dir)]
        self.assert_error(d, code)

    def test_a_signal_in_report_exits_3_and_keeps_the_runs_verdict(self):
        d = self.analysed()
        self.signal_while_rendering()
        self.assertEqual(self.main("report", d), 3)
        self.assertIn("interrupted by SIGTERM", self.output.getvalue())
        self.assertEqual(self.says(d), dict.fromkeys(("run.json", "fidelity.json") + self.PAGES, "INCOMPLETE"))

    def test_a_run_json_that_cannot_be_written_does_not_save_the_pages(self):
        d = self.analysed()
        self.fail_writes("run.json")
        out = []
        self.assertEqual(fidelity_run.finish(d, 2, out.append, "RuntimeError: boom"), 3)
        self.assertEqual(self.says(d), {"run.json": "INCOMPLETE", "fidelity.json": "ERROR"})
        self.assertTrue(any(line.startswith("warning: run.json does not say ERROR") for line in out))

    def test_a_fidelity_json_that_cannot_be_written_is_removed(self):
        d = self.analysed()
        self.fail_writes("fidelity.json")
        self.assertEqual(fidelity_run.finish(d, 2, lambda *_: None, "RuntimeError: boom"), 3)
        self.assertEqual(self.says(d), {"run.json": "ERROR"})

    def test_an_analysis_whose_verdict_cannot_be_recorded_is_error(self):
        d = self.analysed()
        self.fail_writes("run.json")
        code = self.main("analyze", d)
        self.assertEqual(code, 3)
        self.assertEqual(self.says(d)["fidelity.json"], "ERROR")
        self.assertEqual({n for n in self.PAGES if os.path.exists(os.path.join(d, n))}, {"report.html"})
        self.assertEqual(self.says(d)["report.html"], "ERROR")

    def test_cleanup_that_cannot_record_still_reaches_finish(self):
        d = self.analysed()
        self.fail_writes("run.json")
        local = types.SimpleNamespace(remove_containers=lambda: ["docker rm -f exited 1"])
        failures = fidelity_run.cleanup_run(local, d, "r", lambda *_: None)
        self.assertEqual(failures[0], "docker rm -f exited 1")
        self.assertIn("recording the cleanup errors", failures[1])

    def test_a_missing_capture_ends_as_error_everywhere(self):
        d = self.analysed()
        os.remove(os.path.join(d, "capture", "messages.csv.gz"))
        self.assert_error(d, self.main("analyze", d))
        self.assertEqual(fidelity_run.read_json(os.path.join(d, "run.json"))["error"],
                         "no capture (capture/relay.pcap) to analyse")

    def test_a_missing_window_ends_as_error_everywhere(self):
        d = self.analysed()
        run = fidelity_run.read_json(os.path.join(d, "run.json"))
        del run["window"]
        fidelity_run.write_json(os.path.join(d, "run.json"), run)
        self.assert_error(d, self.main("analyze", d))
        self.assertIn("no hold window", fidelity_run.read_json(os.path.join(d, "run.json"))["error"])

    def test_report_bookkeeping_never_changes_the_exit_code(self):
        d = self.analysed()
        fidelity_run.record(d, report_error="KeyError: 'rows'")
        real = fidelity_report.write
        wrote = []
        self.patch(fidelity_report, "write", lambda *a: wrote.append(1) or real(*a))
        self.fail_writes("run.json", when=lambda: bool(wrote))
        self.assertEqual(self.main("analyze", d), 2)
        self.assertEqual(self.says(d)["report.html"], "INCOMPLETE")
        self.assertIn("run.json keeps an old report_error", self.output.getvalue())

    def test_a_run_with_no_result_writes_no_report_warning(self):
        d = AnalyzeTest.run_dir(None, AnalyzeTest.two_party_pcap(None))
        out = []
        self.assertEqual(fidelity_run.conclude(d, 3, out.append, "RuntimeError: boom"), 3)
        self.assertEqual(out, ["ERROR: RuntimeError: boom"])
        self.assertNotIn("report_error", fidelity_run.read_json(os.path.join(d, "run.json")))
        self.assertFalse(os.path.exists(os.path.join(d, "report.html")))

    def test_report_with_no_result_says_why(self):
        d = AnalyzeTest.run_dir(None, AnalyzeTest.two_party_pcap(None))
        self.assertEqual(self.main("report", d), 3)
        self.assertIn("no fidelity.json, so there is no result to report", self.output.getvalue())
        self.assertNotIn("report_error", fidelity_run.read_json(os.path.join(d, "run.json")))

    def foreign(self, run_json):
        d = tempfile.mkdtemp()
        if run_json is not None:
            fidelity_run.write_json(os.path.join(d, "run.json"), run_json)
        for name in self.PAGES + ("fidelity.json",):
            with open(os.path.join(d, name), "w", encoding="utf-8") as fh:
                fh.write("another tool's")
        return d, sorted(os.listdir(d))

    def test_a_folder_of_another_tool_is_left_alone(self):
        for run_json in ({"schema": "another-tool/v2"}, None):
            for cmd in ("analyze", "report"):
                d, before = self.foreign(run_json)
                self.assertEqual(self.main(cmd, d), 3)
                self.assertEqual(sorted(os.listdir(d)), before)
                with open(os.path.join(d, "report.html"), encoding="utf-8") as fh:
                    self.assertEqual(fh.read(), "another tool's")

    def in_parent(self, d):
        self.addCleanup(os.chdir, os.getcwd())
        os.chdir(os.path.dirname(d))
        return os.path.basename(d)

    def page(self, d):
        with open(os.path.join(d, "report.html"), encoding="utf-8") as fh:
            return fh.read()

    @unittest.skipIf(os.geteuid() == 0, "root reads any file")
    def test_a_relative_run_folder_is_not_on_the_page_of_a_read_error(self):
        d = self.analysed()
        conns = os.path.join(d, "capture", "conns.json")
        os.chmod(conns, 0)
        self.addCleanup(os.chmod, conns, 0o600)
        name = self.in_parent(d)
        for form in (f"./{name}", name):
            self.assert_error(d, self.main("analyze", form))
            self.assertIn("Permission denied", self.page(d))
            self.assertNotIn(name, self.page(d))

    def test_a_relative_run_folder_is_not_on_the_page_of_a_failed_delete(self):
        d = AnalyzeTest.run_dir(None, AnalyzeTest.two_party_pcap(None))
        real = os.remove

        def remove(path):
            if path.endswith("relay.pcap"):
                raise PermissionError(13, "Permission denied", path)
            real(path)
        self.patch(os, "remove", remove)
        name = self.in_parent(d)
        self.assert_error(d, self.main("analyze", f"./{name}"))
        self.assertIn("could not delete the raw capture", self.page(d))
        self.assertNotIn(name, self.page(d))

    @unittest.skipIf(os.geteuid() == 0, "root reads any file")
    def test_an_ordinary_error_shows_no_absolute_path_on_the_page(self):
        d = self.analysed()
        conns = os.path.join(d, "capture", "conns.json")
        os.chmod(conns, 0)
        self.addCleanup(os.chmod, conns, 0o600)
        self.assert_error(d, self.main("analyze", d))
        self.assertIn(conns, fidelity_run.read_json(os.path.join(d, "run.json"))["error"])
        with open(os.path.join(d, "report.html"), encoding="utf-8") as fh:
            page = fh.read()
        self.assertNotIn(d, page)
        self.assertIn("Permission denied", page)


class ReadsCorrectlyLaterTest(unittest.TestCase):
    PAGES, setUp, main, analysed = (VerdictAgreementTest.PAGES, VerdictAgreementTest.setUp, VerdictAgreementTest.main,
                                    VerdictAgreementTest.analysed)
    DEVELOPMENT = re.compile(r"#\d+|\b[FTW]\d\b|[Ww]ork item|Done when|harness version|this version"
                             r"|not implemented|\bstub\b|\byet\b")

    def assert_clean(self, text):
        text = re.sub(r"<style>.*?</style>", "", text, flags=re.S)
        self.assertEqual(self.DEVELOPMENT.findall(text), [])

    def assert_folder_clean(self, d):
        for name in self.PAGES:
            path = os.path.join(d, name)
            if os.path.exists(path):
                with open(path, encoding="utf-8") as fh:
                    self.assert_clean(fh.read())

    def test_a_page_and_table_for_every_kind_of_result(self):
        with open(os.path.join(HERE, "testdata", "room10-invalid.json"), encoding="utf-8") as fh:
            fixture = json.load(fh)

        def case(**result):
            doc = json.loads(json.dumps(fixture["fidelity"]))
            doc["result"].update(result)
            return doc
        for doc in (case(), case(verdict="FAIL", problems=[]), case(verdict="ERROR", interrupted="RuntimeError: boom"),
                    case(verdict="INCOMPLETE", problems=[], sizing_blockers=[])):
            self.assert_clean(fidelity_report.render(doc, fixture["run"]))
            self.assert_clean(ft.render(doc["result"], []))
            self.assert_clean(ft.render(doc["result"], [], markdown=True))

    def test_a_run_folder_from_a_real_analysis(self):
        d = self.analysed()
        self.assert_folder_clean(d)
        self.assert_clean(self.output.getvalue())
        invalid = AnalyzeTest.run_dir(None, AnalyzeTest.two_party_pcap(None), preconditions=["the capture dropped 2"])
        self.assertEqual(self.main("analyze", invalid), 4)
        self.assert_folder_clean(invalid)
        self.assertEqual(fidelity_run.finish(d, 2, lambda *_: None, "RuntimeError: boom"), 3)
        fidelity_run.write_report(d, lambda *_: None)
        self.assert_folder_clean(d)

    def test_what_the_commands_print(self):
        self.assertEqual(self.main("remote"), 3)
        self.assert_clean(self.output.getvalue())
        self.assert_clean(fidelity_run.build_parser().format_help())
        self.assert_clean(ws_capture.__doc__)


class RemoveRawCaptureTest(unittest.TestCase):
    def test_every_file_is_tried_and_failures_are_named(self):
        d = tempfile.mkdtemp()
        for name in ("relay.pcap", "relay.pcap.part"):
            open(os.path.join(d, name), "wb").close()
        real = os.remove

        def remove(path):
            if path.endswith("relay.pcap"):
                raise PermissionError(13, "Permission denied", path)
            real(path)

        os.remove = remove
        try:
            with self.assertRaises(OSError) as ctx:
                fidelity_run.remove_raw_capture(d)
        finally:
            os.remove = real
        self.assertIn("could not delete the raw capture", str(ctx.exception))
        self.assertIn(os.path.join(d, "relay.pcap") + " (Permission denied)", str(ctx.exception))
        self.assertEqual(os.listdir(d), ["relay.pcap"])

class ScenarioTest(unittest.TestCase):
    def test_shipped_scenario_compiles(self):
        plan, fid = fidelity_run.compile_run(fidelity_run.DEFAULT_SCENARIO, "0123456789abcdef")
        self.assertEqual((fid["browser_simulcast_max_layers"], fid["browser_hardware_concurrency"]), (3, 10))
        self.assertEqual(fidelity_run.expected_layers(plan, fid)["btalk-00"], 3)

    def test_settings_refused(self):
        _, errors = fidelity_run.fidelity_settings({"fidelity": {"browser_simulcast_max_layers": 4, "x": 1,
                                                                 "browser_hardware_concurrency": 0,
                                                                 "browser_min_video_fps": [1, 2, 3, 4],
                                                                 "browser_source": {"bt": "800x600"}}})
        self.assertEqual(len(errors), 5)
        _, errors = fidelity_run.fidelity_settings({})
        self.assertTrue(errors)

    def test_plan_refusals(self):
        plan = {"target": "local", "events": [{"x": 1}], "participants": [part("a", "rust", "r")],
                "processes": [{"proc_id": "probe-00", "fleet": "browser", "transport": "auto"}]}
        errors = fidelity_run.plan_errors(plan, {"browser_source": {"ghost": "640x480"}})
        self.assertEqual(len(errors), 4)


class AnalyzeTest(unittest.TestCase):
    def run_dir(self, pcap=None, preconditions=()):
        d = tempfile.mkdtemp()
        os.makedirs(os.path.join(d, "capture"))
        parts = [part("talk-00", "rust", "talk", True, True), part("bt-00", "browser", "bt", True, True)]
        for p in parts:
            p["process"] = "rust-pub" if p["fleet"] == "rust" else "probe-00"
        plan = {"run_id": "r", "meeting_id": "scale-r", "participants": parts, "video_mode": "replay",
                "bot_diagnostics": "off", "processes": [{"proc_id": "rust-pub", "video_layers": 1},
                                                        {"proc_id": "probe-00", "video_layers": None}]}
        fidelity_run.write_json(os.path.join(d, "run.json"), {
            "schema": fidelity_run.RUN_SCHEMA, "plan": plan,
            "fidelity": {"browser_simulcast_max_layers": 1, "browser_source": {}, "settle_s": 0},
            "window": [0, 10], "facts": {"capture": {"packets_captured": 100, "packets_dropped_by_kernel": 0},
                                         "host_resource_verdict": {"ok": True}},
            "preconditions": list(preconditions)})
        if pcap is not None:
            pcap.write(os.path.join(d, "capture", "relay.pcap"))
        return d

    def analyzed(self, *messages):
        """(exit code, fidelity.json) of two_party_pcap plus (connection pcap, message) added inside the hold."""
        p = self.two_party_pcap()
        for port, payload in messages:
            extra = Pcap(port)
            extra.open_ws(0.0)
            extra.send(0.4, "u", ws_frame(payload, mask=MASK))
            p.records = sorted(p.records + extra.records, key=lambda r: r[0])
        self.analyzed_dir = self.run_dir(p)
        code = fidelity_run.analyze(self.analyzed_dir, out=lambda *_: None)
        return code, fidelity_run.read_json(os.path.join(self.analyzed_dir, "fidelity.json"))

    def test_a_run_id_with_a_nul_is_traffic_from_outside_the_run(self):
        code, clean = self.analyzed()
        self.assertEqual(code, 2)
        before = clean["measures"]["participants"]["talk-00"]["up_total"]["pps"]
        code, doc = self.analyzed((40002, audio("r-talk-00\0", 1)))
        self.assertEqual(code, 4)
        self.assertEqual(doc["result"]["verdict"], "INVALID")
        self.assertIn("1 user(s) outside this run used the relay during the hold", doc["result"]["problems"])
        self.assertEqual(doc["measures"]["participants"]["talk-00"]["up_total"]["pps"], before)

    def test_a_connection_that_never_sends_a_user_id_is_judged(self):
        _, clean = self.analyzed()
        self.assertEqual(clean["measures"]["unidentified_messages_in_window"], 0)
        no_user = pb((1, 3), (3, pb((1, 2), (3, b"\x01" * 40), (7, pb((5, 1))))))
        code, doc = self.analyzed((40002, no_user))
        self.assertEqual(code, 4)
        self.assertEqual(doc["measures"]["unidentified_messages_in_window"], 1)
        self.assertIn("no decoded user id", " ".join(doc["result"]["problems"]))
        with open(os.path.join(self.analyzed_dir, "table.txt"), encoding="utf-8") as fh:
            self.assertIn("messages in the hold with no user id 1\n", fh.read())

    def test_an_unknown_frame_type_invalidates_the_run_and_counts_no_keyframe(self):
        _, clean = self.analyzed()
        media = pb((1, 1), (2, "r-talk-00"), (3, b"\x11" * 10), (4, "ke\0y"))
        p = self.two_party_pcap()
        extra = Pcap(40002)
        extra.open_ws(0.0)
        extra.send(0.4, "u", ws_frame(pb((1, 3), (2, "r-talk-00"), (3, media), (5, 0)), mask=MASK))
        p.records = sorted(p.records + extra.records, key=lambda r: r[0])
        d = self.run_dir(p)
        self.assertEqual(fidelity_run.analyze(d, out=lambda *_: None), 4)
        doc = fidelity_run.read_json(os.path.join(d, "fidelity.json"))
        self.assertIn("capture: 1 messages with a frame type or tier outside the known values",
                      doc["result"]["problems"])
        self.assertEqual(doc["measures"]["participants"]["talk-00"]["keyframes"],
                         clean["measures"]["participants"]["talk-00"]["keyframes"])

    def test_an_unknown_tier_on_a_known_frame_type_invalidates_the_run(self):
        media = pb((1, 1), (2, "r-talk-00"), (3, b"\x11" * 10), (4, "delta"), (8, pb((6, "lo\0w"))))
        code, doc = self.analyzed((40002, pb((1, 3), (2, "r-talk-00"), (3, media), (5, 0))))
        self.assertEqual(code, 4)
        self.assertIn("capture: 1 messages with a frame type or tier outside the known values",
                      doc["result"]["problems"])

    def test_missing_capture_is_error(self):
        self.assertEqual(fidelity_run.analyze(self.run_dir(), out=lambda *_: None), 3)

    def test_empty_capture_is_invalid(self):
        d = self.run_dir(Pcap())
        out = []
        self.assertEqual(fidelity_run.analyze(d, out=out.append), 4)
        self.assertIn("Verdict: INVALID", out[0])
        self.assertIn("no WebSocket messages", out[0])
        self.assertFalse(os.path.exists(os.path.join(d, "capture", "relay.pcap")))

    def test_retest_refuses_invalid_runs(self):
        dirs = [self.run_dir(Pcap()) for _ in range(2)]
        for d in dirs:
            fidelity_run.analyze(d, out=lambda *_: None)
        args = types.SimpleNamespace(a=dirs[0], b=dirs[1], tol=0.05)
        out = []
        self.assertEqual(fidelity_run.cmd_retest(args, out=out.append), 3)
        self.assertIn("verdict INVALID", out[0])

    def test_retest_refuses_a_run_that_ended_in_error(self):
        dirs = [self.run_dir(self.two_party_pcap()) for _ in range(2)]
        for d in dirs:
            self.assertEqual(fidelity_run.analyze(d, out=lambda *_: None), 2)
        args = types.SimpleNamespace(a=dirs[0], b=dirs[1], tol=0.05)
        self.assertEqual(fidelity_run.cmd_retest(args, out=lambda *_: None), 0)
        fidelity_run.finish(dirs[1], 2, lambda *_: None, "RuntimeError: boom")
        out = []
        self.assertEqual(fidelity_run.cmd_retest(args, out=out.append), 3)
        self.assertIn("verdict ERROR", out[0])

    def test_a_sweep_during_a_standalone_analyze_leaves_its_capture(self):
        out_dir = tempfile.mkdtemp()
        d = os.path.join(out_dir, "fid-A")
        shutil.move(self.run_dir(self.two_party_pcap()), d)
        real, swept = ws_capture.read_pcap, []

        def read_and_race(path):
            for i, rec in enumerate(real(path)):
                if i == 5:
                    swept.extend(fidelity_run.sweep_raw_captures(out_dir))
                yield rec

        ws_capture.read_pcap = read_and_race
        self.addCleanup(setattr, ws_capture, "read_pcap", real)
        self.assertEqual(fidelity_run.main(["analyze", d]), 2)
        self.assertEqual(swept, [])

    def test_analyze_refuses_a_run_folder_another_process_holds(self):
        d = self.run_dir(self.two_party_pcap())
        held = fidelity_run.lock_run(d)
        self.addCleanup(held.close)
        self.assertEqual(fidelity_run.main(["analyze", d]), 3)
        self.assertTrue(os.path.exists(os.path.join(d, "capture", "relay.pcap")))
        self.assertNotIn("verdict", fidelity_run.read_json(os.path.join(d, "run.json")))

    def test_a_sweep_during_a_ws_capture_parse_leaves_its_capture(self):
        out_dir = tempfile.mkdtemp()
        d = os.path.join(out_dir, "fid-A")
        shutil.move(self.run_dir(self.two_party_pcap()), d)
        real, swept = ws_capture.read_pcap, []

        def read_and_race(path):
            for i, rec in enumerate(real(path)):
                if i == 5:
                    swept.extend(fidelity_run.sweep_raw_captures(out_dir))
                yield rec

        ws_capture.read_pcap = read_and_race
        self.addCleanup(setattr, ws_capture, "read_pcap", real)
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(ws_capture.main([os.path.join(d, "capture", "relay.pcap"), os.path.join(d, "out")]), 0)
        self.assertEqual(swept, [])
        self.assertTrue(os.path.exists(os.path.join(d, "out", "messages.csv.gz")))
        fidelity_run.lock_run(d).close()

    def test_ws_capture_refuses_a_run_folder_another_process_holds(self):
        d = self.run_dir(self.two_party_pcap())
        held = fidelity_run.lock_run(d)
        self.addCleanup(held.close)
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(ws_capture.main([os.path.join(d, "capture", "relay.pcap"), os.path.join(d, "out")]), 3)
        self.assertFalse(os.path.exists(os.path.join(d, "out")))

    def test_downlink_rows_name_the_rust_receive_setting(self):
        plan, _ = fidelity_run.compile_run(fidelity_run.DEFAULT_SCENARIO, "0123456789abcdef")
        self.assertEqual(fidelity_run.receive_notes(plan), {
            "viewers": "Rust receive pinned to video layer 0; the browsers choose their own video layers",
            "vpviewers": "Rust receive viewport_visible_count 4; the browsers choose their own video layers"})
        plan["processes"][-1]["receive"] = {"pin_video_layer": None}
        self.assertNotIn("vpviewers", fidelity_run.receive_notes(plan))
        d = self.run_dir(self.two_party_pcap())
        run = fidelity_run.read_json(os.path.join(d, "run.json"))
        run["plan"]["processes"][0].update(fleet="rust", participants=["r-talk-00"], receive={"pin_video_layer": 1})
        fidelity_run.write_json(os.path.join(d, "run.json"), run)
        fidelity_run.analyze(d, out=lambda *_: None)
        rows = fidelity_run.read_json(os.path.join(d, "fidelity.json"))["result"]["rows"]
        [down] = [r for r in rows if r["measure"] == "downlink bytes/s"]
        self.assertEqual(down["note"], "Rust receive pinned to video layer 1; the browsers choose their own video "
                                       "layers")

    def two_party_pcap(self):
        pcaps = []
        for cport, user in ((40000, "r-talk-00"), (40001, "r-bt-00@bots-app.local")):
            p = Pcap(cport)
            p.open_ws(0.0)
            for s in range(10):
                p.seg(s + 0.1, "u", ws_frame(video(user, 0, s % 5 == 0, size=500), mask=MASK))
                p.seg(s + 0.2, "u", ws_frame(audio(user, s), mask=MASK))
                p.seg(s + 0.3, "d", ws_frame(audio("other", s)))
            pcaps.append(p)
        pcaps[0].records = sorted(pcaps[0].records + pcaps[1].records, key=lambda r: r[0])
        return pcaps[0]

    def test_preconditions_and_capture_errors_make_invalid(self):
        out = []
        d = self.run_dir(self.two_party_pcap(), preconditions=["the capture dropped 2 packets in the kernel"])
        self.assertEqual(fidelity_run.analyze(d, out=out.append), 4)
        self.assertIn("dropped 2 packets", out[0])
        d = self.run_dir(self.two_party_pcap())
        fidelity_run.analyze(d, out=lambda *_: None)
        stats = fidelity_run.read_json(os.path.join(d, "capture", "capture-stats.json"))
        for key in ("decode_errors", "reassembly_gaps", "unsynced_bytes", "eof_pending_segments"):
            fidelity_run.write_json(os.path.join(d, "capture", "capture-stats.json"), dict(stats, **{key: 1}))
            out = []
            self.assertEqual(fidelity_run.analyze(d, out=out.append), 4, key)
            self.assertIn("capture: 1 ", out[0])

    def test_starved_browser_is_invalid(self):
        d = self.run_dir(self.two_party_pcap())
        run = fidelity_run.read_json(os.path.join(d, "run.json"))
        run["fidelity"]["browser_min_video_fps"] = [5.0]
        fidelity_run.write_json(os.path.join(d, "run.json"), run)
        out = []
        self.assertEqual(fidelity_run.analyze(d, out=out.append), 4)
        self.assertIn("bt-00: video layer 0 at 1.00 packets/s, under the quiet baseline 5", out[0])

    def test_the_raw_capture_is_gone_when_analysis_fails(self):
        d = self.run_dir()
        with open(os.path.join(d, "capture", "relay.pcap"), "wb") as fh:
            fh.write(b"not a pcap at all")
        with self.assertRaises(ValueError):
            fidelity_run.analyze(d, out=lambda *_: None)
        self.assertFalse(os.path.exists(os.path.join(d, "capture", "relay.pcap")))

    def test_no_affirmative_host_verdict_is_invalid(self):
        for verdict in (None, {"ok": False}, {"ok": "yes"}):
            d = self.run_dir(self.two_party_pcap())
            run = fidelity_run.read_json(os.path.join(d, "run.json"))
            run["facts"]["host_resource_verdict"] = verdict
            fidelity_run.write_json(os.path.join(d, "run.json"), run)
            out = []
            self.assertEqual(fidelity_run.analyze(d, out=out.append), 4, verdict)
            self.assertIn("no affirmative host resource verdict", out[0])

    def test_no_clean_capture_summary_is_invalid(self):
        for capture in (None, {"packets_captured": None, "packets_dropped_by_kernel": 0},
                        {"packets_captured": 10, "packets_dropped_by_kernel": None},
                        {"packets_captured": 10, "packets_dropped_by_kernel": 1}):
            d = self.run_dir(self.two_party_pcap())
            run = fidelity_run.read_json(os.path.join(d, "run.json"))
            run["facts"]["capture"] = capture
            fidelity_run.write_json(os.path.join(d, "run.json"), run)
            out = []
            self.assertEqual(fidelity_run.analyze(d, out=out.append), 4, capture)
            self.assertIn("no tcpdump summary with zero dropped packets", out[0])

    def test_incomplete_message_inside_the_hold_is_invalid(self):
        stats = {"ws_messages": 10, "eof_partial_messages": [[5.0, 500]], "eof_partial_bytes": 500}
        self.assertIn("never completed, last seen inside the hold", fidelity_run.capture_problems(stats, (0, 10))[0])
        stats["eof_partial_messages"] = [[11.0, 500]]
        self.assertEqual(fidelity_run.capture_problems(stats, (0, 10)), [])
        stats.update(eof_partial_messages=[[11.0, 70000]], eof_partial_bytes=70000)
        self.assertIn("70000 bytes", fidelity_run.capture_problems(stats, (0, 10))[0])

    def test_capture_end_to_end(self):
        pcaps = []
        for cport, user in ((40000, "r-talk-00"), (40001, "r-bt-00@bots-app.local")):
            p = Pcap(cport)
            p.open_ws(0.0)
            for s in range(10):
                p.seg(s + 0.1, "u", ws_frame(video(user, 0, s % 5 == 0, size=500), mask=MASK))
                p.seg(s + 0.2, "u", ws_frame(audio(user, s), mask=MASK))
                p.seg(s + 0.3, "d", ws_frame(audio("other", s)))
            pcaps.append(p)
        pcaps[0].records = sorted(pcaps[0].records + pcaps[1].records, key=lambda r: r[0])
        d = self.run_dir(pcaps[0])
        out = []
        code = fidelity_run.analyze(d, out=out.append)
        self.assertEqual(code, 2, out[0])
        self.assertIn("Verdict: INCOMPLETE (0 FAIL", out[0])
        result = fidelity_run.read_json(os.path.join(d, "fidelity.json"))["result"]
        self.assertEqual(result["problems"], [])
        self.assertFalse(os.path.exists(os.path.join(d, "capture", "relay.pcap")))
        self.assertTrue(os.path.exists(os.path.join(d, "table.md")))


if __name__ == "__main__":
    unittest.main()
