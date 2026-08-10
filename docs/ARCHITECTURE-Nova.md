# nova engine peer networking

nova engine is the pi-coupled backend that runs on every nova device. desktop, tui, local ipc clients, and paired engines all terminate at the same typed rpc service.

## topology

```text
                         local ipc (`ws://127.0.0.1:27654`)
desktop ui / tui ───────────────────────────────────────────┐
                                                            ▼
                                                     nova engine ── pi
                                                            │
                     encrypted iroh quic (direct when able) │
                     ┌──────────────────────────────────────┘
                     ▼
                paired nova engine ── pi
```

`Engine::assemble_runtime` binds iroh quic over udp and a signed discovery-only websocket probe over tcp on port `27655`; `NOVA_PORT` overrides both. a `targetDeviceId` rpc call dials the stored iroh ticket for that device and proxies unary or streaming replies through the ordinary `RpcClient` seam. iroh attempts a direct path with hole punching and uses an end-to-end encrypted relay path when direct connectivity is unavailable. `NOVA_IROH_RELAY_URL` selects a self-hosted relay; otherwise iroh's default network is used.

hosted workos authentication, organization bootstrap, cloudflare workers, durable-object rooms, the host relay/link cache, attachment mirroring, and diff sidecars are not part of nova. those implementations have been removed from the active tree.

## local storage

an engine owns all durable state under its data directory:

- `device-id` is the stable engine id;
- `nova/identity.json` contains the ed25519 device identity;
- `nova/trust.json` contains paired public keys, iroh tickets, names, roles, and revocation state;
- `orgs/dev-org/dev-user/` retains the local workspace, transcript snapshots, and run journals.

`dev-org/dev-user` is only a compatibility directory name for existing local data. it no longer represents a hosted account or authorization boundary.

## identity and pairing

private identity and trust files use mode `0600` on unix. every peer connection begins with a server challenge containing the server identity, a fresh nonce, and an ed25519 signature. a known peer verifies the challenge and responds with a signature bound to both device ids and that nonce.

first-time pairing is explicit:

1. engine b generates a six-digit code valid for five minutes and at most five failed attempts;
2. engine a enters b's iroh ticket and code;
3. a proves possession of its device key while submitting the code;
4. b consumes the code atomically and stores a as an admin peer;
5. a verifies b's signed identity against the iroh tls endpoint id and stores b's ticket and public key.

pairing codes are single-use. discovery does not grant trust.

## authorization and revocation

local ipc is fully trusted. the iroh nova endpoint never upgrades loopback traffic to local trust: every peer connection must authenticate.

remote roles use explicit allow-lists:

- `peer` can read shared state, inspect repositories, watch transcripts, queue chat commands, and participate in loro convergence;
- `admin` also has device-local repository mutations, terminals, agent account flows, uploads, updates, and pi settings/credentials/packages;
- pairing, discovery, and trust-management methods remain local-only.

unknown methods are denied remotely. the service re-reads the trust store for every rpc call, so revocation and role changes apply to already-open sockets rather than only to the next handshake.

## direct loro convergence

loro remains the local workspace and transcript document model. paired engines repair each other directly through two internal, authenticated rpc methods:

1. `NovaSyncHeads` returns workspace and chat version vectors;
2. `NovaSyncApply` imports the caller's missing updates and returns everything the caller lacked at its advertised heads.

workspace updates apply before transcript updates so host ownership exists before an imported command can wake its executor. exchanges are bounded to 4,096 chat documents, 32 mib per document, and 128 mib total. a five-second process-lifetime repair loop converges missed edits and sleep/wake reconnects; trust changes wake it immediately. device presence is updated after a successful exchange.

this model has no central application backend or backup service. iroh address lookup and packet relays can establish a path, but relays see only encrypted quic packets and cannot interpret nova rpc or loro state. offline edits converge when an iroh path becomes available again.

## settings, discovery, and device switching

**settings → nova engine** exposes:

- local device identity and listener port;
- pairing-code generation;
- iroh-ticket and pairing-code inputs plus a one-click ticket copy action;
- editable private cidr input and signed discovery-only websocket probes;
- paired-device test, edit, role, revoke, and forget actions.

paired non-revoked identities stream through `NovaWatchPeers` and merge into the ordinary device list immediately. pi pages expose the selected device in the **pi runs on** control; projects and chats retain their owning device id so repositories, terminals, uploads, models, accounts, and changes route to the correct engine.

discovery performs a websocket upgrade and verifies a signed public challenge containing the device's iroh ticket, without transmitting authentication, pairing codes, or rpc data. ranges are explicit, private by default, deduplicated, limited to 65,536 unique addresses, scanned with at most 64 concurrent probes, and given a 250 ms per-host timeout.

## process lifecycle

headed mode first probes local ipc. if a daemon answers, the ui attaches to it. otherwise the ui embeds an engine, starts the nova listener and peer-sync loop, and serves the same engine on ipc so a tui can attach. headless mode starts the same runtime and keeps it alive independently of any viewport.

engine startup has no hosted sign-in phase. networking may contact iroh's default address-lookup/relay network or the relay selected by `NOVA_IROH_RELAY_URL`; optional release checking uses `NOVA_UPDATE_URL`.

## transport security boundary

peer rpc uses iroh quic with tls encryption and endpoint-id authentication derived from nova's existing ed25519 seed. nova's signed nonce handshake binds the stable engine device id to that endpoint key, and method authorization still re-reads the local trust record for every call. the tcp websocket on `NOVA_PORT` is discovery-only public metadata and closes immediately after its signed challenge.

## verification

```sh
cargo test -p comet-nova --lib
cargo test -p comet-engine --test device_routing
cargo test -p comet-engine
cargo test -p comet-ui --lib
cargo test -p comet-tui
cargo clippy --workspace --all-targets -- -D warnings
```

`device_routing.rs` starts two engine identities and local-only iroh endpoints, pairs them, verifies bidirectional unary and streaming rpc, exercises remote pi configuration, and proves workspace projects and transcripts converge through the encrypted transport.

## remaining proof

- verify pairing, revocation, reconnection, and convergence on two physical devices;
- verify direct-path upgrades and relay fallback across the intended pi deployment network, including sleep/wake and changed addresses;
- deploy and exercise the intended self-hosted iroh relay through `NOVA_IROH_RELAY_URL`.
