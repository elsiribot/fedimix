# Decisions

- Commits are made as repo-local identity `elsiribot` because no git identity was configured.
- `scripts/screenshot.sh` builds the binary before starting sway, so the 15 s window-map timeout excludes compilation.
- `just screenshot` writes to `target/screenshots/<name>.png`, which is already gitignored via `/target/`.
- Screenshot harness waits for sway app_id `fedimix` (what GTK reports under headless sway), not the reverse-DNS application id.
