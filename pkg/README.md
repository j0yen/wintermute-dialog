# wintermute-dialog packaging

systemd-user service files and an installer for `wm-dialog`. Pairs with
the system-scope units from `wintermute-platform`.

## What's here

- `systemd/user/wm-dialog.service` — user-scoped unit that runs
  `wm-dialog start` as the long-lived agorabus participant. `PartOf` and
  `WantedBy` `wintermute.target` so it follows the laptop's voice-stack
  lifecycle.
- `install-user.sh` — copies the unit into `~/.config/systemd/user/`,
  reloads, and enables (without `--now`).

## Install

```sh
cargo install --path . --locked --root "$HOME/.local"
pkg/install-user.sh
systemctl --user start wintermute.target   # or just wm-dialog.service
```

## Unit notes

- `After=wmd-init.service` so the supervisor + agorabus are ready
  before the FSM tries to subscribe. `Wants=` (not `Requires=`) keeps
  wm-dialog restart-survivable even if wmd-init flaps.
- `Restart=on-failure` with `RestartSec=2`. Successful `wm-dialog stop`
  shutdowns (when wired) exit 0 and won't loop.
- `Environment=RUST_LOG=info` matches the rest of the wintermute fleet;
  override per-user via `systemctl --user edit wm-dialog.service`.
- No `EnvironmentFile=` — `wm-dialog` does not read any environment
  variables today (only `RUST_LOG`, set above). If iter-N adds env
  config, hang it off `/etc/wintermute/conf.d/00-bootstrap.env` like
  `wmd-init` does.
