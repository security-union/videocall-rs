"""Decode a relay-side pcap of WebSocket relay traffic into one row per WS message.

  ws_capture.py <in.pcap> <out_dir> [--port 8080] [--users ID,ID,...]

Writes messages.csv.gz (both directions: u = client to relay, d = relay to client) and conns.json. Standard library
only. With --users only the listed user ids are written, matched in full on the wire bytes; any other, including one
that shares a listed id's part before the @, becomes "other". Frame type, tier and URL route are written only when they
are a known value, otherwise "unknown"; of the HTTP status line only the number is kept. Exits 3 when the pcap is in a
run folder's capture/ and another process holds that run folder's lock.
"""

import csv
import fcntl
import gzip
import heapq
import json
import os
import re
import struct
import sys

PTYPE = {0: "UNKNOWN", 1: "RSA_PUB_KEY", 2: "AES_KEY", 3: "MEDIA", 4: "CONNECTION", 5: "DIAGNOSTICS", 6: "HEALTH",
         7: "MEETING", 8: "SESSION_ASSIGNED", 9: "CONGESTION", 10: "PEER_EVENT", 11: "VIEWPORT",
         12: "LAYER_PREFERENCE", 13: "LAYER_HINT", 14: "DOWNLINK_CONGESTION", 17: "REACTION", 18: "RAISE_HAND",
         19: "MEETING_TIMER", 20: "CAPTION"}
MTYPE = {0: "UNKNOWN", 1: "VIDEO", 2: "AUDIO", 3: "SCREEN", 4: "HEARTBEAT", 5: "RTT", 6: "KEYFRAME_REQUEST"}
WS_OP = {0: "cont", 1: "text", 2: "binary", 8: "close", 9: "ping", 10: "pong"}
WRAPPER_FIELDS = {"packet_type": 1, "user_id": 2, "data": 3, "session_id": 4, "simulcast_layer_id": 5,
                  "media_kind": 6}
MEDIA_FIELDS = {"media_type": 1, "user_id": 2, "data": 3, "frame_type": 4, "timestamp": 5, "audio_metadata": 7,
                "video_metadata": 8, "heartbeat_metadata": 9, "target_session_id": 10}
VIDEO_META_FIELDS = {"sequence": 1, "source_width": 3, "source_height": 4, "adaptive_tier": 6}
AUDIO_META_FIELDS = {"sequence": 5}
HEARTBEAT_FIELDS = {"video_enabled": 1, "audio_enabled": 2, "screen_enabled": 3, "is_speaking": 4, "transport_type": 5}
DIAGNOSTICS_FIELDS = {"target_id": 3, "media_type": 5}
VARINT, FIXED64, LENGTH = 0, 1, 2
COLS = ["t", "conn", "dir", "ws_bytes", "op", "ptype", "mtype", "layer", "mkind", "ftype", "wuser", "wsess",
        "iuser", "tsess", "seq", "w", "h", "src_w", "src_h", "tier", "hb", "plen", "frag", "mts"]
MAX_PENDING_SEGMENTS = 20000
MAX_PENDING_BYTES = 64 * 1024 * 1024
CHECK_EVERY_RECORDS = 20000
OTHER_USER = "other"
UNKNOWN_TEXT = "unknown"
FRAME_TYPES = frozenset({b"", b"key", b"delta"})
KEYFRAME_TARGETS = frozenset({b"", b"VIDEO", b"SCREEN"})
TIERS = frozenset({b"", b"full_hd", b"hd_plus", b"hd", b"standard", b"medium", b"low", b"very_low", b"minimal",
                   b"native", b"high", b"emergency"})
ROUTES = frozenset({"/", "/lobby", "/metrics", "/version", "/login", "/session", "/profile", "/logout"})
HTTP_STATUS_RE = re.compile(r"HTTP/\d(?:\.\d)? (\d{3})(?: |$)")
MAX_UNCHECKED_ID_BYTES = 1024
NEW_CONN_AFTER_S = 1


