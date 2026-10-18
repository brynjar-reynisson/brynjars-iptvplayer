---
name: build-and-deploy
description: Use when the user wants to build the IPTV player release and put the latest version in dist\iptv-player (the folder their taskbar shortcut points at), or says "deploy", "rebuild the app", "update the installed copy". Builds the release exe and copies it plus libmpv-2.dll into dist without touching their config, cache or favorites.
---

# Build and deploy the IPTV player

The user runs the app from `dist\iptv-player\iptv-player.exe` (a pinned taskbar shortcut points there). This skill rebuilds and refreshes that folder. Run it from the repo root `C:\Users\Lenovo\misc_projects\brynjars-iptvplayer`.

## Steps

1. **Check whether the app is running.** `Get-Process iptv-player -ErrorAction SilentlyContinue`. A running exe is locked and the copy would fail.
   - If it is running, tell the user and ask before closing it: they may be watching something. Only stop it (`Stop-Process -Name iptv-player`) after they say yes. Never kill it unasked.
2. **Make sure the tests still pass** unless the user said to skip them: `cargo test --workspace` (cargo is in `%USERPROFILE%\.cargo\bin`). Stop and report if anything fails. Do not deploy a failing build.
3. **Build and copy:**
   ```powershell
   powershell -ExecutionPolicy Bypass -File scripts\package-release.ps1
   ```
   This runs `cargo build --release -p iptv-player` and copies `iptv-player.exe`, `libmpv-2.dll` (from `libmpv\`) and `config.example.toml` into `dist\iptv-player\`. It does **not** touch `config.toml`, `cache\`, `favorites.json` or `settings.json` in that folder: the app keeps its data beside the exe, so those survive a redeploy.
   - If it fails with "libmpv\libmpv-2.dll not found", follow README.md (libmpv section) to fetch it; do not guess.
4. **Verify the result.** `Get-ChildItem dist\iptv-player` shows the exe (new timestamp), the DLL, and `config.example.toml`. If there is no `config.toml` in the folder, tell the user to copy `config.example.toml` to `config.toml` and set `playlist_url` (their credentials; never print or read the file's contents).
5. **Report** the exe's path and build time, and what changed (`git log --oneline -5`). Do not launch the app unless asked.

## Taskbar shortcut (one-time, for the user)

Right-click `dist\iptv-player\iptv-player.exe` → Show more options → Pin to taskbar (or create a shortcut and pin that). The app finds its config, cache and favorites next to the exe, so the shortcut's "Start in" folder does not matter. The pin keeps working across redeploys because the path does not change.

## Rules

- Never commit `dist\`, `config.toml`, the cache or `libmpv-2.dll` (all gitignored).
- Never print the playlist URL or the contents of `config.toml`.
- Releases are built from the current working tree; if it has uncommitted changes, mention that in the report.
