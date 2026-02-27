# Fast Chat

Rust native Twitch chat viewer (performance-first) using `eframe/egui`.

## Current Status

Implemented foundation and MVP path:
- Rust workspace (`fastchat-core`, `fastchat-twitch`, `fastchat-ui`, `fastchat-desktop`)
- Anonymous Twitch IRC connection via `twitch-irc`
- Reconnect loop + normalized chat/system events
- Persistent config (last channel + global filters)
- Global filter engine (keywords, hidden users, commands, moderation toggles)
- Bounded in-memory chat store (75k default)
- Disk JSONL backlog writer with daily rotation + pruning
- Native `egui` desktop app with virtualized chat list and filter panel
- Badge text-pill fallback rendering
- Emote URL resolution + inline emote placeholders (image rendering scaffolded)
- CI workflow + benchmark skeleton + replay fixture skeleton

Not fully implemented yet:
- Actual image emote rendering / animated emote decoding
- Badge icon metadata fetch and icon rendering
- Asset disk cache/downloader pipeline
- Perf replay harness + frame-timing acceptance automation

## Run

```bash
cargo run -p fastchat-desktop
```

Enter a Twitch channel username and connect.
