# Fast Chat

A fast native Twitch chat viewer in Rust (`eframe/egui`), made for smooth reading and popout use.

It only needs a channel name to get going.

## What this thing does rn

- Connects to public Twitch chat (anonymous read)
- Remembers your last channel + app settings
- Lets you filter chat by keywords, badge types, user groups, hidden users, etc
- Shows emotes and badges (with fallbacks if a badge source is being weird)
- Has a popout window for chat
- Lets you click a message in main chat and mirror only that one in popout
- Has custom popout message overlay (centered box over dimmed/blurred chat)
- Saves main window + popout window size/position when closing

## Project layout

- `apps/fastchat-desktop` -> app entrypoint
- `crates/fastchat-core` -> models, filters, store, config, backlog
- `crates/fastchat-twitch` -> twitch ingest + normalizing
- `crates/fastchat-ui` -> egui app and rendering

## Run it

```bash
cargo run -p fastchat-desktop
```

Then:
1. Open sidebar
2. type your channel username
3. click connect

thats pretty much it.

## Controls (quick mental map)

- Sidebar has all tools now:
  - Connection
  - Typography
  - Popout
  - Runtime
  - Filters / Appearance
- Main chat has:
  - open/close sidebar button
  - stick to bottom
  - jump to latest

## Notes / still kinda rough

- Twitch stuff changes sometimes, so some badge data paths can fail and fallback
- Performance is good for normal/high traffic, but there is still tuning left for super cursed burst chat
