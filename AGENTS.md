# AGENTS.md

Rust Windows desktop IPTV player (browse + play in one window, with persistent stream retry). Design: `docs/superpowers/specs/2026-10-17-iptv-player-design.md`.

## Rules
- Playlist URL/credentials live only in gitignored `config.toml`. Never commit it, the sample playlists, or the cache; never print credentialed URLs in logs/errors.
- Branches: `feature/...`, `bugfix/...`. Use `git -C <path>`. Update this file and docs when committing a feature branch.
- Add new source files to git.
- Sample playlist (outside repo, contains credentials): `<samples dir>\`. Reference APK in `<apk dir>\` is a WebView+ExoPlayer shell; nothing to port.
