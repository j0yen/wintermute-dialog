#!/usr/bin/env bash
# install-user.sh — install wm-dialog as a systemd-user service.
#
# Idempotent: re-running drops the unit fresh, runs daemon-reload, and
# enables (without --now) so the next `systemctl --user start
# wintermute.target` picks it up. Does NOT start the service directly;
# that's wintermute.target's job (see PRD-wintermute-platform).
#
# Requires:
#   - ~/.local/bin/wm-dialog (build + install the binary first; e.g.
#     `cargo install --path . --locked` from the repo root)
#   - systemd-user session (true under any modern Arch desktop login)
#
# Does NOT install wintermute.target itself — that ships with
# wintermute-platform. If you haven't installed wintermute-platform yet,
# `systemctl --user enable wm-dialog.service` will warn that the target
# is unknown; that's expected and harmless.

set -euo pipefail

here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
unit_src="$here/systemd/user/wm-dialog.service"
unit_dst_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
unit_dst="$unit_dst_dir/wm-dialog.service"

if [[ ! -x "$HOME/.local/bin/wm-dialog" ]]; then
  echo "warn: ~/.local/bin/wm-dialog not found; install the binary before enabling." >&2
  echo "      e.g. cargo install --path . --locked --root \"\$HOME/.local\"" >&2
fi

mkdir -p "$unit_dst_dir"
install -m 0644 "$unit_src" "$unit_dst"

systemctl --user daemon-reload
systemctl --user enable wm-dialog.service

echo "wm-dialog.service installed and enabled (not started). Start via:" >&2
echo "  systemctl --user start wintermute.target" >&2
echo "or directly:" >&2
echo "  systemctl --user start wm-dialog.service" >&2
