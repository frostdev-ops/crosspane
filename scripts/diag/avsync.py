#!/usr/bin/env python3
# Audio/video sync work prompted by Thiago Goncalves (2026-10).
"""Join two agents' `crosspane::avsync` trace logs and print the audio/video offset.

Usage:
    avsync.py --source SRC.log --dest DST.log [--dest RENDER.log ...] [--stream N]
              [--projection N] [--json]
    avsync.py --self-test            # runs on scripts/diag/avsync_fixtures, asserts +50 ms
    avsync.py --regen-fixtures       # rewrites those fixtures (deterministic)

Turn the events on with RUST_LOG=crosspane::avsync=trace (the Windows agent log file already
records TRACE for crosspane*). --source is the capturing machine's log, --dest the playing and
showing machine's log; --dest may repeat if the proxy renderer logs to a separate file.

Clock offset cancellation: audio latency = (dest playout - source capture) and video latency =
(dest present - source capture) both contain the same unknown (dest clock - source clock) term,
so median(audio) - median(video) is the true A/V offset with that term subtracted out.

Log line format assumed (tracing_subscriber::fmt() default, ANSI optional, any prefix allowed):
    2026-10-12T10:00:00.250000Z TRACE crosspane::avsync: side="tx" peer="d1" stream=3 seq=1 ...
optionally with span context before the target (`TRACE a{x=1}:b: crosspane::avsync: ...`) and a
journald/syslog prefix before the timestamp. Lines that are JSON objects (fmt().json()) with
`target` and `fields` are accepted too. The event name is taken from the first of the fields
`side`, `event`, `kind`, `ev`, then from a leading message word (`tx`, `rx`, `video`,
`present`), then inferred from the field set (queued_frames => tx, ring_frames => rx,
decoded_ns => video, after_us => present).

Definitions (all times in ns on the logging machine's monotonic clock, 48 kHz audio):
    tx capture time of sample_time  = tx.at_ns - tx.queued_frames / 48 kHz
    rx playout time                 = rx.at_ns + rx.ring_frames / 48 kHz
    audio latency(sample_time)      = rx playout - tx capture (rx sample_time is matched to the
                                      tx at or below it, within 0.5 s, and interpolated)
    video present time              = video.sent_ns + present.after_us
    video latency(seq)              = present time - video.captured_ns
    offset = median(audio latency) - median(video latency); positive = audio lags video.
"""

import argparse
import bisect
import json
import os
import re
import statistics
import sys

RATE = 48_000
NS = 1_000_000_000
BUCKET_S = 30
MAX_INTERP_FRAMES = RATE // 2
ATSC_LAG_MS = 45.0
ATSC_LEAD_MS = -15.0
TARGET = "crosspane::avsync"
KINDS = ("tx", "rx", "video", "present")
NAME_FIELDS = ("side", "event", "kind", "ev")
INFER = (("queued_frames", "tx"), ("ring_frames", "rx"), ("decoded_ns", "video"), ("after_us", "present"))

ANSI_RE = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
TARGET_RE = re.compile(r"(?:^|[\s:])" + re.escape(TARGET) + r":\s?")
FIELD_RE = re.compile(r'([A-Za-z_][\w.]*)=("(?:[^"\\]|\\.)*"|\S+)')
FIXTURES = os.path.join(os.path.dirname(os.path.abspath(__file__)), "avsync_fixtures")


# ---------------------------------------------------------------- parsing

def _value(raw):
    if isinstance(raw, (int, float)):
        return raw
    if len(raw) >= 2 and raw[0] == '"' and raw[-1] == '"':
        try:
            return json.loads(raw)
        except ValueError:
            return raw[1:-1]
    try:
        return int(raw)
    except ValueError:
        try:
            return float(raw)
        except ValueError:
            return raw


def _kind(fields, message):
    for name in NAME_FIELDS:
        v = fields.get(name)
        if isinstance(v, str) and v in KINDS:
            return v
    if message:
        word = message.split()[0].strip("{}:,")
        if word in KINDS:
            return word
    for key, kind in INFER:
        if key in fields:
            return kind
    return None