def varint(b, i):
    r = s = 0
    while True:
        c = b[i]
        i += 1
        r |= (c & 0x7F) << s
        if c < 0x80:
            return r, i
        s += 7
        if s > 63:
            raise ValueError("varint over 10 bytes")


def fields(b):
    """(field_number, wire_type, value): an int for a varint, a slice otherwise."""
    i, n = 0, len(b)
    while i < n:
        key, i = varint(b, i)
        fn, wt = key >> 3, key & 7
        if wt == 0:
            v, i = varint(b, i)
        elif wt in (1, 5):
            size = 8 if wt == 1 else 4
            v, i = b[i:i + size], i + size
        elif wt == 2:
            ln, i = varint(b, i)
            v, i = b[i:i + ln], i + ln
        else:
            raise ValueError(f"wire type {wt}")
        if i > n:
            raise ValueError("truncated field")
        yield fn, wt, v


def fdict(b):
    d = {}
    for fn, wt, v in fields(b):
        d.setdefault(fn, []).append((wt, v))
    return d


def first(d, fn, wire, default=None):
    """The field's first value; a wire type other than the .proto's is a decode error."""
    if fn not in d:
        return default
    wt, value = d[fn][0]
    if wt != wire:
        raise ValueError(f"field {fn}: wire type {wt}, expected {wire}")
    return value


def known_text(d, fn, allowed):
    """The field's text when its bytes are exactly an allowed value, otherwise UNKNOWN_TEXT."""
    raw = first(d, fn, LENGTH, b"")
    return raw.decode("ascii") if raw in allowed else UNKNOWN_TEXT


class Bits:
    def __init__(self, b):
        self.b, self.p = b, 0

    def get(self, n):
        r = 0
        for _ in range(n):
            byte = self.b[self.p >> 3]
            r = (r << 1) | ((byte >> (7 - (self.p & 7))) & 1)
            self.p += 1
        return r


def vp9_key_dims(data):
    """(width, height) from a VP9 keyframe's uncompressed header, or (0, 0)."""
    try:
        r = Bits(data[:32])
        if r.get(2) != 2:
            return 0, 0
        profile = r.get(1) | (r.get(1) << 1)
        if profile == 3:
            r.get(1)
        if r.get(1) or r.get(1) != 0:
            return 0, 0
        r.get(2)
        if (r.get(8), r.get(8), r.get(8)) != (0x49, 0x83, 0x42):
            return 0, 0
        if profile >= 2:
            r.get(1)
        if r.get(3) != 7:
            r.get(1)
            if profile in (1, 3):
                r.get(3)
        elif profile in (1, 3):
            r.get(1)
        return r.get(16) + 1, r.get(16) + 1
    except IndexError:
        return 0, 0


def read_pcap(path):
    """(timestamp, link type, frame bytes) per record of a classic pcap file."""
    formats = {b"\xd4\xc3\xb2\xa1": ("<", 1e6), b"\xa1\xb2\xc3\xd4": (">", 1e6), b"\x4d\x3c\xb2\xa1": ("<", 1e9),
               b"\xa1\xb2\x3c\x4d": (">", 1e9)}
    with open(path, "rb") as f:
        gh = f.read(24)
        if gh[:4] not in formats:
            raise ValueError(f"not a pcap file: {gh[:4]!r}")
        e, div = formats[gh[:4]]
        link = struct.unpack(e + "I", gh[20:24])[0]
        while True:
            h = f.read(16)
            if len(h) < 16:
                return
            ts, tf, incl, _ = struct.unpack(e + "IIII", h)
            yield ts + tf / div, link, f.read(incl)


def ip_tcp(link, pkt):
    """(src, sport, dst, dport, seq, flags, payload) for a TCP segment; None for anything else."""
    r = tcp_span(link, pkt)
    return None if r is None else r[:6] + (pkt[r[6]:r[7]],)


