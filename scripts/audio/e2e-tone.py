#!/usr/bin/env python3
"""Fixtures for scripts/e2e/audio-private.sh (WP-3.6d, speaker v0).

  play    a paced 1 kHz stereo test tone as raw f32le on stdout (pipe it into `pw-cat -p`)
  record  read raw f32le on stdin (from `pw-record`) and log, per 10 ms block, its time, level and
          rising zero crossings
  links   the policy daemon the private servers lack: keep the listed port pairs linked
  watch   log every node matching a pattern that ever appears in the server (a tripwire)
  node    wait for a node to appear or disappear in the server
  stamp   write the current time to a file
  check   judge a recording: `tone` (frequency, level, delay) or `silent`

The private servers (scripts/audio/private-pipewire.sh) run no session manager, so nothing links
streams by itself; `links` does that for the fixtures and for the agents' playback streams.

Every subcommand that talks to PipeWire refuses to run unless PIPEWIRE_REMOTE and
PIPEWIRE_RUNTIME_DIR name a private server: the owner's audio server is never touched. Times are
time.monotonic() seconds on this machine, so the files one fixture writes can be compared with
another's. Nothing here logs samples beyond block levels.

`links` and `watch` follow `pw-dump -m`. That child ends with its parent however the parent ends
(a termination signal, then a kill, with Linux's parent-death signal as the last resort), and the
shell that starts these fixtures puts each in its own process group and kills the group.
"""

import argparse
import array
import ctypes
import fnmatch
import json
import math
import os
import select
import signal
import statistics
import subprocess
import sys
import time

RATE = 48000
BLOCK = 480  # frames in 10 ms
BLOCK_SECONDS = BLOCK / RATE
PRIVATE_PREFIX = "/tmp/crosspane-pipewire."
PR_SET_PDEATHSIG = 1


def fail(message):
    print(f"e2e-tone: FAIL: {message}", file=sys.stderr)
    sys.exit(1)


def require_private():
    """Refuse to touch any PipeWire server but one scripts/audio/private-pipewire.sh made."""
    remote = os.environ.get("PIPEWIRE_REMOTE", "")
    runtime = os.environ.get("PIPEWIRE_RUNTIME_DIR", "")
    private = (
        remote.startswith("crosspane-")
        and "/" not in remote
        and runtime.startswith(PRIVATE_PREFIX)
        and "/" not in runtime[len(PRIVATE_PREFIX):]
        and os.path.exists(os.path.join(runtime, remote))
    )
    if not private:
        fail(
            "refusing to run outside a private PipeWire server "
            f"(PIPEWIRE_REMOTE={remote!r}, PIPEWIRE_RUNTIME_DIR={runtime!r})"
        )


def write_time(path, value):
    with open(path, "w") as handle:
        handle.write(f"{value:.6f}\n")


def read_time(path):
    try:
        with open(path) as handle:
            return float(handle.read().strip())
    except (OSError, ValueError) as error:
        fail(f"no usable time in {path}: {error}")


# ---- play --------------------------------------------------------------------------------------


def cmd_play(args):
    if sys.byteorder != "little":
        fail("raw f32le output needs a little-endian machine")
    out = sys.stdout.buffer

    def stop(_signum, _frame):
        write_time(args.stop_file, time.monotonic())
        os._exit(0)

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    begin = time.monotonic()
    block = 0
    while True:
        # Block n is due at begin + n * 10 ms; a little ahead keeps the consumer fed.
        wait = begin + block * BLOCK_SECONDS - args.ahead - time.monotonic()
        if wait > 0:
            time.sleep(wait)
        samples = array.array("f")
        first = block * BLOCK
        for i in range(BLOCK):
            phase = ((first + i) * args.freq / RATE) % 1.0
            value = args.amp * math.sin(2.0 * math.pi * phase)
            samples.append(value)
            samples.append(value)
        if block == 0:
            # Taken before the first write: no audio can reach the sink earlier than this.
            write_time(args.start_file, time.monotonic())
        try:
            out.write(samples.tobytes())
            out.flush()
        except BrokenPipeError:
            sys.exit(0)
        block += 1


# ---- record ------------------------------------------------------------------------------------


def cmd_record(args):
    inp = sys.stdin.buffer
    channels = args.channels
    size = BLOCK * channels * 4
    with open(args.log, "w", buffering=1) as log:
        while True:
            data = inp.read(size)
            now = time.monotonic()
            if len(data) < size:
                return
            samples = array.array("f")
            samples.frombytes(data)
            left = samples[0::channels]
            right = samples[1::channels] if channels > 1 else left
            levels = []
            for channel in (left, right):
                total = sum(v * v for v in channel)
                # A non-finite sample shows up as a non-finite level, which `check` refuses.
                levels.append(math.sqrt(total / len(channel)) if math.isfinite(total) else math.inf)
            rising = sum(1 for a, b in zip(left, left[1:]) if a <= 0.0 < b)
            log.write(f"{now:.6f} {levels[0]:.5f} {levels[1]:.5f} {rising}\n")


