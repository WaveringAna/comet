# Nova

A fork of [https://github.com/zeronsh/comet](comet) where the backend will be replaced to hook into pi. many many thanks to zeronsh for the shell

will be heavily vibed

## developing

one command, no env vars:

```sh
scripts/nova-dev.sh
```

starts the engine daemon (if it isn't already running), builds the gui, and opens a window. edit anything under `crates/ui` or `apps/comet` and save — the script rebuilds in the background and swaps the window in place once the new one reports ready. your chats, terminals, and agent sessions live in the daemon, so nothing is lost on swap. failed builds leave the old window alone.

there's also a **settings → developer → hot reload** toggle that enables the same ready/restore contract for windows launched outside the script.

todos
- [] rip out claude code/codex support
- [] theming + settings work + maybe atproto for settings storage? or not and let ↓ handle it
- [] rip out daemons, have nova be able to connect to other novas with iroh + web support
- [] compile list of good pi extensions 
- [] reimplement them so we dont run risk of being npm wormed and clean up vibiness
- [] install script to install pi, these extensions, nova
- [] pi extension to have a discord thread per session where the agent can interact in
