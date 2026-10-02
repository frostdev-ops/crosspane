#!/usr/bin/env bash
# End-to-end speaker v0 (WP-3.6d): two agents on loopback, each with its OWN private PipeWire
# server (scripts/audio/private-pipewire.sh), and a 1 kHz tone played into agent A's "B speakers"
# virtual sink coming out of agent B's playback.
#
#   scripts/lead/impl-env.sh scripts/e2e/audio-private.sh
#   KEEP=1 scripts/lead/impl-env.sh scripts/e2e/audio-private.sh    # keep logs and the work dir
#
# Never touches the owner's PipeWire, Hyprland or deployed agent:
# - two private servers (A's and B's), no hardware, no session manager; scripts/audio/e2e-tone.py
#   links the fixtures' and the agents' streams by hand. Every audio client below gets
#   PIPEWIRE_RUNTIME_DIR and PIPEWIRE_REMOTE of its private server explicitly, and the script
#   reads /proc/<pid>/environ to check that each long-lived client really has them.
# - the agents need a Hyprland session: they share one nested instance (scripts/hypr-nested.sh)
#   with a name of its own, which this run stops only once it has started it.
# - each agent has its own config, state, control socket and port, no session bus (no tray, no
#   Secret Service) and no GPU paths. They run with CROSSPANE_DISCOVERY=0: no mDNS at all, so no
#   other agent on the network is ever contacted. They are paired with `trust add` and connect over
#   explicit loopback addresses only. The script checks the build knows the switch, that each agent
#   logs "discovery off" and that neither holds an mDNS socket.
# - every fixture and agent runs in a process group of its own, and the cleanup signals the
#   members of those groups (found by this run's token in their environment), so nothing outlives
#   the run. A FIFO is opened inside the new session (`setsid bash -c 'exec "$@" <"$0"' …`), never
#   by a redirect that would block in the parent's group first. Phase shutdowns are bounded (TERM,
#   then KILL, then reap) and the exit status is the cleanup's: a leftover process fails the run.
#   Test seam: E2E_TEST_FIFO_HOLD="rec|tone|mic SECONDS" holds back that FIFO's writer, so its
#   reader sits blocked in the open (to try interrupting a run there).
#
# Phases (B is the playing side: it grants A the speakers). Throughout, a watcher on each server
# logs any physical capture node (`crosspane.capture`) that ever appears.
#   0  no grant: the tone must not reach B, and B opens no playback
#   1  granted: B's private sink carries a 1 kHz tone at the expected level within 500 ms of the
#      start (measured from the earliest of the fixture's stamp before its first write and the
#      time before the link to A's sink was made)
#   2  the fixture stops: B's playback goes within 1 s and its sink falls silent
#   3  the fixture plays again, then B withdraws the grant: playback goes within 1 s
#   4  B grants A the microphone ("not supported yet", stored); an app on A records from
#      "audio-b microphone": A refuses it, the recording is silent, no capture node ever appears
set -euo pipefail

self=$(cd "$(dirname "$0")" && pwd -P)/$(basename "$0")
repo=$(cd "$(dirname "$0")/../.." && pwd -P)
cd "$repo"
fx=$repo/scripts/audio/e2e-tone.py
bin=$repo/target/debug
nest=audio-e2e-$$
port_a=47931 port_b=47941

die() { echo "FAIL: $*" >&2; exit 1; }
note() { echo "-- $*"; }

# ---- stage 1: build, the nested Hyprland, then B's private server --------------------------------
outer() {
  for tool in pipewire pw-cat pw-record pw-link pw-dump python3 jq ss pgrep setsid; do
    command -v "$tool" >/dev/null || die "missing tool: $tool"
  done
  # The owner's audio server stays out of reach of everything below.
  unset PULSE_SERVER PIPEWIRE_REMOTE PIPEWIRE_RUNTIME_DIR PIPEWIRE_CORE
  export E2E_REAL_XDG=${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR is not set}
  cargo build -q -p crosspane-agent -p crosspanectl
  # The nest's name is this run's alone. A nest is stopped only once this run has started it: a
  # failed start (say, a name that is somehow taken) must never stop someone else's compositor.
  nest_started=0
  stop_nest() { (( nest_started )) && scripts/hypr-nested.sh stop --name "$nest" >/dev/null 2>&1 || true; }
  trap stop_nest EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  scripts/hypr-nested.sh start --name "$nest" --width 800 --height 600 >/dev/null
  nest_started=1
  # Only the agents use this; the audio fixtures don't care.
  eval "$(scripts/hypr-nested.sh env --name "$nest")"
  scripts/audio/private-pipewire.sh "$self" --server-b
}