def tcp_span(link, pkt):
    """(src, sport, dst, dport, seq, flags, payload start, payload end) for a TCP segment; None otherwise."""
    if link == 1:
        off, et = 14, struct.unpack(">H", pkt[12:14])[0]
        if et == 0x8100:
            off, et = 18, struct.unpack(">H", pkt[16:18])[0]
    elif link == 113:
        off, et = 16, struct.unpack(">H", pkt[14:16])[0]
    elif link == 276:
        off, et = 20, struct.unpack(">H", pkt[0:2])[0]
    else:
        return None
    if et == 0x0800:
        if pkt[off + 9] != 6:
            return None
        total = struct.unpack(">H", pkt[off + 2:off + 4])[0]
        src, dst = pkt[off + 12:off + 16], pkt[off + 16:off + 20]
        t, end = off + (pkt[off] & 0x0F) * 4, off + total
    elif et == 0x86DD:
        if pkt[off + 6] != 6:
            return None
        plen = struct.unpack(">H", pkt[off + 4:off + 6])[0]
        src, dst = pkt[off + 8:off + 24], pkt[off + 24:off + 40]
        t = off + 40
        end = t + plen
    else:
        return None
    sport, dport, seq = struct.unpack(">HHI", pkt[t:t + 8])
    return bytes(src), sport, bytes(dst), dport, seq, pkt[t + 13], t + (pkt[t + 12] >> 4) * 4, end


