#!/usr/bin/env bash
# One-command demo: boots a seeded engine daemon + the headed app, offline.
# Made for judging look & feel with real input — no edge, no auth needed.
#
#   scripts/dev-demo.sh            # build, seed demo data, open the app
#   scripts/dev-demo.sh --slow     # pace mock streams (~10s) to watch streaming
#
# Everything lives under /tmp/nova-demo-*; re-runs reuse it. Ctrl-C cleans up.
set -euo pipefail
cd "$(dirname "$0")/.."

DAEMON_DIR=/tmp/nova-demo-daemon
UI_DIR=/tmp/nova-demo-ui
IPC=27921
DELAY=""
[[ "${1:-}" == "--slow" ]] && DELAY=350

echo "▸ building (first run takes a few minutes)…"
cargo build -p nova -q

echo "▸ starting engine daemon on :$IPC"
env NOVA_DATA_DIR="$DAEMON_DIR" NOVA_IPC_PORT=$IPC NOVA_HARNESS=mock \
  ${DELAY:+NOVA_MOCK_DELAY_MS=$DELAY} RUST_LOG=warn \
  ./target/debug/nova headless &
DAEMON_PID=$!
trap 'kill $DAEMON_PID 2>/dev/null || true' EXIT
for _ in $(seq 1 40); do
  (exec 3<>/dev/tcp/127.0.0.1/$IPC) 2>/dev/null && { exec 3>&-; break; }
  sleep 0.25
done

probe() { cargo run -q -p nova-rpc --example rpc_probe -- "ws://127.0.0.1:$IPC" "$@"; }

if [[ ! -f "$DAEMON_DIR/.demo-seeded" ]]; then
  echo "▸ seeding demo chats"
  DEV=$(probe LocalDevice '{}' | python3 -c 'import json,sys;print(json.load(sys.stdin)["deviceId"])')
  # One project per demo folder, created up-front (chats join by project id).
  declare -A PROJECTS=()
  for project in nova-native soccertcg nova aether; do
    pid=$(uuidgen | tr 'A-Z' 'a-z')
    probe Mutate "{\"op\":\"createProject\",\"projectId\":\"$pid\",\"deviceId\":\"$DEV\",\"path\":\"$HOME/github/$project\",\"name\":\"$project\",\"gitDetected\":true}" >/dev/null
    PROJECTS[$project]="$pid"
  done
  seed() { # title project branch age_hours run
    local id; id=$(uuidgen | tr 'A-Z' 'a-z')
    local pid="${PROJECTS[$2]}"
    probe Mutate "{\"op\":\"createChat\",\"chatId\":\"$id\",\"projectId\":\"$pid\",\"config\":{\"harness\":\"mock\",\"model\":\"fable-5\",\"reasoning\":null,\"sandbox\":\"workspace-write\"},\"branch\":\"$3\"}" >/dev/null
    probe Mutate "{\"op\":\"renameChat\",\"chatId\":\"$id\",\"title\":\"$1\"}" >/dev/null
    if [[ "$5" == run ]]; then
      probe QueueCommand "{\"chatId\":\"$id\",\"command\":{\"kind\":\"run\",\"messageId\":\"$(uuidgen)\",\"request\":{\"prompt\":\"Walk me through the streaming pipeline\",\"model\":null,\"reasoning\":null,\"modelOptions\":{},\"cwd\":\"/tmp\",\"sandbox\":\"workspace-write\",\"autoApprove\":true,\"resume\":null}}}" >/dev/null
      sleep 1
    fi
    probe Mutate "{\"op\":\"setChatActivity\",\"chatId\":\"$id\",\"lastMessageAt\":$(( ($(date +%s) - $4*3600) * 1000 ))}" >/dev/null
  }
  seed "Native Nova Rust Rewrite"    nova-native nova-native/main                 0  run
  seed "Rebalance Player Stats Caps"  soccertcg    nova/rebalance-player-stat-caps  2  run
  seed "Craft Premium TCG Experience" soccertcg    nova/craft-premium-tcg-exp       26 skip
  seed "Initial Context Exploration"  nova        nova/initial-context-exploration 14 skip
  seed "Soccer TCG Repo Creation"     aether       aether/main                       48 skip
  touch "$DAEMON_DIR/.demo-seeded"
fi

echo "▸ opening nova (composer is live — type into it; --slow shows streaming)"
NOVA_DATA_DIR="$UI_DIR" NOVA_IPC_PORT=$IPC RUST_LOG=warn ./target/debug/nova
