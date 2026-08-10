# nova

nova is a local-first, multi-device controller for coding agents, built around pi. it ships a native gpui desktop, a terminal viewport, a headless engine, direct encrypted nova-to-nova sync, and native parent/child agent collaboration.

the interface began as a fork of [comet](https://github.com/zeronsh/comet). many thanks to zeronsh for the original shell.

## developing

one command, no environment setup:

```sh
scripts/nova-dev.sh
```

the script keeps the engine daemon alive, builds the desktop, and warm-swaps the window after successful ui rebuilds. chats, terminals, and agent sessions survive the swap.

direct commands are also available:

```sh
cargo run -p nova
cargo run -p nova -- headless
cargo run -p nova -- tui
```

nova uses `NOVA_DATA_DIR`, `NOVA_IPC_PORT`, and the other `NOVA_*` runtime settings. existing `.comet-native` data and core `COMET_*` settings are detected as migration fallbacks, so the rename does not strand prior chats or device state.

## layout

- `apps/nova` — desktop and headless command
- `crates/engine` — sessions, repositories, terminals, local state, and peer sync
- `crates/ui` — native gpui interface
- `crates/tui` — terminal viewport
- `crates/nova` — authenticated iroh transport and trust
- `crates/harness` — pi and compatibility harness adapters

see `ARCHITECTURE.md` for the active system boundary.