class Block:
    def __init__(self, line):
        t, left, right, rising = line.split()
        self.t = float(t)
        self.left = float(left)
        self.right = float(right)
        self.rising = int(rising)

    def level(self):
        return max(self.left, self.right)


def read_log(path):
    try:
        with open(path) as handle:
            blocks = [Block(line) for line in handle if line.strip()]
    except (OSError, ValueError) as error:
        fail(f"unreadable recording log {path}: {error}")
    if any(not math.isfinite(b.left) or not math.isfinite(b.right) for b in blocks):
        fail("the recording holds non-finite samples")
    return blocks


# ---- PipeWire graph ----------------------------------------------------------------------------


def graph(objects):
    """Nodes (id -> name), ports and links (output port id, input port id) of `pw-dump` objects."""
    nodes, ports, links = {}, [], set()
    for obj in objects:
        info = obj.get("info") or {}
        props = info.get("props") or {}
        kind = obj.get("type", "")
        if kind.endswith("Interface:Node"):
            nodes[obj["id"]] = props.get("node.name", "")
        elif kind.endswith("Interface:Port"):
            ports.append(
                {
                    "id": obj["id"],
                    "node": props.get("node.id"),
                    "name": props.get("port.name", ""),
                    "direction": info.get("direction", ""),
                }
            )
        elif kind.endswith("Interface:Link"):
            links.add((info.get("output-port-id"), info.get("input-port-id")))
    return nodes, ports, links


def snapshot():
    """The private server's graph, from one `pw-dump`."""
    result = subprocess.run(["pw-dump"], capture_output=True, timeout=5, check=False)
    if result.returncode != 0:
        fail(f"pw-dump failed: {result.stderr.decode(errors='replace').strip()}")
    return graph(json.loads(result.stdout))


def die_with_parent():
    """In a child about to exec: have Linux send it SIGTERM if this process dies first, so a
    fixture that is killed outright can't leave its `pw-dump -m` behind."""
    ctypes.CDLL("libc.so.6", use_errno=True).prctl(PR_SET_PDEATHSIG, signal.SIGTERM)


def stop_process(proc):
    """End a child within a bound: TERM, then KILL, then reap it."""
    if proc.poll() is not None:
        return
    proc.terminate()
    try:
        proc.wait(timeout=1.0)
        return
    except subprocess.TimeoutExpired:
        pass
    proc.kill()
    try:
        proc.wait(timeout=2.0)
    except subprocess.TimeoutExpired:
        print("e2e-tone: warning: a pw-dump monitor would not die", file=sys.stderr)


class Monitor:
    """`pw-dump -m` of the private server: its objects by id, kept current. The first array it
    prints is every object, later ones are what changed (an object that went away has an id and
    no type)."""

    def __init__(self):
        require_private()
        self.objects = {}
        self.ready = False  # the first full dump has been read
        self._decoder = json.JSONDecoder()
        self._buffer = ""
        self._proc = subprocess.Popen(
            ["pw-dump", "-m", "-N"],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            preexec_fn=die_with_parent,
        )

    def step(self, timeout):
        """Wait up to `timeout` s for news. Returns (objects that arrived or changed, ids that
        went away)."""
        changed, removed = [], []
        ready, _, _ = select.select([self._proc.stdout], [], [], timeout)
        if not ready:
            if self._proc.poll() is not None:
                fail("pw-dump -m ended: the private server is gone")
            return changed, removed
        chunk = os.read(self._proc.stdout.fileno(), 1 << 16)
        if not chunk:
            fail("pw-dump -m ended: the private server is gone")
        self._buffer += chunk.decode("utf-8", errors="replace")
        while True:
            self._buffer = self._buffer.lstrip()
            try:
                batch, end = self._decoder.raw_decode(self._buffer)
            except json.JSONDecodeError:
                break
            self._buffer = self._buffer[end:]
            self.ready = True
            for obj in batch:
                if "type" in obj:
                    self.objects[obj["id"]] = obj
                    changed.append(obj)
                else:
                    self.objects.pop(obj["id"], None)
                    removed.append(obj["id"])
        return changed, removed

    def close(self):
        stop_process(self._proc)


def terminating():
    """A list that gains an item when this process is asked to stop."""
    stop = []
    signal.signal(signal.SIGTERM, lambda *_: stop.append(True))
    signal.signal(signal.SIGINT, lambda *_: stop.append(True))
    return stop


