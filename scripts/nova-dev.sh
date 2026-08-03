#!/usr/bin/env bash
# Warm-handoff dev loop for the headed app ("hot reload" for a compiled gpui
# UI): the engine daemon stays up, source changes trigger a background
# `cargo build -p comet`, and only after the NEW window reports
# `nova-hotreload-ready` (crates/ui/src/state.rs attach_engine) is the old
# window retired. Compile time never blanks the screen; failed builds leave
# the running window untouched.
#
#   scripts/nova-dev.sh            # daemon (if needed) + supervised app
#
# Env overrides: COMET_DATA_DIR (default ~/.comet-native), COMET_IPC_PORT
# (default 27654, matching apps/comet), NOVA_DEV_READY_TIMEOUT (seconds, 30).
set -euo pipefail
cd "$(dirname "$0")/.."

export COMET_DATA_DIR="${COMET_DATA_DIR:-$HOME/.comet-native}"
export COMET_IPC_PORT="${COMET_IPC_PORT:-27654}"
READY_TIMEOUT="${NOVA_DEV_READY_TIMEOUT:-30}"

RUN_DIR=/tmp/nova-dev
mkdir -p "$RUN_DIR"
GUI_PID=""
GEN=0

cleanup() {
  # Retire the supervised window only — the daemon outlives this script.
  [[ -n "$GUI_PID" ]] && kill "$GUI_PID" 2>/dev/null || true
}
trap cleanup EXIT

# 1. Engine daemon: connect-or-start. The headed app embeds an engine when the
# port is free, which would make every replacement window race the old one's
# embedded engine — so the supervisor guarantees a daemon is listening first.
if ! (echo >"/dev/tcp/127.0.0.1/$COMET_IPC_PORT") 2>/dev/null; then
  echo "▸ starting engine daemon on :$COMET_IPC_PORT (data: $COMET_DATA_DIR)"
  cargo build -p comet -q
  ./target/debug/comet headless >"$RUN_DIR/daemon.log" 2>&1 &
  for _ in $(seq 1 60); do
    (echo >"/dev/tcp/127.0.0.1/$COMET_IPC_PORT") 2>/dev/null && break
    sleep 0.5
  done
fi

spawn_gui() {
  GEN=$((GEN + 1))
  local log="$RUN_DIR/gui.$GEN.log"
  : >"$log"
  NOVA_HOTRELOAD=1 ./target/debug/comet >"$log" 2>&1 &
  GUI_PID=$!
}

# Wait for the ready marker in a generation's log; success only if the process
# is still alive when the marker appears.
await_ready() {
  local pid="$1" log="$RUN_DIR/gui.$GEN.log"
  for _ in $(seq 1 "$((READY_TIMEOUT * 2))"); do
    if grep -q nova-hotreload-ready "$log" 2>/dev/null && kill -0 "$pid" 2>/dev/null; then
      return 0
    fi
    kill -0 "$pid" 2>/dev/null || return 1
    sleep 0.5
  done
  return 1
}

echo "▸ initial build…"
cargo build -p comet -q
spawn_gui
echo "▸ gui gen $GEN up (pid $GUI_PID) — editing crates/ui or apps/comet swaps it in place"

STAMP="$RUN_DIR/stamp"
touch "$STAMP"

while true; do
  sleep 1
  # Nothing to do until a source file is newer than the last handled change.
  if [[ -z $(find crates apps -name '*.rs' -newer "$STAMP" -print -quit 2>/dev/null) ]]; then
    # Old window crashed on its own? Restart it so the loop self-heals.
    if ! kill -0 "$GUI_PID" 2>/dev/null; then
      echo "▸ gui gen $GEN died — relaunching"
      spawn_gui
    fi
    continue
  fi
  touch "$STAMP"

  echo "▸ change detected — building (old window keeps running)…"
  if ! cargo build -p comet 2>&1 | tail -5; then
    echo "✗ build failed — keeping gen $GEN"
    continue
  fi

  old_pid="$GUI_PID"
  old_gen="$GEN"
  spawn_gui
  if await_ready "$GUI_PID"; then
    echo "▸ gen $GEN ready — retiring gen $old_gen (pid $old_pid)"
    { kill "$old_pid" && wait "$old_pid"; } 2>/dev/null || true
  else
    echo "✗ gen $GEN never reported ready — keeping gen $old_gen (pid $old_pid)"
    { kill "$GUI_PID" && wait "$GUI_PID"; } 2>/dev/null || true
    GUI_PID="$old_pid"
    GEN="$old_gen"
  fi
done