# ---- stage 2: B's server runs; start A's -----------------------------------------------------------
server_b() {
  export E2E_B_DIR=$PIPEWIRE_RUNTIME_DIR E2E_B_REMOTE=$PIPEWIRE_REMOTE
  scripts/audio/private-pipewire.sh "$self" --body
}

# ---- stage 3: both servers run; the test ---------------------------------------------------------
body() {
  local real_xdg=$E2E_REAL_XDG
  local a_dir=$PIPEWIRE_RUNTIME_DIR a_remote=$PIPEWIRE_REMOTE
  local b_dir=$E2E_B_DIR b_remote=$E2E_B_REMOTE
  for server in "$a_dir/$a_remote" "$b_dir/$b_remote"; do
    [[ $server == /tmp/crosspane-pipewire.*/crosspane-* && -S $server ]] \
      || die "$server is not a private PipeWire socket"
  done
  [[ $a_dir != "$b_dir" && $a_remote != "$b_remote" ]] || die "both agents would share one server"
  [[ $a_dir != "$real_xdg" && $b_dir != "$real_xdg" ]] || die "a private server is in the owner's runtime dir"
  [[ ${CROSSPANE_PRIVATE_PIPEWIRE:-} == 1 && -z ${PULSE_SERVER:-} ]] || die "not inside the private harness"
  # No agent may start discovery: a build that doesn't know the switch would advertise on the LAN.
  grep -aq CROSSPANE_DISCOVERY "$bin/crosspane-agent" \
    || die "this crosspane-agent build doesn't know CROSSPANE_DISCOVERY; rebuild it"

  # An agent's exit under a nested compositor can abort inside the GL stack after its clean
  # shutdown (an EGL debug callback logging from a destroyed thread-local); no core dumps for it.
  ulimit -c 0
  work=$(mktemp -d /tmp/crosspane-audio-e2e.XXXXXX)
  # Every process of this run carries the token, and leads a process group of its own: `track`
  # records the leader's PID, which is the group's ID. Anything that opens a FIFO does so inside
  # its own session (`setsid bash -c 'exec "$@" <"$0"' FIFO command…`): a plain redirect would
  # block in the parent's process group, before `setsid` ever ran.
  token="audio-e2e-$$-$RANDOM"
  export E2E_TOKEN=$token
  groups=()
  track() { groups+=("$1"); }
  # A running (not exited, not zombie) direct child of this shell: the only PIDs signalled by PID,
  # so a stale PID can't hit a stranger.
  alive() {
    local stat state ppid
    stat=$(cat "/proc/$1/stat" 2>/dev/null) || return 1
    read -r state ppid _ <<<"${stat##*) }"
    [[ $ppid == "$$" && $state != Z ]]
  }
  mine() { alive "$1"; }
  # The live processes of group $1 that carry this run's token: never another run's process. A
  # child that has not reached its `setsid` yet is in no group of its own, so it counts by its PID.
  members() {
    local p
    for p in $(pgrep -g "$1" 2>/dev/null || true) $(alive "$1" && echo "$1"); do
      if tr '\0' '\n' <"/proc/$p/environ" 2>/dev/null | grep -qx "E2E_TOKEN=$token"; then echo "$p"; fi
    done
  }
  signal_groups() { # signal
    local g p
    for g in "${groups[@]}"; do for p in $(members "$g"); do kill -"$1" "$p" 2>/dev/null; done; done
  }
  any_alive() { local g; for g in "${groups[@]}"; do [[ -n $(members "$g") ]] && return 0; done; return 1; }
  # End children within a bound: TERM to all, up to 2 s for them to go, KILL for what remains,
  # then reap them (they are dead by then, so `wait` can't block).
  stop_children() { # pid…
    local pid i
    for pid in "$@"; do if alive "$pid"; then kill -TERM "$pid" 2>/dev/null || true; fi; done
    for i in $(seq 1 40); do
      local any=0
      for pid in "$@"; do if alive "$pid"; then any=1; fi; done
      (( any )) || break
      sleep 0.05
    done
    for pid in "$@"; do if alive "$pid"; then kill -KILL "$pid" 2>/dev/null || true; fi; done
    for pid in "$@"; do
      for i in $(seq 1 40); do alive "$pid" || break; sleep 0.05; done
      if alive "$pid"; then
        echo "WARNING: pid $pid would not die" >&2
      else
        wait "$pid" 2>/dev/null || true
      fi
    done
    return 0
  }
  # The exit status is the cleanup's to decide: the trap is disarmed and the script exits
  # explicitly, because a status set inside an EXIT trap would not change it.
  cleanup() {
    local status=$?
    trap - EXIT INT TERM
    set +e
    signal_groups TERM
    # The agents stop cleanly (their audio worker joins within 2.5 s); kill only what lingers.
    for _ in $(seq 1 40); do any_alive || break; sleep 0.25; done
    signal_groups KILL
    sleep 0.3
    if any_alive; then
      echo "WARNING: processes of this run are still alive: $(for g in "${groups[@]}"; do members "$g"; done | tr '\n' ' ')" >&2
      (( status != 0 )) || status=1
    fi
    if [[ ${KEEP:-0} == 1 || $status -ne 0 ]]; then echo "work dir kept: $work"; else rm -rf "$work"; fi
    exit "$status"
  }
  # A test seam for the cleanup (docs: this header): E2E_TEST_FIFO_HOLD="rec|tone|mic SECONDS" holds
  # back the writer of that FIFO, so its reader sits blocked in the open while the run is interrupted.
  fifo_gap() { # site
    if [[ ${E2E_TEST_FIFO_HOLD:-} == "$1 "* ]]; then
      mkfifo "$work/hold.fifo" 2>/dev/null || true
      read -r -t "${E2E_TEST_FIFO_HOLD#* }" <>"$work/hold.fifo" || true
    fi
    return 0
  }
  trap cleanup EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM

  props='adapter.auto-port-config = "{ mode = dsp position = preserve }"'
  # The environment of a client of node a/b's private server, and nothing else.
  pw_a=(PIPEWIRE_RUNTIME_DIR="$a_dir" PIPEWIRE_REMOTE="$a_remote" XDG_RUNTIME_DIR="$a_dir")
  pw_b=(PIPEWIRE_RUNTIME_DIR="$b_dir" PIPEWIRE_REMOTE="$b_remote" XDG_RUNTIME_DIR="$b_dir")
  # An agent: its own state, its server for audio, the nested Hyprland for the rest (the real
  # runtime dir is where the nest's sockets are), no session bus, no GPU paths, no discovery.
  mkdir -p "$work/a/config/crosspane" "$work/a/state" "$work/a/run" \
    "$work/b/config/crosspane" "$work/b/state" "$work/b/run"
  agent_common=(XDG_RUNTIME_DIR="$real_xdg" DBUS_SESSION_BUS_ADDRESS="unix:path=$work/no-session-bus"
    CROSSPANE_GPU=0 CROSSPANE_REGION_VIDEO=0 CROSSPANE_DISCOVERY=0
    RUST_LOG="${E2E_RUST_LOG:-info,crosspane_agent=debug}" RUST_BACKTRACE=1)
  agent_a=(PIPEWIRE_RUNTIME_DIR="$a_dir" PIPEWIRE_REMOTE="$a_remote" XDG_CONFIG_HOME="$work/a/config"
    XDG_STATE_HOME="$work/a/state" CROSSPANE_RUNTIME_DIR="$work/a/run" "${agent_common[@]}")
  agent_b=(PIPEWIRE_RUNTIME_DIR="$b_dir" PIPEWIRE_REMOTE="$b_remote" XDG_CONFIG_HOME="$work/b/config"
    XDG_STATE_HOME="$work/b/state" CROSSPANE_RUNTIME_DIR="$work/b/run" "${agent_common[@]}")

  ctl() { local n=$1; shift; CROSSPANE_RUNTIME_DIR="$work/$n/run" "$bin/crosspanectl" "$@"; }
  # Start an external command in the background, in a process group of its own; its PID is $pid.
  start_bg() { # log name, then the command
    local name=$1
    shift
    setsid "$@" >"$work/$name.log" 2>&1 </dev/null &
    pid=$!
    track "$pid"
  }
  # The process has exec'd its program and runs with its server's PipeWire environment, not the
  # owner's: the isolation the whole test rests on.
  assert_private() { # pid, program name, a|b
    local pid=$1 comm=$2 who=$3 dir remote
    if [[ $who == a ]]; then dir=$a_dir remote=$a_remote; else dir=$b_dir remote=$b_remote; fi
    for _ in $(seq 1 150); do
      [[ $(cat "/proc/$pid/comm" 2>/dev/null || true) == "$comm" ]] && break
      sleep 0.02
    done
    [[ $(cat "/proc/$pid/comm" 2>/dev/null || true) == "$comm" ]] || die "$comm ($pid) did not start"
    local environ
    environ=$(tr '\0' '\n' <"/proc/$pid/environ")
    grep -qx "PIPEWIRE_REMOTE=$remote" <<<"$environ" \
      || die "$comm ($pid) is not on server $who: PIPEWIRE_REMOTE is not $remote"
    grep -qx "PIPEWIRE_RUNTIME_DIR=$dir" <<<"$environ" \
      || die "$comm ($pid) is not on server $who: PIPEWIRE_RUNTIME_DIR is not $dir"
    ! grep -q '^PULSE_SERVER=' <<<"$environ" || die "$comm ($pid) has a PULSE_SERVER"
  }
  diag() {
    for f in a.agent b.agent a.links b.links a.capture.watch b.capture.watch rec.py mic.py tone.cat tone.play b.rec a.micrec; do
      [[ -f $work/$f.log || -f $work/$f ]] || continue
      echo "--- $f"
      tail -n 25 "$work/$f.log" 2>/dev/null || tail -n 25 "$work/$f" 2>/dev/null || true
    done
    for who in a b; do
      echo "--- ports on server $who"
      on "$who" timeout 3 pw-link -I -o -i 2>&1 | head -40 || true
    done
  }
  fail() {
    echo "FAIL: $*" >&2
    diag >&2
    # The agents watch the real login session and fail closed: if its lock state can't be read in
    # time (a loaded machine), the gate closes and audio sessions end with `Locked`, by design.
    if grep -qs 'refused: Locked' "$work/a.agent.log" "$work/b.agent.log"; then
      echo "hint: an agent's session gate closed during the run (audio refused: Locked); rerun" >&2
    fi
    exit 1
  }
  wait_for() { # description, command string: poll up to 15 s
    local what=$1 cmd=$2
    for _ in $(seq 1 150); do
      if eval "$cmd"; then return 0; fi
      sleep 0.1
    done
    fail "timed out waiting for $what"
  }
  on() { # a|b, then a PipeWire client: run it against that private server (foreground only)
    local who=$1
    shift
    if [[ $who == a ]]; then env -u PULSE_SERVER "${pw_a[@]}" "$@"; else env -u PULSE_SERVER "${pw_b[@]}" "$@"; fi
  }
  # The agent runs no mDNS: it logs that discovery is off, and (its QUIC socket being visible to
  # `ss`, so that the check can see it at all) holds no socket on the mDNS port.
  assert_no_discovery() { # pid, log
    wait_for "pid $1 to report that discovery is off" "grep -q 'discovery off' '$2'"
    ss -H -u -a -n -p | grep -q "pid=$1," || fail "pid $1 has no UDP socket in ss: the mDNS check couldn't see it"
    ! ss -H -u -a -n -p | grep ':5353 ' | grep -q "pid=$1," || fail "pid $1 holds a socket on the mDNS port"
  }

  # ---- the two agents: paired by key, connecting over explicit loopback addresses only ----
  # `crossing = false`: no E1 edge crossing. Both agents sit in one nested compositor, and a real
  # pointer over it would otherwise make A ask B for control over and over (refused, with a notice
  # each time), which scrolls the notices this test looks for out of `status`.
  printf 'name = "audio-a"\nport = %s\nforce_file_keystore = true\ncrossing = false\n[[peers]]\naddr = "127.0.0.1:%s"\n' \
    "$port_a" "$port_b" >"$work/a/config/crosspane/config.toml"
  printf 'name = "audio-b"\nport = %s\nforce_file_keystore = true\ncrossing = false\n' \
    "$port_b" >"$work/b/config/crosspane/config.toml"
  spki_a=$(env -u PULSE_SERVER "${agent_a[@]}" "$bin/crosspane-agent" identity 2>/dev/null | awk '/^spki:/{print $2}')
  spki_b=$(env -u PULSE_SERVER "${agent_b[@]}" "$bin/crosspane-agent" identity 2>/dev/null | awk '/^spki:/{print $2}')
  [[ -n $spki_a && -n $spki_b ]] || die "could not create the agents' identities"
  env -u PULSE_SERVER "${agent_a[@]}" "$bin/crosspane-agent" trust add audio-b "$spki_b" >/dev/null 2>&1
  env -u PULSE_SERVER "${agent_b[@]}" "$bin/crosspane-agent" trust add audio-a "$spki_a" >/dev/null 2>&1

  # Tripwires first: a physical capture node must never appear in either server. Each logs "ready"
  # once it has read the server, and "seen NAME" if the node ever shows up.
  start_bg a.cwatch env -u PULSE_SERVER "${pw_a[@]}" python3 "$fx" watch --log "$work/a.capture.watch" --name 'crosspane.capture'
  assert_private "$pid" python3 a
  pid_watch_a=$pid
  start_bg b.cwatch env -u PULSE_SERVER "${pw_b[@]}" python3 "$fx" watch --log "$work/b.capture.watch" --name 'crosspane.capture'
  assert_private "$pid" python3 b
  pid_watch_b=$pid
  wait_for "the capture watchers to read their servers" \
    'grep -qs ready "$work/a.capture.watch" && grep -qs ready "$work/b.capture.watch"'

  note "starting both agents (A on server A, B on server B)"
  start_bg b.agent env -u PULSE_SERVER "${agent_b[@]}" "$bin/crosspane-agent" run
  pid_b=$pid
  assert_private "$pid_b" crosspane-agent b
  # B has no peers to dial. It must report that discovery is off before A starts and dials it.
  assert_no_discovery "$pid_b" "$work/b.agent.log"
  start_bg a.agent env -u PULSE_SERVER "${agent_a[@]}" "$bin/crosspane-agent" run
  pid_a=$pid
  assert_private "$pid_a" crosspane-agent a
  assert_no_discovery "$pid_a" "$work/a.agent.log"
  wait_for "both agents to run their audio worker" \
    'grep -q "audio sharing on" "$work/a.agent.log" && grep -q "audio sharing on" "$work/b.agent.log"'
  ! grep -q "no audio backend" "$work/a.agent.log" "$work/b.agent.log" || fail "an agent has no audio backend"
  connected() {
    ctl "$1" --json status 2>/dev/null \
      | jq -e '.result.peers[] | select(.connected and (.features | index("audio")))' >/dev/null
  }
  wait_for "the agents to connect and both advertise audio" 'connected a && connected b'
  note "connected over loopback; both advertise audio; discovery off on both"
  # The nodes the agents created in their own servers: A's "B speakers" is the sink the tone goes to.
  sink=$(on a python3 "$fx" node --name 'crosspane.*.speaker' --timeout 10) || fail "no virtual speakers in A's server"
  on b python3 "$fx" node --name 'crosspane.*.speaker' --timeout 10 >/dev/null || fail "no virtual speakers in B's server"
  on a pw-dump | jq -e --arg n "$sink" \
    '.[] | select(.info.props["node.name"] == $n) | .info.props["node.description"] == "audio-b speakers"' >/dev/null \
    || fail "A's virtual sink is not called \"audio-b speakers\""
  note "A's virtual sink: $sink (\"audio-b speakers\")"

  # ---- the fixtures: B's linker and recorder, A's linker ----
  start_bg b.links env -u PULSE_SERVER "${pw_b[@]}" python3 "$fx" links --log "$work/b.links" \
    'crosspane.playback:output_1>crosspane.private.sink:playback_FL' \
    'crosspane.playback:output_2>crosspane.private.sink:playback_FR' \
    'crosspane.private.sink:monitor_FL>crosspane.e2e.rec:input_FL' \
    'crosspane.private.sink:monitor_FR>crosspane.e2e.rec:input_FR'
  assert_private "$pid" python3 b
  start_bg a.links env -u PULSE_SERVER "${pw_a[@]}" python3 "$fx" links --log "$work/a.links" \
    'crosspane.e2e.tone:output_FL>crosspane.*.speaker:playback_1' \
    'crosspane.e2e.tone:output_FR>crosspane.*.speaker:playback_2' \
    'crosspane.*.mic:capture_1>crosspane.e2e.micrec:input_*'
  assert_private "$pid" python3 a
  # The recorder: pw-record on B's private sink monitor, analysed block by block.
  mkfifo "$work/rec.fifo"
  setsid bash -c 'exec "$@" <"$0"' "$work/rec.fifo" python3 "$fx" record --log "$work/rec.log" \
    >"$work/rec.py.log" 2>&1 &
  track $!
  fifo_gap rec
  env -u PULSE_SERVER "${pw_b[@]}" setsid bash -c 'exec "$@" >"$0"' "$work/rec.fifo" \
    pw-record --target 0 --raw --rate 48000 --channels 2 --format f32 \
    --latency 10ms -P "{ node.name = crosspane.e2e.rec $props }" - 2>"$work/b.rec.log" &
  pid_rec=$!
  track "$pid_rec"
  assert_private "$pid_rec" pw-record b
  wait_for "the recorder to deliver blocks" '[[ -f $work/rec.log ]] && (( $(wc -l <"$work/rec.log") > 50 ))'
  note "the recorder on B's private sink monitor runs"

  # The tone fixture: the paced generator into pw-cat, linked to "B speakers" by A's linker.
  tone_start() { # label
    local label=$1 base
    base=$(grep -c 'linked crosspane.e2e.tone' "$work/a.links" || true)
    rm -f "$work/tone.fifo"
    mkfifo "$work/tone.fifo"
    env -u PULSE_SERVER "${pw_a[@]}" setsid bash -c 'exec "$@" <"$0"' "$work/tone.fifo" \
      pw-cat -p --target 0 --raw --rate 48000 --channels 2 --format f32 \
      --latency 10ms -P "{ node.name = crosspane.e2e.tone $props }" - >"$work/tone.cat.log" 2>&1 &
    pid_cat=$!
    track "$pid_cat"
    fifo_gap tone
    setsid bash -c 'exec "$@" >"$0"' "$work/tone.fifo" \
      python3 "$fx" play --start-file "$work/$label.start" --stop-file "$work/$label.stop" \
      2>"$work/tone.play.log" &
    pid_play=$!
    track "$pid_play"
    assert_private "$pid_cat" pw-cat a
    wait_for "the tone to be linked to $sink" \
      "(( \$(grep -c 'linked crosspane.e2e.tone' '$work/a.links' || true) >= base + 2 ))"
    # The time just before the first link to the sink was made, and the fixture's own stamp from
    # just before its first write: audio can't reach the sink before either, and the delay is
    # judged from the earlier of the two.
    awk -v n=$((base + 1)) '/linked crosspane.e2e.tone/ { c++; if (c == n) { print $1; exit } }' \
      "$work/a.links" >"$work/$label.linked"
    wait_for "the tone fixture to stamp its start" "[[ -s '$work/$label.start' ]]"
  }
  tone_stop() { # label: stop the generator (it stamps the time) and pw-cat together, within a bound
    stop_children "$pid_play" "$pid_cat"
    [[ -s $work/$1.stop ]] || fail "the tone fixture did not stamp its stop"
  }
  check_tone() { # label
    python3 "$fx" check tone --log "$work/rec.log" --since "$work/$1.start" --since "$work/$1.linked" \
      --freq 1000 --within 0.5
  }
  playing() { on b python3 "$fx" node --name 'crosspane.playback' --timeout 0.2 >/dev/null 2>&1; }
  speaker_in_use() { ctl b --json status | jq -e '.result.peers[] | select(.name == "audio-a") | .speaker_in_use' >/dev/null; }
  gone() { # label of the stamp file: B's playback node is gone within 1 s of it
    on b python3 "$fx" node --name 'crosspane.playback' --gone --since "$work/$1" --within 1.0 --timeout 5
  }
  no_capture_ever() { # neither tripwire has seen a physical capture node, and both still watch
    mine "$pid_watch_a" && mine "$pid_watch_b" || fail "a capture watcher died"
    ! grep -qs seen "$work/a.capture.watch" "$work/b.capture.watch" \
      || fail "a physical capture node appeared: $(cat "$work/a.capture.watch" "$work/b.capture.watch" | grep seen)"
  }

  # ---- phase 0: no grant ----
  note "phase 0: B has not granted A the speakers"
  ctl b --json status | jq -e '.result.peers[] | select(.name == "audio-a") | (.grants | index("speaker")) == null' >/dev/null \
    || fail "B grants the speakers by default"
  tone_start p0
  sleep 2
  python3 "$fx" check silent --log "$work/rec.log" --since "$work/p0.linked" --after 0 --span 1.5 \
    || fail "audio reached B without a grant"
  if playing; then fail "B opened a playback although A has no grant"; fi
  # The agent's log keeps every notice (its status keeps only the last 20).
  grep -q 'audio (Speaker) with audio-a refused: Permission' "$work/b.agent.log" \
    || fail "B did not report the refusal"
  tone_stop p0
  note "ok: refused, silent, no playback opened"

  # ---- phase 1: granted ----
  note "phase 1: B grants A the speakers"
  ctl b allow audio-a speaker | sed 's/^/   /'
  ctl b --json status | jq -e '.result.peers[] | select(.name == "audio-a") | .grants | index("speaker") != null' >/dev/null \
    || fail "the grant is not in B's status"
  sleep 0.5
  tone_start p1
  sleep 3
  check_tone p1 || fail "B's sink did not carry the tone as expected"
  playing || fail "B has no playback node while the tone plays"
  wait_for "B's status to show A on its speakers" speaker_in_use
  ctl b --json status | jq -e '.result.audio.enabled' >/dev/null || fail "B's status says audio is off"
  # The human-readable status shows the grant and the speaker use per peer.
  human=$(ctl b status)
  grep -q 'allowed here: .*speaker' <<<"$human" || fail "crosspanectl status doesn't show the speaker grant"
  grep -q "playing sound on this machine's speakers now" <<<"$human" \
    || fail "crosspanectl status doesn't show the speaker use"
  grep -q 'audio: speakers' <<<"$human" || fail "crosspanectl status doesn't show that audio is on"
  note "ok: tone through the agents; B's status lists audio-a on its speakers"

  # ---- phase 2: the fixture stops ----
  note "phase 2: the fixture stops"
  tone_stop p1
  gone p1.stop || fail "B's playback stream did not stop within 1 s"
  sleep 1.8
  python3 "$fx" check silent --log "$work/rec.log" --since "$work/p1.stop" --after 1.0 --span 0.6 \
    || fail "B's sink was not silent 1 s after the fixture stopped"
  wait_for "B's status to drop A from its speakers" '! speaker_in_use'
  note "ok: playback stopped within 1 s of the fixture stopping; sink silent"

  # ---- phase 3: the grant is withdrawn while playing ----
  note "phase 3: B withdraws the grant while the tone plays"
  tone_start p3
  sleep 2.5
  check_tone p3 || fail "the second tone did not come through (streams do not restart cleanly)"
  python3 "$fx" stamp "$work/off.stamp"
  ctl b allow audio-a speaker --off | sed 's/^/   /'
  gone off.stamp || fail "B's playback did not stop within 1 s of withdrawing the grant"
  sleep 1.8
  python3 "$fx" check silent --log "$work/rec.log" --since "$work/off.stamp" --after 1.0 --span 0.6 \
    || fail "B's sink was not silent 1 s after the grant was withdrawn"
  tone_stop p3
  note "ok: withdrawing the grant stopped playback within 1 s"

  # ---- phase 4: microphones stay refused end to end ----
  note "phase 4: B grants A the microphone; A has none to offer"
  mic_answer=$(ctl b allow audio-a mic)
  grep -q 'not supported yet' <<<"$mic_answer" || fail "allowing the mic doesn't say it is not supported yet: $mic_answer"
  ctl b --json status | jq -e '.result.peers[] | select(.name == "audio-a") | .grants | index("mic") != null' >/dev/null \
    || fail "the mic grant is not stored"
  # An app on A records from "audio-b microphone" (mono), analysed block by block: the agent must
  # refuse it without asking B, and what the app gets is silence.
  mkfifo "$work/mic.fifo"
  setsid bash -c 'exec "$@" <"$0"' "$work/mic.fifo" python3 "$fx" record --channels 1 --log "$work/mic.log" \
    >"$work/mic.py.log" 2>&1 &
  track $!
  fifo_gap mic
  env -u PULSE_SERVER "${pw_a[@]}" setsid bash -c 'exec "$@" >"$0"' "$work/mic.fifo" \
    pw-record --target 0 --raw --rate 48000 --channels 1 --format f32 \
    --latency 10ms -P "{ node.name = crosspane.e2e.micrec $props }" - 2>"$work/a.micrec.log" &
  pid_mic=$!
  track "$pid_mic"
  assert_private "$pid_mic" pw-record a
  wait_for "A's linker to link the microphone to the recorder" "grep -q 'linked crosspane\\..*\\.mic:capture_1' '$work/a.links'"
  python3 "$fx" stamp "$work/mic.stamp"
  wait_for "A's agent to refuse the microphone" \
    "grep -q 'audio (Microphone) with audio-b refused' '$work/a.agent.log'"
  sleep 1.5
  python3 "$fx" check silent --log "$work/mic.log" --since "$work/mic.stamp" --after 0.2 --span 1.0 \
    || fail "the microphone recording on A is not silent"
  no_capture_ever
  stop_children "$pid_mic"
  note "ok: A refused the microphone locally; the recording is silent; no capture node appeared on either server"

  # ---- the fixtures all ran, and everything stayed in the private servers ----
  grep -q 'linked crosspane.e2e.tone' "$work/a.links" || fail "A's linker never linked the tone"
  grep -q 'linked crosspane.playback' "$work/b.links" || fail "B's linker never linked B's playback"
  grep -q 'linked crosspane.private.sink:monitor_FL' "$work/b.links" || fail "B's linker never linked the recorder"
  (( $(wc -l <"$work/rec.log") > 500 )) || fail "the recorder logged too little"
  no_capture_ever
  mine "$pid_a" || fail "agent A died during the test"
  mine "$pid_b" || fail "agent B died during the test"
  assert_private "$pid_a" crosspane-agent a
  assert_private "$pid_b" crosspane-agent b
  assert_private "$pid_rec" pw-record b
  note "agents, fixtures and recorder ran on their private servers only"
  echo "PASS: speaker v0 end to end: a 1 kHz tone played into A's \"audio-b speakers\" came out of B's"
  echo "      playback within 500 ms, stopped within 1 s of the fixture and of the grant being withdrawn,"
  echo "      and was refused without the grant; a microphone was refused, recorded silence and opened"
  echo "      no capture; discovery was off on both agents"
}

case ${1:-} in
  '') outer ;;
  --server-b) server_b ;;
  --body) body ;;
  *) echo "usage: $0 (no arguments)" >&2; exit 2 ;;
esac