def parse_line(line):
    """Return (kind, fields) for a crosspane::avsync event line, else None."""
    line = ANSI_RE.sub("", line).strip()
    if not line:
        return None
    if line.startswith("{"):
        try:
            obj = json.loads(line)
        except ValueError:
            return None
        if obj.get("target") != TARGET:
            return None
        raw = dict(obj.get("fields") or {})
        message = raw.pop("message", "")
        fields = {k: _value(v) for k, v in raw.items()}
        kind = _kind(fields, message)
        return (kind, fields) if kind else None
    m = TARGET_RE.search(line)
    if not m:
        return None
    rest = line[m.end():]
    first = FIELD_RE.search(rest)
    message = rest[: first.start()].strip() if first else rest.strip()
    fields = {k: _value(v) for k, v in FIELD_RE.findall(rest)}
    fields.pop("message", None)
    kind = _kind(fields, message)
    return (kind, fields) if kind else None


def load(paths):
    events = {k: [] for k in KINDS}
    for path in paths:
        with open(path, encoding="utf-8", errors="replace") as fh:
            for line in fh:
                parsed = parse_line(line)
                if parsed:
                    events[parsed[0]].append(parsed[1])
    return events


def _num(ev, key):
    v = ev.get(key)
    return v if isinstance(v, (int, float)) else None


# ---------------------------------------------------------------- statistics

