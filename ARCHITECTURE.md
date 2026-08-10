# nova architecture

this document describes the current nova system. `docs/research/` preserves historical comet and cloudflare research, but it is not the runtime architecture.

## 1. system boundary

nova is a local-first controller for pi. each device runs one nova engine that owns the device's agent processes and durable state. viewports do not depend on a hosted account, organization, application database, application relay, or object store. iroh may use an encrypted packet relay only to establish the peer path.

```text
                         ┌──────────── local device ────────────┐
                         │                                      │
                         │  gpui ui ─┐                           │
                         │  tui ─────┼─ typed rpc ─ nova engine ┼─ pi
                         │  ipc cli ─┘                           │
                         └──────────────────┬───────────────────┘
                                            │ authenticated direct rpc
                         ┌──────────────────▼───────────────────┐
                         │ nova engine on another paired device ┼─ pi
                         └──────────────────────────────────────┘
```

## 2. processes

### headed desktop

`nova` probes `ws://127.0.0.1:27654`. if a daemon answers with the nova rpc protocol, the desktop attaches to it. otherwise it embeds an engine in-process and best-effort serves that same engine on the ipc port for other local viewports.

### headless engine

`nova headless` owns the data directory, runs sessions and terminals, listens on local ipc, listens for paired nova engines, and runs the peer convergence loop. launchd and systemd user-service helpers live in `apps/nova/src/daemon.rs`.

### terminal viewport

`nova-tui` always attaches to a separate engine. it may start a detached headless engine when none is available, so quitting the terminal only detaches the viewport and does not kill runs or ptys.

## 3. engine responsibilities

`crates/engine` couples the interface to the device-local pi runtime:

- `SessionsEngine` launches and supervises harness runs;
- `DocHost` owns transcript loro documents and command ledgers;
- `WorkspaceHost` owns devices, projects, chats, and session status;
- `Repos`, `ProjectsSync`, and `CheckoutDiffSync` own filesystem and git state;
- `Terminals` owns ptys and bounded replay;
- `AgentAccounts` and pi management own device-local provider configuration;
- `Uploads` stages and commits local attachments;
- `NovaHost` owns identity, trust, discovery, and direct connections;
- `PeerSync` converges workspace and transcript documents between paired engines.

all durable application data is local. the retained `orgs/dev-org/dev-user` path is a migration namespace for existing data, not a hosted identity model.

## 4. rpc seam

`crates/rpc` provides the transport-independent `RpcService` and `RpcClient` seam. the same method names and json values run over:

- an in-memory duplex for an embedded desktop engine;
- localhost websocket ipc for local viewports;
- an encrypted, mutually authenticated iroh quic stream between paired nova engines.

methods with `targetDeviceId` are forwarded to that device through `NovaHost`. streaming methods are proxied item by item. local-only nova trust methods are deliberately not forwardable.

## 5. documents and command execution

workspace and transcript state use loro documents persisted in sqlite snapshots. chat actions are durable command-ledger entries. the engine that owns a chat drains its pending commands, records outcomes before executing side effects where required, runs pi, and folds streamed events back into the transcript.

paired engines exchange version-vector deltas directly. `NovaSyncHeads` advertises document heads and `NovaSyncApply` imports and replies with missing updates. a five-second repair pass handles missed notifications and reconnects. no room server or nudge endpoint is involved.

## 6. nova networking

`crates/nova` contains:

- stable ed25519 device identities;
- short-lived single-use pairing codes;
- persisted iroh tickets, public keys, roles, and revocation;
- method-level allow-lists;
- bounded explicit cidr discovery;
- encrypted iroh quic transport with direct hole punching and relay fallback.

`Engine::assemble_runtime` binds iroh udp and a discovery-only tcp probe on port `27655` by default. see `docs/ARCHITECTURE-Nova.md` for the handshake, ticket, synchronization, relay, limits, and trust model.

## 7. ui state and device selection

`crates/ui::state::AppState` maintains standing watches for workspace devices, projects, chats, sessions, updates, and paired nova peers. `NovaWatchPeers` merges newly paired devices into selectors immediately. project and chat rows retain their owning device id; pi pages also expose an explicit **pi runs on** selector.

**settings → nova engine** is the control surface for pairing, discovery, endpoint edits, role changes, testing, revocation, and forgetting.

## 8. security

localhost ipc is a local trust boundary. nova peer sockets always authenticate, even over loopback, and authorize every method against the peer's current trust record. pairing and trust mutation remain ipc-only.

peer rpc is end-to-end encrypted by iroh quic. a self-hosted relay can be selected with `NOVA_IROH_RELAY_URL`; relays forward opaque packets and do not receive nova trust or application authority.

## 9. removed hosted system

nova no longer contains or starts the previous workos/cloudflare backend, durable-object rooms, device relay, r2 mirroring, hosted auth rpc, or the edge-only ios client. historical design notes remain under `docs/research/` only as provenance.

## 10. verification boundary

unit and integration tests cover identity, pairing, authorization, revocation, discovery, encrypted rpc routing, remote pi configuration, and loro convergence. the remaining system proof is two physical devices on the intended pi network and the intended self-hosted iroh relay.