def parse_rule(text):
    """`OUT_NODE:OUT_PORT>IN_NODE:IN_PORT`, node and port names as fnmatch patterns."""
    try:
        left, right = text.split(">")
        out_node, out_port = left.rsplit(":", 1)
        in_node, in_port = right.rsplit(":", 1)
    except ValueError:
        fail(f"bad link rule {text!r}: want OUT_NODE:OUT_PORT>IN_NODE:IN_PORT")
    return (out_node, out_port, in_node, in_port)


def link_missing(rules, objects, asked, log):
    """Make every link the rules ask for that the graph doesn't have yet. The log line starts with
    the time before `pw-link` was run, then the time after."""
    nodes, ports, links = graph(objects.values())
    for out_node, out_port, in_node, in_port in rules:
        outs = [
            p for p in ports
            if p["direction"] == "output" and fnmatch.fnmatch(p["name"], out_port)
            and fnmatch.fnmatch(nodes.get(p["node"], ""), out_node)
        ]
        ins = [
            p for p in ports
            if p["direction"] == "input" and fnmatch.fnmatch(p["name"], in_port)
            and fnmatch.fnmatch(nodes.get(p["node"], ""), in_node)
        ]
        for out in outs:
            for inp in ins:
                pair = (out["id"], inp["id"])
                # A link just made may not have reached the graph yet: don't ask twice at once.
                if pair in links or time.monotonic() - asked.get(pair, -1.0) < 0.3:
                    continue
                before = time.monotonic()
                asked[pair] = before
                made = subprocess.run(
                    ["pw-link", str(out["id"]), str(inp["id"])],
                    capture_output=True, timeout=5, check=False,
                )
                if made.returncode == 0:
                    log.write(
                        f"{before:.6f} {time.monotonic():.6f} linked "
                        f"{nodes.get(out['node'])}:{out['name']} -> {nodes.get(inp['node'])}:{inp['name']}\n"
                    )


def cmd_links(args):
    """Event driven: a stream is linked within milliseconds of appearing, an idle server costs
    nothing."""
    rules = [parse_rule(r) for r in args.rules]
    stop = terminating()
    monitor = Monitor()
    asked = {}
    try:
        with open(args.log, "w", buffering=1) as log:
            while not stop:
                monitor.step(0.2)
                # After every change, and on idle ticks, so a link that failed is asked again.
                link_missing(rules, monitor.objects, asked, log)
    finally:
        monitor.close()


def cmd_watch(args):
    """A tripwire: log `ready` once the server's objects are read, then every node whose name
    matches ever appearing (including one that was already there)."""
    stop = terminating()
    monitor = Monitor()
    seen = set()
    announced = False
    try:
        with open(args.log, "w", buffering=1) as log:
            while not stop:
                changed, removed = monitor.step(0.2)
                seen.difference_update(removed)
                for obj in changed:
                    if not obj.get("type", "").endswith("Interface:Node"):
                        continue
                    name = ((obj.get("info") or {}).get("props") or {}).get("node.name", "")
                    if fnmatch.fnmatch(name, args.name) and obj["id"] not in seen:
                        seen.add(obj["id"])
                        log.write(f"{time.monotonic():.6f} seen {name}\n")
                if monitor.ready and not announced:
                    announced = True
                    log.write(f"{time.monotonic():.6f} ready watching {args.name}\n")
    finally:
        monitor.close()


# ---- node --------------------------------------------------------------------------------------


def cmd_node(args):
    require_private()
    deadline = time.monotonic() + args.timeout
    while True:
        nodes, _, _ = snapshot()
        matching = sorted(n for n in nodes.values() if fnmatch.fnmatch(n, args.name))
        now = time.monotonic()
        if args.gone and not matching:
            if args.since:
                elapsed = now - read_time(args.since)
                print(f"{args.name} gone {elapsed * 1000:.0f} ms after the stop")
                if elapsed > args.within:
                    fail(f"{args.name} took {elapsed:.2f} s to go, more than {args.within} s")
            else:
                print(f"{args.name} gone")
            return
        if not args.gone and matching:
            print(matching[0])
            return
        if now > deadline:
            fail(f"{args.name} {'still there' if args.gone else 'never appeared'} after {args.timeout} s")
        time.sleep(args.poll)


# ---- check -------------------------------------------------------------------------------------


