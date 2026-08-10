# Parity checklist

Status of the native rewrite against `docs/research/feature-inventory.md`
(§1–§8), audited after the direct-Nova migration. Legend: **done** (implemented and
tested), **partial** (core in place, listed gaps), **deferred** (intentionally
not built yet).

## §1 Desktop app

| Item | Status | Notes |
| --- | --- | --- |
| 1.1 Window shell | partial | gpui window, always-dark theme, external links via OS browser. Deferred: frameless-inset/traffic-light chrome (macOS packaging not executed), single-instance lock, dev-vs-packaged port split (env vars instead). |
| 1.2 App phases | done | Local engine boot / failure / app with crossfade; no hosted sign-in or organization gate. |
| 1.3 Shell layout | done | Collapsible drag-resizable sidebar (208–400), right Changes pane (360–760, 52% cap), header variants, widths persisted to `ui-settings.json`. |
| 1.4 Keyboard shortcuts | done | Customizable keymap, click-to-record with conflict detection, per-row reset (`ui/src/settings/shortcuts.rs`); persisted with UI settings. |
| 1.5 Routes | partial | Native navigation instead of URL routes; Nova devices / agents / shortcuts / archived settings pages exist. Profile page (heatmap) is an §8 exclusion. |
| 1.6 Sidebar | done | New session, grouped-by-project or flat, status dots (staleness-checked), row context menu (rename/archive/delete), resort glide. |
| 1.7 Composer | done | Send/Steer/Stop morph, compact↔expanded flip, per-chat drafts, optimistic echo with failure return-to-draft, QuestionPanel (paged, auto-advance, number keys), all four pickers (harness/model, traits, repo with folder browser + clone/create, branch with worktree toggle), image attachments (paste/drop/picker → strip → chunked upload to host device → `withAttachments` refs in prompt text + inline image blocks for the Claude harness; per-chat stash, failure hand-back, lightbox — `ui/src/attachments.rs`). |
| 1.8 Transcript | done | Doc-projection source, virtualized, markdown + syntax highlight, tool folding (ToolGroup/ToolChip), input/error chips, stick-to-bottom band, MessageRail minimap (hover preview, hidden < 48rem), user-bubble attachment thumbnails (112×80, read-back from owning device, 2s→15s retry ladder, seeded cache, click-to-expand lightbox). |
| 1.9 Accounts settings | done | Provider cards, usage meters with 80/95% thresholds + reset time, Switch/Forget, paste-code and browser-poll add flows, device switcher (`targetDeviceId`). |
| 1.10 Terminal panel | done | Session-scoped tabs, drag-reorder, middle-click close, height drag, replay-then-tail streams, input coalescing, ANSI emulator (`ui/src/terminal/`). |
| 1.11 Changes viewer | done | Patch → file/hunk/line rows, per-file collapse, ±gutters, time-sliced highlighting, preparing/clean/error states, checkout_id → device+cwd resolution. |
| 1.12 Motion catalog | partial | Motion kit (cubic-bezier curves, fade-in/quick, splash-out, pulse/gradient spinners, menu/dialog-in, resort glide). Gap: prefers-reduced-motion switch. |
| 1.13 State & connection | done | WatchDevices/Chats/Sessions/CheckoutDiffs, per-chat WatchDocMessages, LocalDevice and Nova peer probes; reconnect from scratch. |

## §2 Control plane

| Item | Status | Notes |
| --- | --- | --- |
| ListHarnesses / ListModels | done | Direct Nova-forwardable. |
| Run/Subscribe/Interrupt/Steer/RespondInput RPCs | done (changed shape) | Deliberate redesign: these ride the durable doc command queue (`QueueCommand {run|steer|interrupt|respondInput}`) instead of device-addressed RPCs — same capability, offline-tolerant. |
| Repos/folders/worktrees RPCs | done | All eight methods, direct Nova-forwardable. |
| Uploads / ReadAttachmentChunk | done | Chunked staging → device-local durable file; path-jailed reads and direct Nova forwarding. |
| Terminals RPCs | done | Open/Subscribe/Write/Resize/Close, forwardable. |
| Agent-account RPCs | done | Full login/activate/forget/poll surface, forwardable. |
| LocalDevice | done | `{deviceId}` for the connected engine; never forwarded by `targetDeviceId`. |
| DataRpc watches + QueueCommand | done | — |
| Mutate ops | done | Project/chat create, rename, archive, delete, host/cwd/branch/config updates, device rename, and seen-state writes are exposed. |
| Hosted AuthRpc | removed | Nova starts its pi-coupled local engine immediately; WorkOS and organization bootstrap RPCs are not served. |
| Wire types | done | `comet-proto`: AgentEvent, ToolCall kinds, models/options, and local workspace entities. Hosted auth wire types are removed. |

## §3 Backend engine