def pct(values, p):
    """Nearest-rank percentile."""
    s = sorted(values)
    if not s:
        return None
    rank = max(1, -(-len(s) * p // 100))
    return s[int(rank) - 1]


def summary(values_ns):
    if not values_ns:
        return {"n": 0, "median_ms": None, "p95_ms": None}
    return {
        "n": len(values_ns),
        "median_ms": statistics.median(values_ns) / 1e6,
        "p95_ms": pct(values_ns, 95) / 1e6,
    }


# ---------------------------------------------------------------- joining

def join_audio(src, dst, stream=None):
    """List of samples: (source capture ns, latency ns, capture->send, send->rx, rx->playout)."""
    tx_by_stream = {}
    for ev in src["tx"]:
        st, at, q, sid = _num(ev, "sample_time"), _num(ev, "at_ns"), _num(ev, "queued_frames"), ev.get("stream")
        if None in (st, at, q) or (stream is not None and sid != stream):
            continue
        tx_by_stream.setdefault(sid, {})[st] = (at - q * NS // RATE, at)
    index = {sid: sorted(m) for sid, m in tx_by_stream.items()}
    out = []
    for ev in dst["rx"]:
        st, at, ring, sid = _num(ev, "sample_time"), _num(ev, "at_ns"), _num(ev, "ring_frames"), ev.get("stream")
        if None in (st, at) or sid not in index:
            continue
        keys = index[sid]
        i = bisect.bisect_right(keys, st) - 1
        if i < 0 or st - keys[i] > MAX_INTERP_FRAMES:
            continue
        cap0, sent = tx_by_stream[sid][keys[i]]
        cap = cap0 + (st - keys[i]) * NS // RATE
        play = at + (ring or 0) * NS // RATE
        out.append({
            "t": cap,
            "latency": play - cap,
            "capture_to_send": sent - cap0,
            "send_to_rx": at - sent - (st - keys[i]) * NS // RATE,
            "rx_to_playout": play - at,
        })
    return out


def join_video(dst, projection=None):
    """List of samples: (captured ns, latency ns, captured->decoded, decoded->presented)."""
    presents = {}
    by_seq = {}
    for ev in dst["present"]:
        seq, after = _num(ev, "seq"), _num(ev, "after_us")
        if seq is None or after is None:
            continue
        presents.setdefault((ev.get("id"), seq), after)
        by_seq.setdefault(seq, []).append(after)
    out = []
    loose = 0
    for ev in dst["video"]:
        seq, cap, dec, sent = (_num(ev, k) for k in ("seq", "captured_ns", "decoded_ns", "sent_ns"))
        proj = ev.get("projection")
        if None in (seq, cap, dec, sent) or (projection is not None and proj != projection):
            continue
        after = presents.get((proj, seq))
        if after is None and len(by_seq.get(seq, ())) == 1:
            after = by_seq[seq][0]  # proxy id differs from projection id: unique seq match
            loose += 1
        if after is None:
            continue
        shown = sent + after * 1000
        out.append({
            "t": cap,
            "latency": shown - cap,
            "captured_to_decoded": dec - cap,
            "decoded_to_presented": shown - dec,
        })
    return out, loose


def analyse(src, dst, stream=None, projection=None):
    audio = join_audio(src, dst, stream)
    video, loose = join_video(dst, projection)
    a = summary([s["latency"] for s in audio])
    v = summary([s["latency"] for s in video])
    offset = None
    if a["n"] and v["n"]:
        offset = a["median_ms"] - v["median_ms"]
    stages = {
        "audio.capture_to_send": summary([s["capture_to_send"] for s in audio]),
        "audio.send_to_rx (incl. clock offset)": summary([s["send_to_rx"] for s in audio]),
        "audio.rx_to_playout": summary([s["rx_to_playout"] for s in audio]),
        "video.captured_to_decoded (incl. clock offset)": summary([s["captured_to_decoded"] for s in video]),
        "video.decoded_to_presented": summary([s["decoded_to_presented"] for s in video]),
    }
    drift = []
    if audio and video:
        t0 = min(min(s["t"] for s in audio), min(s["t"] for s in video))
        buckets = {}
        for kind, samples in (("a", audio), ("v", video)):
            for s in samples:
                b = (s["t"] - t0) // (BUCKET_S * NS)
                buckets.setdefault(b, {"a": [], "v": []})[kind].append(s["latency"])
        for b in sorted(buckets):
            av = buckets[b]
            off = None
            if av["a"] and av["v"]:
                off = (statistics.median(av["a"]) - statistics.median(av["v"])) / 1e6
            drift.append({"start_s": b * BUCKET_S, "audio_n": len(av["a"]), "video_n": len(av["v"]),
                          "offset_ms": off})
    verdict = None
    if offset is not None:
        if offset > ATSC_LAG_MS:
            verdict = "audio lags beyond ATSC guide"
        elif offset < ATSC_LEAD_MS:
            verdict = "audio leads beyond ATSC guide"
        else:
            verdict = "within ATSC guide"
    return {
        "counts": {k: len(src[k]) for k in KINDS} | {"dest." + k: len(dst[k]) for k in KINDS},
        "audio_latency": a,
        "video_latency": v,
        "video_matched_by_seq_only": loose,
        "offset_ms": offset,
        "atsc_guide_ms": {"lag": ATSC_LAG_MS, "lead": ATSC_LEAD_MS},
        "verdict": verdict,
        "stages": stages,
        "drift": drift,
        "note": "absolute latencies include the unknown cross-machine clock offset; offset_ms does not",
    }


def _ms(v):
    return "   n/a" if v is None else f"{v:8.2f}"


def render(r):
    lines = []
    c = r["counts"]
    lines.append(f"events: source tx={c['tx']}  dest rx={c['dest.rx']} video={c['dest.video']} "
                 f"present={c['dest.present']}")
    lines.append(f"matched: audio={r['audio_latency']['n']}  video={r['video_latency']['n']}"
                 + (f" ({r['video_matched_by_seq_only']} by seq only)" if r["video_matched_by_seq_only"] else ""))
    off = r["offset_ms"]
    if off is None:
        lines.append("A/V offset: n/a (need matched audio and video samples)")
    else:
        lines.append(f"A/V offset: {off:+.2f} ms  (positive = audio lags video)  "
                     f"ATSC guide: +{ATSC_LAG_MS:.0f} ms lag / {ATSC_LEAD_MS:.0f} ms lead -> {r['verdict']}")
    lines.append("")
    lines.append(f"{'stage':50} {'n':>6} {'median ms':>10} {'p95 ms':>9}")
    for name, s in [("audio latency (incl. clock offset)", r["audio_latency"]),
                    ("video latency (incl. clock offset)", r["video_latency"])] + list(r["stages"].items()):
        lines.append(f"{name:50} {s['n']:>6} {_ms(s['median_ms']):>10} {_ms(s['p95_ms']):>9}")
    lines.append("")
    lines.append(f"drift ({BUCKET_S} s buckets, by source capture time):")
    for d in r["drift"]:
        off = "n/a" if d["offset_ms"] is None else f"{d['offset_ms']:+.2f} ms"
        lines.append(f"  {d['start_s']:>5}-{d['start_s'] + BUCKET_S:<5}s  audio={d['audio_n']:<5} "
                     f"video={d['video_n']:<5} offset={off}")
    return "\n".join(lines)


# ---------------------------------------------------------------- fixtures

FIX_SOURCE = "source.log"
FIX_DEST = "dest.log"
FIX_RENDER = "dest-render.log"
FIX_OFFSET_MS = 50.0


def _stamp(ns):
    s, frac = divmod(ns, NS)
    h, rem = divmod(s % 86400, 3600)
    return f"2026-10-12T{h:02}:{rem // 60:02}:{rem % 60:02}.{frac // 1000:06}Z"


def regen_fixtures(directory=FIXTURES):
    """Deterministic synthetic logs. Dest clock = source clock + 5.000123456 s.

    Audio: true capture->playout 80 ms (+-2 ms symmetric jitter), video: true captured->presented
    30 ms (+-2 ms), so the expected offset is +50.00 ms. Every 4th rx sample_time sits 240
    frames past its tx to exercise interpolation. Noise lines exercise the parser.
    """
    os.makedirs(directory, exist_ok=True)
    s0 = 1_000_000_000_000
    clock = 5_000_123_456
    ms = 1_000_000
    n = 260
    src, dst, rnd = [], [], []
    src.append(f"{_stamp(s0)}  INFO crosspane_agent: agent started")
    dst.append(f"{_stamp(s0 + clock)}  INFO crosspane_agent::audio: playback opened stream=3")
    for k in range(n):
        j = ((k % 5) - 2) * ms
        cap = s0 + k * 250 * ms
        q = 480 + (k % 3) * 96
        tx_at = cap + q * NS // RATE
        st = k * 12_000
        line = (f"{_stamp(tx_at)} TRACE crosspane::avsync: side=\"tx\" peer=\"desk-01\" stream=3 "
                f"seq={k} sample_time={st} at_ns={tx_at} queued_frames={q}")
        if k == 7:
            line = "\x1b[2m" + line + "\x1b[0m"
        if k == 9:
            line = "Oct 12 10:00:02 mac crosspane-agent[412]: " + line.replace(
                "TRACE crosspane", "TRACE audio{peer=desk-01}:pump: crosspane")
        src.append(line)
        extra = 240 if k % 4 == 0 else 0
        rx_cap = cap + extra * NS // RATE
        ring = 960
        rx_at = rx_cap + clock + 60 * ms + j
        name = 'side="rx" ' if k % 2 else ""  # odd lines named, even lines inferred
        dst.append(f"{_stamp(rx_at)} TRACE crosspane::avsync: {name}peer=\"mac-01\" stream=3 "
                   f"sample_time={st + extra} at_ns={rx_at} ring_frames={ring} target_ms=40")
        vcap = s0 + k * 250 * ms + 3 * ms
        dec = vcap + clock + 25 * ms
        sent = dec + 1 * ms
        dst.append(f"{_stamp(dec)} TRACE crosspane::avsync: side=\"video\" source=\"mac-01\" projection=7 "
                   f"seq={k} captured_ns={vcap} decoded_ns={dec} sent_ns={sent}")
        after_us = 4000 + j // 1000
        rnd.append(f"{_stamp(sent)} TRACE crosspane::avsync: side=\"present\" id=7 seq={k} after_us={after_us}")
        if k % 50 == 0:
            rnd.append(f"{_stamp(sent)} TRACE crosspane::avsync: side=\"present\" id=99 seq={k} after_us=1")
    src.append(f"{_stamp(s0 + n * 250 * ms)} DEBUG crosspane::avsync: unrelated debug line without fields")
    dst.append(f"{_stamp(s0 + clock + n * 250 * ms)} DEBUG crosspane_media::audio::jitter: stats target_ms=40")
    for name, lines in ((FIX_SOURCE, src), (FIX_DEST, dst), (FIX_RENDER, rnd)):
        with open(os.path.join(directory, name), "w", encoding="utf-8", newline="\n") as fh:
            fh.write("\n".join(lines) + "\n")


def self_test():
    sample = ('2026-10-12T10:00:00.250000Z TRACE a{x=1}:b: crosspane::avsync: side="tx" peer="p q" '
              'stream=3 seq=1 sample_time=480 at_ns=10 queued_frames=2')
    kind, f = parse_line(sample)
    assert kind == "tx" and f["peer"] == "p q" and f["sample_time"] == 480, (kind, f)
    assert parse_line('{"target":"crosspane::avsync","fields":{"message":"rx","sample_time":1,'
                      '"at_ns":2,"ring_frames":0}}')[0] == "rx"
    assert parse_line("2026 TRACE crosspane::avsync: present id=1 seq=2 after_us=3")[0] == "present"
    assert parse_line("2026  INFO crosspane_agent: side=\"tx\" queued_frames=1") is None

    src = load([os.path.join(FIXTURES, FIX_SOURCE)])
    dst = load([os.path.join(FIXTURES, FIX_DEST), os.path.join(FIXTURES, FIX_RENDER)])
    r = analyse(src, dst)
    print(render(r))
    assert r["audio_latency"]["n"] == 260, r["audio_latency"]
    assert r["video_latency"]["n"] == 260, r["video_latency"]
    assert abs(r["offset_ms"] - FIX_OFFSET_MS) < 0.01, r["offset_ms"]
    assert r["verdict"] == "audio lags beyond ATSC guide"
    assert abs(r["stages"]["audio.rx_to_playout"]["median_ms"] - 20.0) < 0.01
    assert abs(r["stages"]["video.decoded_to_presented"]["median_ms"] - 5.0) < 0.01
    assert len(r["drift"]) == 3 and all(abs(d["offset_ms"] - FIX_OFFSET_MS) < 2.5 for d in r["drift"])
    print(f"\nself-test OK: offset {r['offset_ms']:+.2f} ms (expected {FIX_OFFSET_MS:+.2f})")
    return 0


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--source", action="append", default=[], help="capturing machine's log (repeatable)")
    ap.add_argument("--dest", action="append", default=[], help="playing/showing machine's log (repeatable)")
    ap.add_argument("--stream", type=int, help="only this audio stream id")
    ap.add_argument("--projection", type=int, help="only this projection id")
    ap.add_argument("--json", action="store_true", help="machine-readable output")
    ap.add_argument("--self-test", action="store_true", help="run on the bundled fixtures")
    ap.add_argument("--regen-fixtures", action="store_true", help=argparse.SUPPRESS)
    a = ap.parse_args(argv)
    if a.regen_fixtures:
        regen_fixtures()
        return 0
    if a.self_test:
        return self_test()
    if not a.source or not a.dest:
        ap.error("--source and --dest are required")
    r = analyse(load(a.source), load(a.dest), a.stream, a.projection)
    print(json.dumps(r, indent=2) if a.json else render(r))
    return 0 if r["offset_ms"] is not None else 1


if __name__ == "__main__":
    sys.exit(main())