def cmd_tone(args):
    blocks = read_log(args.log)
    references = {path: read_time(path) for path in args.since}
    # The earliest of the references bounds the delay: each was taken before audio could flow.
    reference = min(references.values())
    quiet = 0.5 * args.rms

    def loud(block):
        return min(block.left, block.right) > quiet

    first = next((b for b in blocks if b.t >= reference and loud(b)), None)
    if first is None:
        fail(f"no tone reached the recorder in the {len(blocks)} blocks it logged")
    delay = first.t - reference
    start = first.t + args.settle
    window = [b for b in blocks if start <= b.t < start + args.span]
    if len(window) < 0.8 * args.span / BLOCK_SECONDS:
        fail(f"the tone or the recording ended early: {len(window)} blocks in the steady window")
    gaps = sum(1 for b in window if not loud(b))
    if gaps > 0.02 * len(window):
        fail(f"the tone is not steady: {gaps} of {len(window)} blocks are quiet")
    frequency = sum(b.rising for b in window) / (len(window) * BLOCK_SECONDS)
    left = statistics.median(b.left for b in window)
    right = statistics.median(b.right for b in window)
    from_each = ", ".join(
        f"{os.path.basename(path)} {(first.t - t) * 1000:.0f} ms" for path, t in references.items()
    )
    print(
        f"tone: first heard {delay * 1000:.0f} ms after the earliest reference ({from_each}); "
        f"{frequency:.0f} Hz, level L {left:.3f} R {right:.3f} RMS (expected {args.rms:.3f})"
    )
    problems = []
    if delay > args.within:
        problems.append(f"heard {delay:.2f} s after the start, more than {args.within} s")
    if abs(frequency - args.freq) > 0.03 * args.freq:
        problems.append(f"frequency {frequency:.0f} Hz is not {args.freq} Hz")
    for name, level in (("left", left), ("right", right)):
        if not 0.7 * args.rms <= level <= 1.3 * args.rms:
            problems.append(f"{name} level {level:.3f} is not within 30% of {args.rms:.3f}")
    if problems:
        fail("; ".join(problems))


def cmd_silent(args):
    blocks = read_log(args.log)
    start = read_time(args.since) + args.after
    window = [b for b in blocks if start <= b.t < start + args.span]
    if len(window) < 0.5 * args.span / BLOCK_SECONDS:
        fail(f"the recorder logged only {len(window)} blocks in the window: it did not run")
    loud = [b for b in window if b.level() > args.threshold]
    if loud:
        fail(
            f"{len(loud)} of {len(window)} blocks are audible in the window, "
            f"the first {loud[0].t - start:.2f} s in"
        )
    print(f"silent: {len(window)} blocks, none above {args.threshold}")


def cmd_stamp(args):
    write_time(args.file, time.monotonic())


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = parser.add_subparsers(dest="command", required=True)

    stamp = sub.add_parser("stamp", help="write the current time to FILE")
    stamp.add_argument("file")
    stamp.set_defaults(run=cmd_stamp)

    play = sub.add_parser("play")
    play.add_argument("--start-file", required=True)
    play.add_argument("--stop-file", required=True)
    play.add_argument("--freq", type=float, default=1000.0)
    play.add_argument("--amp", type=float, default=0.25)
    play.add_argument("--ahead", type=float, default=0.03)
    play.set_defaults(run=cmd_play)

    record = sub.add_parser("record")
    record.add_argument("--log", required=True)
    record.add_argument("--channels", type=int, choices=(1, 2), default=2)
    record.set_defaults(run=cmd_record)

    links = sub.add_parser("links")
    links.add_argument("--log", required=True)
    links.add_argument("rules", nargs="+")
    links.set_defaults(run=cmd_links)

    watch = sub.add_parser("watch")
    watch.add_argument("--log", required=True)
    watch.add_argument("--name", required=True, help="node name (fnmatch pattern)")
    watch.set_defaults(run=cmd_watch)

    node = sub.add_parser("node")
    node.add_argument("--name", required=True, help="node name (fnmatch pattern)")
    node.add_argument("--gone", action="store_true")
    node.add_argument("--since", help="file with the stop time (for --gone)")
    node.add_argument("--within", type=float, default=1.0)
    node.add_argument("--timeout", type=float, default=10.0)
    node.add_argument("--poll", type=float, default=0.02)
    node.set_defaults(run=cmd_node)

    check = sub.add_parser("check")
    kinds = check.add_subparsers(dest="kind", required=True)
    tone = kinds.add_parser("tone")
    tone.add_argument("--log", required=True)
    tone.add_argument(
        "--since", action="append", required=True,
        help="file with a time taken before the tone could flow (repeatable; the earliest counts)",
    )
    tone.add_argument("--freq", type=float, default=1000.0)
    tone.add_argument("--rms", type=float, default=0.25 / math.sqrt(2.0))
    tone.add_argument("--within", type=float, default=0.5)
    tone.add_argument("--settle", type=float, default=0.3)
    tone.add_argument("--span", type=float, default=1.0)
    tone.set_defaults(run=cmd_tone)
    silent = kinds.add_parser("silent")
    silent.add_argument("--log", required=True)
    silent.add_argument("--since", required=True)
    silent.add_argument("--after", type=float, default=0.0)
    silent.add_argument("--span", type=float, required=True)
    silent.add_argument("--threshold", type=float, default=0.005)
    silent.set_defaults(run=cmd_silent)

    args = parser.parse_args()
    args.run(args)


if __name__ == "__main__":
    main()