| Item | Status | Notes |
| --- | --- | --- |
| 3.1 Lifecycle | partial | Local engine starts without authentication, direct Nova listener starts beside IPC, stale-session recovery and single-instance data-dir lock remain. `comet daemon install/start/stop/restart/status/uninstall` manages launchd / systemd `--user` units. Gaps: login-shell PATH capture, crash shield, parent-PID watchdog. |
| 3.2 Sessions engine | partial | Run journal on disk with crash recovery (aborted stamps), steering mailbox at step boundaries, doc hooks at boundaries, streamed part folding at STREAM_COMMIT_MS. Gaps: idle reaper + 10-min stall watchdog for persistent harness sessions. |
| 3.3 Session-docs host | done | docs.sqlite snapshots + processed-command ledger, mark-BEFORE-execute, and on-demand chat handles. Paired engines exchange transcript updates directly through `NovaSyncApply`; imported commands wake the owning executor without a room or nudge service. |
| 3.4 Terminals | done | PTYs, 1MB bounded replay + `afterSeq` resume, 32 max, exited 30-min TTL, live shells survive detach. |
| 3.5 Repos/diffs | done | list/add/clone/create, branches, worktrees, checkout identity; CheckoutDiffSync (fs watchers + repair pass, name-status+numstat+patch incl. untracked, 3MiB cap, sha256); chat.branch upkeep from HEAD watch; folder listing with timeout. |
| 3.7 Nova / uploads / accounts | partial | Ed25519 device pairing, encrypted iroh RPC with hole punching and relay fallback, trust configuration, direct Loro convergence, chunked uploads, and device-local agent/Pi credentials are wired. Hosted auth, rooms, host relay, and peer link cache are removed. Remaining: physical-device and self-hosted-relay proof. |

## §4 Harness

| Item | Status | Notes |
| --- | --- | --- |
| Claude Code adapter | done | stream-json, model discovery/effort ladders, AskUserQuestion → requestInput, steering via persistent input, init dedup, subagent filtering. **Live-verified against the real `claude` CLI 2.1.215**: doc-queued run → host executor → subprocess → streamed reply landed complete in the doc. |
| Codex adapter | done | `codex app-server` JSON-RPC (thread/start/resume, sandbox policy). |
| Cursor adapter | deferred | Parity item scheduled after Codex; no CLI surface settled. |
| Mock harness | done | Scripted event replay; powers tests + the e2e smoke. |

## §5 Session doc schema

| Item | Status | Notes |
| --- | --- | --- |
| Containers (meta/messages/commands), LoroText bodies | done | Shape-compatible with TS `packages/session-doc`; `tokens` dropped per §8. |
| Command rules (append-only, host outcome writer, evaluateCommand) | done | Processed-ledger dedupe, TTL, supersede rules. |
| Continuation splitting / joining (MSG_INLINE_MAX 256KB) | done | `split at part boundaries`, `root#cN`, render-time join. |
| Render-parts privacy policy | done | WriteFile content / Edit bodies / etc. stripped; full inputs only in the host journal. |

## §6 Hosted edge

| Item | Status | Notes |
| --- | --- | --- |
| Cloudflare worker and Durable Objects | removed | Workspace and transcript Loro deltas now converge directly between paired Nova Engines. |
| WorkOS and organization service | removed | Engine and UI boot without hosted identity or account gates. |
| DeviceRoom relay and nudges | removed | Device-addressed RPC and command delivery use authenticated direct connections. |
| R2 attachment mirror and diff/tail sidecars | removed | Attachments and diffs stay device-local and are read through direct RPC. |
| Legacy iOS edge client | removed | It depended exclusively on the deleted auth, room, and relay protocols; a future mobile client must speak Nova. |

## §7 Server replacement

| Item | Status | Notes |
| --- | --- | --- |
| Pi-coupled local backend | done | `EngineCore` owns Pi configuration, credentials, packages, sessions, repositories, terminals, documents, and updates. |
| Cross-device backend | partial | Pairing, encrypted iroh routing, trust management, and Loro convergence are implemented. Physical-device topology and self-hosted-relay proof remain. |

## §8 Exclusions

| Item | Status | Notes |
| --- | --- | --- |
| Token-usage display dropped | done | No WatchUsage, no doc `tokens`, no profile heatmap; rate-limit meters + Usage AgentEvent passthrough kept as specified. |

## Deferred (cross-cutting)

- **Mobile app** — the obsolete edge-only iOS client was removed; a Nova-native mobile viewport is deferred.
- **Physical iroh topology proof** — exercise direct-path upgrades and the intended
  self-hosted relay between the mac and pi devices.
- **Cursor harness** (§4).
- **macOS packaging execution** — config + steps in `dist/` only (needs a Mac).
- **Engine hardening**: parent-PID watchdog, crash shield, idle reaper, and stall watchdog.

## Summary

the desktop and terminal surfaces now run entirely against the local Pi-coupled engine and paired Nova Engines. the hosted compatibility backend is gone. remaining cross-cutting work is physical iroh topology validation, mobile replacement, packaging execution, and the named engine-hardening gaps.
