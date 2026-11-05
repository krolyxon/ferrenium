#!/bin/sh
# Install or upgrade Ferrenium from its GitHub releases.
#
#   curl -fsSL https://github.com/krolyxon/ferrenium/releases/latest/download/install.sh | sudo sh
#
# Options, as environment variables (put them after `sudo`):
#   FERRENIUM_DOMAIN=git.example.com  fill in base_url and clone URLs (fresh config only)
#   FERRENIUM_USER=git                user that owns the repositories (created if missing)
#   FERRENIUM_VERSION=v0.1.0          a specific release instead of the latest
#
# `sh -s -- --uninstall` removes the binary and the unit, and keeps
# /etc/ferrenium, the user and the repositories.
#
# Upgrades replace the binary and the unit and restart the service; the
# config is never overwritten. Customize the unit with `systemctl edit
# ferrenium`, not by editing the file.

set -eu

REPO=krolyxon/ferrenium
BIN=/usr/local/bin/ferrenium
ETC=/etc/ferrenium
CONFIG=$ETC/ferrenium.toml
UNIT=/etc/systemd/system/ferrenium.service

say() { printf '%s\n' "$*"; }
die() { printf 'ferrenium install: %s\n' "$*" >&2; exit 1; }
has() { command -v "$1" >/dev/null 2>&1; }

fetch() {
	if has curl; then
		curl -fsSL -o "$2" "$1"
	elif has wget; then
		wget -qO "$2" "$1"
	else
		die "curl or wget is required"
	fi
}

uninstall() {
	if [ -d /run/systemd/system ]; then
		systemctl disable --now ferrenium 2>/dev/null || true
	fi
	rm -f "$UNIT" "$BIN"
	if [ -d /run/systemd/system ]; then
		systemctl daemon-reload
	fi
	say "Removed $BIN and $UNIT. Kept $ETC and the repositories."
}

main() {
	[ "$(id -u)" -eq 0 ] || die "run as root (… | sudo sh)"
	if [ "${1:-}" = "--uninstall" ]; then
		uninstall
		return
	fi

	[ "$(uname -s)" = Linux ] || die "only Linux is supported"
	case "$(uname -m)" in
		x86_64 | amd64) arch=x86_64 ;;
		aarch64 | arm64) arch=aarch64 ;;
		*) die "no release build for $(uname -m); build from source instead" ;;
	esac
	has sha256sum || die "sha256sum is required"
	has git || say "warning: git is not installed; HTTPS clone needs it (apt install git)"

	version=${FERRENIUM_VERSION:-latest}
	if [ -n "${FERRENIUM_BASE_URL:-}" ]; then
		base=$FERRENIUM_BASE_URL
	elif [ "$version" = latest ]; then
		base=https://github.com/$REPO/releases/latest/download
	else
		base=https://github.com/$REPO/releases/download/$version
	fi
	asset=ferrenium-$arch-linux.tar.gz

	tmp=$(mktemp -d)
	trap 'rm -rf "$tmp"' EXIT
	say "Downloading $asset ($version)"
	fetch "$base/$asset" "$tmp/$asset" || die "download failed: $base/$asset"
	fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || die "download failed: $base/SHA256SUMS"
	(cd "$tmp" && grep " $asset\$" SHA256SUMS | sha256sum -c - >/dev/null) ||
		die "checksum mismatch for $asset"
	tar -xzf "$tmp/$asset" -C "$tmp" ferrenium
	"$tmp/ferrenium" --version >/dev/null 2>&1 ||
		die "the downloaded binary does not run here (needs glibc 2.28 or newer)"

	# The service runs as the user that owns the repositories.
	user=${FERRENIUM_USER:-git}
	if ! id "$user" >/dev/null 2>&1; then
		shell=$(command -v git-shell || echo /usr/sbin/nologin)
		useradd --system --create-home --home-dir "/home/$user" --shell "$shell" "$user"
		say "Created user $user (home /home/$user, shell $shell)"
	fi
	group=$(id -gn "$user")
	home=$(getent passwd "$user" | cut -d: -f6)

	install -m 755 "$tmp/ferrenium" "$BIN.new"
	mv -f "$BIN.new" "$BIN"
	say "Installed $("$BIN" --version) to $BIN"

	if [ -e "$CONFIG" ]; then
		say "Kept $CONFIG"
	else
		mkdir -p "$ETC"
		"$BIN" example config | sed "s|^scan_path = .*|scan_path = \"$home\"|" >"$tmp/config"
		if [ -n "${FERRENIUM_DOMAIN:-}" ]; then
			d=$FERRENIUM_DOMAIN
			sed -i \
				-e "s|^# base_url = .*|base_url = \"https://$d\"|" \
				-e "s|^# clone_urls = .*|clone_urls = [\"https://$d/{name}\", \"$user@$d:{name}\"]|" \
				"$tmp/config"
		fi
		install -m 644 "$tmp/config" "$CONFIG"
		say "Wrote $CONFIG"
	fi

	fresh=yes
	[ -e "$UNIT" ] && fresh=no
	"$BIN" example systemd |
		sed -e "s/^User=.*/User=$user/" -e "s/^Group=.*/Group=$group/" >"$tmp/unit"
	install -m 644 "$tmp/unit" "$UNIT"

	if [ ! -d /run/systemd/system ]; then
		say "systemd is not running; start it yourself:"
		say "  $BIN serve --config $CONFIG"
		return
	fi
	systemctl daemon-reload
	"$BIN" check --config "$CONFIG" >/dev/null || die "$CONFIG is invalid; fix it, then run: systemctl enable --now ferrenium"
	if [ "$fresh" = yes ]; then
		systemctl enable --now ferrenium
		say "Started ferrenium.service"
	elif systemctl is-active --quiet ferrenium; then
		systemctl restart ferrenium
		say "Restarted ferrenium.service"
	fi

	say ""
	say "Repositories: bare repos under $home"
	say "Config:       $CONFIG (then: systemctl reload ferrenium)"
	say "Logs:         journalctl -u ferrenium"
	say "Proxy:        ferrenium example caddy"
}

main "$@"