class Half:
    """One direction of a TCP connection: reassembly in sequence order, then HTTP upgrade and WS framing."""

    def __init__(self, conn, d, stats):
        self.conn, self.d, self.stats = conn, d, stats
        self.next = None
        self.pos = 0
        self.pending, self.queued, self.queued_bytes = [], {}, 0
        self.buf = bytearray()
        self.buf_ts = None
        self.http = True
        self.frag_op, self.frag, self.frag_wire, self.frag_ts = None, bytearray(), 0, None
        self.lost = False

    def segment(self, ts, seq, flags, payload, sink):
        if flags & 0x02:
            self.next = (seq + 1) & 0xFFFFFFFF
            return
        if not payload or self.lost:
            return
        if self.next is None:
            self.stats["unsynced_bytes"] += len(payload)
            return
        diff = (seq - self.next) & 0xFFFFFFFF
        start = self.pos + diff if diff < 0x80000000 else self.pos - ((self.next - seq) & 0xFFFFFFFF)
        if start > self.pos:
            if start not in self.queued:
                heapq.heappush(self.pending, start)
            old = self.queued.get(start, b"")
            if len(payload) > len(old):
                self.queued[start] = bytes(payload)
                self.queued_bytes += len(payload) - len(old)
            if len(self.queued) > MAX_PENDING_SEGMENTS or self.queued_bytes > MAX_PENDING_BYTES:
                self.stats["reassembly_gaps"] += 1
                self.lost = True
                self.pending.clear()
                self.queued.clear()
                self.queued_bytes = 0
                self.buf.clear()
                return
        elif start + len(payload) > self.pos:
            self.feed(ts, payload[self.pos - start:], sink)
        self.drain(ts, sink)

    def drain(self, ts, sink):
        """Feed pending segments that now start at or before the next expected byte; drop ones already covered."""
        while self.pending and self.pending[0] <= self.pos:
            start = heapq.heappop(self.pending)
            p = self.queued.pop(start)
            self.queued_bytes -= len(p)
            if start + len(p) > self.pos:
                self.feed(ts, p[self.pos - start:], sink)

    def feed(self, ts, payload, sink):
        self.pos += len(payload)
        self.next = (self.next + len(payload)) & 0xFFFFFFFF
        self.buf += payload
        if self.http:
            k = self.buf.find(b"\r\n\r\n")
            if k < 0:
                if len(self.buf) > 65536:
                    self.buf.clear()
                return
            first_line = bytes(self.buf[:k]).decode("latin-1", "replace").split("\r\n", 1)[0]
            del self.buf[:k + 4]
            self.http = False
            if self.d == "u":
                parts = first_line.split(" ")
                route = "/" + parts[1].split("?", 1)[0].lstrip("/").split("/", 1)[0] if len(parts) > 1 else ""
                self.conn["http_path"] = route if route in ROUTES else UNKNOWN_TEXT
                self.conn["upgrade_req_ts"] = ts
            else:
                status = HTTP_STATUS_RE.match(first_line)
                self.conn["http_status"] = int(status.group(1)) if status else None
                self.conn["upgrade_resp_ts"] = ts
                if self.conn["http_status"] != 101:
                    self.conn["not_ws"] = True
        if self.conn.get("not_ws"):
            self.buf.clear()
            return
        self.frames(ts, sink)

    def frames(self, ts, sink):
        b, i = self.buf, 0
        n = len(b)
        while n - i >= 2:
            b0, b1 = b[i], b[i + 1]
            ln, h = b1 & 0x7F, 2
            if ln == 126:
                if n - i < 4:
                    break
                ln, h = struct.unpack(">H", b[i + 2:i + 4])[0], 4
            elif ln == 127:
                if n - i < 10:
                    break
                ln, h = struct.unpack(">Q", b[i + 2:i + 10])[0], 10
            if b1 & 0x80:
                h += 4
            if n - i < h + ln:
                break
            payload = bytes(b[i + h:i + h + ln])
            if b1 & 0x80 and ln:
                rep = (bytes(b[i + h - 4:i + h]) * (ln // 4 + 1))[:ln]
                payload = (int.from_bytes(payload, "little") ^ int.from_bytes(rep, "little")).to_bytes(ln, "little")
            wire, fin, op = h + ln, b0 & 0x80, b0 & 0x0F
            i += h + ln
            if op == 0:
                self.frag += payload
                self.frag_wire += wire
                self.frag_ts = ts
                if fin:
                    sink(self.conn, self.d, ts, self.frag_op, bytes(self.frag), self.frag_wire, True)
                    self.frag_op, self.frag, self.frag_wire, self.frag_ts = None, bytearray(), 0, None
            elif op in (1, 2) and not fin:
                self.frag_op, self.frag, self.frag_wire, self.frag_ts = op, bytearray(payload), wire, ts
            else:
                sink(self.conn, self.d, ts, op, payload, wire, False)
        del b[:i]
        if b:
            self.buf_ts = ts


def keep_user(raw, users):
    """The id to write for these wire bytes. With users, only bytes that are exactly a listed id keep it; without,
    nothing is judged and the text is only made safe for csv."""
    if not raw:
        return ""
    if users is None:
        return raw[:MAX_UNCHECKED_ID_BYTES].decode("utf-8", "replace").replace("\0", "")
    try:
        user = raw.decode("utf-8")
    except UnicodeDecodeError:
        return OTHER_USER
    return user if user in users else OTHER_USER


def decode_wrapper(payload, row, users=None):
    """Fill row from a PacketWrapper; returns the wrapper's user id. row is partial when this raises, so the caller
    decodes into a scratch dict and writes it only on success."""
    pw = fdict(payload)
    pt = first(pw, WRAPPER_FIELDS["packet_type"], VARINT, 0)
    row["ptype"] = PTYPE.get(pt, pt)
    wuser = keep_user(first(pw, WRAPPER_FIELDS["user_id"], LENGTH, b""), users)
    row["wuser"] = wuser
    row["wsess"] = first(pw, WRAPPER_FIELDS["session_id"], VARINT, "")
    row["layer"] = first(pw, WRAPPER_FIELDS["simulcast_layer_id"], VARINT, 0)
    row["mkind"] = first(pw, WRAPPER_FIELDS["media_kind"], VARINT, 0)
    inner = first(pw, WRAPPER_FIELDS["data"], LENGTH)
    if pt == 3 and inner is not None:
        decode_media(inner, row, users)
    elif pt == 5 and inner is not None:
        dp = fdict(inner)
        mt = first(dp, DIAGNOSTICS_FIELDS["media_type"], VARINT, 0)
        row["mtype"] = MTYPE.get(mt, mt)
        row["iuser"] = keep_user(first(dp, DIAGNOSTICS_FIELDS["target_id"], LENGTH, b""), users)
    return wuser


def decode_media(inner, row, users):
    mp = fdict(inner)
    mt = first(mp, MEDIA_FIELDS["media_type"], VARINT, 0)
    row["mtype"] = MTYPE.get(mt, mt)
    row["iuser"] = keep_user(first(mp, MEDIA_FIELDS["user_id"], LENGTH, b""), users)
    if mt == 6:
        row["ftype"] = known_text(mp, MEDIA_FIELDS["data"], KEYFRAME_TARGETS)
    else:
        row["ftype"] = known_text(mp, MEDIA_FIELDS["frame_type"], FRAME_TYPES)
    row["tsess"] = first(mp, MEDIA_FIELDS["target_session_id"], VARINT, "")
    ts5 = first(mp, MEDIA_FIELDS["timestamp"], FIXED64)
    if ts5 is not None:
        row["mts"] = round(struct.unpack("<d", ts5)[0], 1)
    if mt in (1, 3):
        vm = first(mp, MEDIA_FIELDS["video_metadata"], LENGTH)
        if vm is not None:
            v = fdict(vm)
            row["seq"] = first(v, VIDEO_META_FIELDS["sequence"], VARINT, "")
            row["src_w"] = first(v, VIDEO_META_FIELDS["source_width"], VARINT, "")
            row["src_h"] = first(v, VIDEO_META_FIELDS["source_height"], VARINT, "")
            row["tier"] = known_text(v, VIDEO_META_FIELDS["adaptive_tier"], TIERS)
        if row["ftype"] == "key":
            data = first(mp, MEDIA_FIELDS["data"], LENGTH)
            if data is not None:
                row["w"], row["h"] = vp9_key_dims(data[:32])
    elif mt == 2:
        am = first(mp, MEDIA_FIELDS["audio_metadata"], LENGTH)
        if am is not None:
            row["seq"] = first(fdict(am), AUDIO_META_FIELDS["sequence"], VARINT, "")
    elif mt == 4:
        hm = first(mp, MEDIA_FIELDS["heartbeat_metadata"], LENGTH)
        if hm is not None:
            h = fdict(hm)
            flags = ("video_enabled", "audio_enabled", "screen_enabled", "is_speaking")
            row["hb"] = "".join(str(int(bool(first(h, HEARTBEAT_FIELDS[k], VARINT, 0)))) for k in flags) \
                + f"t{first(h, HEARTBEAT_FIELDS['transport_type'], VARINT, 0)}"


def endpoint(addr, port):
    return f"{'.'.join(map(str, addr)) if len(addr) == 4 else addr.hex()}:{port}"


def parse_pcap(src, out_dir, port=8080, users=None, check=None):
    """Decode src into out_dir/messages.csv.gz and out_dir/conns.json; returns the counts. users: the full user ids
    written as they are; None writes every user id. check is called every CHECK_EVERY_RECORDS
    records and may raise to stop the parse."""
    os.makedirs(out_dir, exist_ok=True)
    conns, every, halves = {}, [], {}
    stats = {"pcap_records": 0, "tcp_segments": 0, "ws_messages": 0, "decode_errors": 0, "unknown_text": 0,
             "reassembly_gaps": 0,
             "unsynced_bytes": 0, "eof_pending_segments": 0, "eof_partial_bytes": 0,
             "eof_partial_messages": []}
    with gzip.open(os.path.join(out_dir, "messages.csv.gz"), "wt", newline="") as mfh:
        w = csv.writer(mfh)
        w.writerow(COLS)

        def sink(conn, d, ts, op, payload, wire, frag):
            stats["ws_messages"] += 1
            row = {"t": f"{ts:.6f}", "conn": conn["id"], "dir": d, "ws_bytes": wire, "op": WS_OP.get(op, op),
                   "plen": len(payload), "frag": int(frag)}
            if op == 8:
                conn.setdefault("close_" + d, ts)
            if op == 2 and payload:
                decoded = {}
                try:
                    wuser = decode_wrapper(payload, decoded, users)
                except (ValueError, IndexError, struct.error):
                    stats["decode_errors"] += 1
                    decoded = {"ptype": "DECODE_ERROR"}
                else:
                    if d == "u" and wuser:
                        conn.setdefault("users", {}).setdefault(wuser, 0)
                        conn["users"][wuser] += 1
                    if UNKNOWN_TEXT in (decoded.get("ftype"), decoded.get("tier")):
                        stats["unknown_text"] += 1
                row.update(decoded)
            w.writerow([row.get(c, "") for c in COLS])

        for ts, link, pkt in read_pcap(src):
            stats["pcap_records"] += 1
            if check is not None and stats["pcap_records"] % CHECK_EVERY_RECORDS == 0:
                check()
            try:
                r = ip_tcp(link, pkt)
            except (IndexError, struct.error):
                r = None
            if r is None:
                continue
            s, sp, dst, dp, seq, flags, payload = r
            if dp == port:
                key, d = (s, sp), "u"
            elif sp == port:
                key, d = (dst, dp), "d"
            else:
                continue
            stats["tcp_segments"] += 1
            conn = conns.get(key)
            fresh_syn = flags & 0x02 and d == "u" and not flags & 0x10
            if conn is None or (fresh_syn and conn.get("syn_ts") is not None
                                and ts - conn["syn_ts"] > NEW_CONN_AFTER_S):
                conn = {"id": len(every), "client": endpoint(*key)}
                every.append(conn)
                conns[key] = conn
                halves[(conn["id"], "u")] = Half(conn, "u", stats)
                halves[(conn["id"], "d")] = Half(conn, "d", stats)
            if fresh_syn:
                conn.setdefault("syn_ts", ts)
            conn.setdefault("first_ts", ts)
            conn["last_ts"] = ts
            if flags & 0x01:
                conn.setdefault("fin_" + d, ts)
            if flags & 0x04:
                conn.setdefault("rst_" + d, ts)
            halves[(conn["id"], d)].segment(ts, seq, flags, payload, sink)
    for h in halves.values():
        stats["eof_pending_segments"] += len(h.queued)
        if h.http or h.lost or h.conn.get("not_ws"):
            continue
        for when, size in ((h.buf_ts, len(h.buf)), (h.frag_ts, len(h.frag))):
            if size:
                stats["eof_partial_bytes"] += size
                stats["eof_partial_messages"].append([when, size])
    stats["pcap_bytes"] = os.path.getsize(src)
    with open(os.path.join(out_dir, "conns.json"), "w", encoding="utf-8") as fh:
        json.dump(every, fh, indent=1)
    return stats


RUN_LOCK = ".lock"


def lock_run(run_dir):
    """An exclusive flock on run_dir/.lock while the returned file is open; None when another holder has it."""
    flags = os.O_WRONLY | os.O_CREAT | os.O_APPEND | os.O_NOFOLLOW | os.O_NONBLOCK
    fd = os.open(os.path.join(run_dir, RUN_LOCK), flags, 0o600)
    fh = os.fdopen(fd, "a", encoding="utf-8")
    try:
        fcntl.flock(fh, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        fh.close()
        return None
    return fh


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    if len(argv) < 2:
        print(__doc__, file=sys.stderr)
        return 3
    port = int(argv[argv.index("--port") + 1]) if "--port" in argv else 8080
    users = set(argv[argv.index("--users") + 1].split(",")) if "--users" in argv else None
    cap = os.path.dirname(os.path.abspath(argv[0]))
    run_dir = os.path.dirname(cap) if os.path.basename(cap) == "capture" else None
    held = lock_run(run_dir) if run_dir else None
    if run_dir and held is None:
        print(f"another process holds the run folder of {argv[0]}", file=sys.stderr)
        return 3
    try:
        print(json.dumps(parse_pcap(argv[0], argv[1], port, users)))
    finally:
        if held:
            held.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
