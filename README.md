# Ferrenium

A read-only Git repository browser, like cgit, as a single Rust daemon. It
serves every bare repository under one directory and keeps no state on disk.

Pages: repo index, summary + README, tree, file (highlighted), raw, log
(path filter, message search), commit + diff, `.patch`, blame, refs, file
finder, `.tar.gz`/`.zip` snapshots, Atom feed. `git clone` over HTTPS
(read-only).

Themeable without rebuilding: your own stylesheet, images and fonts, plus a
favicon, logo and accent color (see [Appearance](#appearance)).

## Install

On a Linux host with systemd (x86_64 or aarch64, glibc 2.28+):

```sh
curl -fsSL https://github.com/krolyxon/ferrenium/releases/latest/download/install.sh | sudo FERRENIUM_DOMAIN=git.example.com sh
```

This:

- downloads the latest release binary, verifies its checksum and installs it to `/usr/local/bin/ferrenium`
- creates the `git` user if it doesn't exist (home `/home/git`, shell `git-shell`)
- writes `/etc/ferrenium/ferrenium.toml` (never overwritten on later runs)
- installs `/etc/systemd/system/ferrenium.service` and runs `systemctl enable --now ferrenium`

Variables: `FERRENIUM_DOMAIN` (fills `base_url` and clone URLs in a new
config), `FERRENIUM_USER` (default `git`), `FERRENIUM_VERSION` (e.g.
`v0.1.0`, default latest).

Run the same command again to upgrade: it replaces the binary and unit
and restarts the service. To remove it: `… | sudo sh -s -- --uninstall`.
This keeps `/etc/ferrenium` and the repositories.

### The service

It's a normal system service. The unit has `User=git`, so systemd starts it
as the repo owner. You don't need `su git` or `systemctl --user`.

```sh
sudo systemctl status ferrenium
sudo systemctl reload ferrenium      # after editing the config
sudo journalctl -u ferrenium -f
sudo systemctl edit ferrenium        # overrides; survive upgrades
```

The unit is sandboxed: the filesystem is read-only, there are no
capabilities, a syscall filter applies, and only localhost may connect.

### Reverse proxy

Ferrenium listens on `127.0.0.1:8080` (plain HTTP/1.1). Caddy:

```caddy
git.example.com {
	reverse_proxy 127.0.0.1:8080
}
```

If Caddy runs in Docker, use `network_mode: host` so it can reach
`127.0.0.1`.

### Repositories

Put bare repositories anywhere under `/home/git` (up to 3 levels deep).
Ferrenium picks up new repos and pushes within about a second.

```sh
sudo -u git git init --bare /home/git/project.git
git push git@git.example.com:project.git main   # after adding your key to /home/git/.ssh/authorized_keys
```

## Configuration

Edit `/etc/ferrenium/ferrenium.toml`. Every key is optional and unknown
keys are errors. The file lists all keys with their defaults
(`ferrenium example config` prints the same). To validate the config and
list the repositories it finds:

```sh
ferrenium check --config /etc/ferrenium/ferrenium.toml
```

`listen`, `base_path`, `compression`, `cache.size_mb` and the `limits.*_parallel`/`max_requests`
settings need a restart; everything else applies on reload.

Per-repository metadata comes from the first source that sets it:

1. `[[repo]]` in the config (`name`, `description`, `section`, `owner`, `hidden`, `readme`, `default_branch`)
2. the repo's git config: `ferrenium.{description,owner,section,hidden,readme}`, or the gitweb/cgit keys
3. the repo's `description` file

```sh
sudo -u git git -C /home/git/secret.git config ferrenium.hidden true
```

### Appearance

These keys are optional. Files are read when the config is loaded or reloaded,
and relative paths are resolved against the config file's directory, e.g.
`/etc/ferrenium/`.

| Key | |
|---|---|
| `site.title` | name in the top bar and page titles |
| `site.favicon` | `.ico`, `.png`, `.svg`, `.webp`, `.gif` or `.jpg`, up to 1 MiB |
| `site.logo` | image shown before the title in the top bar (same formats) |
| `site.custom_css` | stylesheet loaded after the built-in one |
| `site.assets_dir` | directory of images, fonts and stylesheets the custom CSS can load |
| `site.header` / `site.footer` | HTML inserted below the top bar / in the footer |
| `ui.theme` | default theme: `auto`, `dark` or `light` |
| `ui.accent` | accent color as hex, e.g. `"#d71921"` |
| `ui.code_font` | font family for code (must be installed on the client or loaded by `custom_css`) |

The custom stylesheet overrides the built-in one, so it only needs the rules
you want to change: design tokens, buttons, backgrounds, layout. Files in
`assets_dir` are served at `/-/site/` (images, fonts and `.css` only; hidden
files and paths leading outside the directory are refused), and the
stylesheet refers to them with relative URLs:

```toml
[site]
custom_css = "style.css"     # /etc/ferrenium/style.css
assets_dir = "assets"        # /etc/ferrenium/assets/
```

```css
@font-face { font-family: "Inter"; src: url("site/inter.woff2") format("woff2"); }
:root { --font-body: "Inter", sans-serif; --interactive: light-dark(#0b57d0, #8ab4f8); }
body { background: url("site/bg.jpg") center / cover fixed; }
.segmented { border-radius: 4px; }
```

Edits to the stylesheet, favicon and logo need `systemctl reload ferrenium`;
files in `assets_dir` are read on each request. Images from other `https://`
sites work too, but fonts must come from `assets_dir` (the content security
policy allows only same-origin fonts). Inline `<script>`/`<style>` in
`header`/`footer` is blocked for the same reason; put styles in `custom_css`.

Most of the look is driven by these CSS variables, so overriding a few of
them on `:root` goes a long way. Colors are written as
`light-dark(LIGHT, DARK)` to follow the theme switch; a single color applies
to both themes.

| Variable | Used for |
|---|---|
| `--black` | page background |
| `--surface`, `--surface-raised` | panels, menus, code blocks |
| `--border`, `--border-visible` | hairlines, button and field outlines |
| `--text-display`, `--text-primary`, `--text-secondary`, `--text-disabled` | text, brightest to dimmest; syntax highlighting uses the same scale |
| `--accent`, `--accent-subtle` | active markers, highlights and removed lines in diffs (`ui.accent` sets both) |
| `--interactive` | links |
| `--success` | added lines in diffs |
| `--error` | `[ERROR: …]` messages |
| `--font-body`, `--font-mono`, `--font-display`, `--font-code` | type families |
| `--page-max`, `--gutter` | column width and side padding |

The full list is in [`assets/css/tokens.css`](assets/css/tokens.css). For
anything else, use the browser's inspector to find the element's class and
override its rules.

### Limits

Defaults are sized for a small VPS:

| Key | Default | |
|---|---|---|
| `limits.git_parallel` | CPUs, max 8 | concurrent git work |
| `limits.heavy_parallel` | 2 | concurrent blame / archive / clone / log search |
| `limits.heavy_wait_secs` | 10 | queue time for those before `503` |
| `limits.max_requests` | 512 | in-flight requests before `503` |
| `limits.request_timeout_secs` | 30 | until the response starts |
| `limits.diff_files` / `diff_lines` | 300 / 10000 | larger diffs are truncated |
| `ui.max_blob_kb` / `max_highlight_kb` | 1024 / 256 | larger files: raw link / no highlighting |
| `cache.size_mb` | 64 | in-memory cache |

`/robots.txt` keeps crawlers off the expensive pages. Memory use at idle is
about 10 MB.

## Building from source

Requires Rust 1.89+.

```sh
cargo build --release              # target/release/ferrenium
ferrenium example config|systemd|caddy
```

## Development

```sh
cargo test                         # needs the git CLI
cargo test --release
cargo clippy --all-targets -- -D warnings
cargo run -- serve --config dev.toml
tools/gen-languages/gen.py         # regenerate the Linguist language table
```

To release: bump `version` in `Cargo.toml`, commit, then push a `vX.Y.Z` tag.
The `release` workflow tests, builds x86_64 and aarch64 binaries, and
publishes them with `install.sh` and `SHA256SUMS`.

## License

GPL-3.0 (see `LICENSE`). Bundled fonts (Doto, Space Grotesk, Space Mono)
are licensed under the OFL; see `assets/fonts/OFL.txt`. Icons come from
Lucide (ISC license).
