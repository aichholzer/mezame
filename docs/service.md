# Running Okiro as a service

Okiro is a foreground process. To start it at boot and keep it running, hand it off to the OS's init system: systemd on Linux, launchd on macOS. Both of these capture Okiro's stderr into the usual places (`journalctl` / Console.app) and send SIGTERM when asked to stop. Okiro handles SIGTERM and exits cleanly.

## Linux (systemd)

Pick one of the two patterns below. User service is the simpler choice for a single-user machine and is what most people want. System service is for multi-user hosts or setups where you want Okiro running completely independently of any login session.

### User service (recommended for single-user machines)

1. Put your unit file at `~/.config/systemd/user/okiro.service`:

   ```ini
   [Unit]
   Description=Okiro
   After=network-online.target
   Wants=network-online.target

   [Service]
   Type=simple
   ExecStart=%h/.cargo/bin/okiro
   Restart=on-failure
   RestartSec=5

   [Install]
   WantedBy=default.target
   ```

   `%h` expands to `$HOME`. Adjust the `ExecStart` path if you installed Okiro somewhere else.

2. Reload, enable, start:

   ```sh
   systemctl --user daemon-reload
   systemctl --user enable --now okiro.service
   ```

3. To have the service keep running after logout and start at boot without needing an active login session, enable lingering once:

   ```sh
   sudo loginctl enable-linger "$USER"
   ```

4. Inspect:

   ```sh
   systemctl --user status okiro
   journalctl --user -u okiro -f
   ```

### System service (multi-user or headless)

1. Put the unit at `/etc/systemd/system/okiro.service`:

   ```ini
   [Unit]
   Description=Okiro
   After=network-online.target
   Wants=network-online.target

   [Service]
   Type=simple
   User=youruser
   Group=youruser
   Environment=HOME=/home/youruser
   ExecStart=/home/youruser/.cargo/bin/okiro
   Restart=on-failure
   RestartSec=5

   [Install]
   WantedBy=multi-user.target
   ```

   Replace `youruser` with the Unix account that has Okiro and the ACP agent (kiro-cli, claude, etc.) installed. The explicit `Environment=HOME=...` matters: system units do not inherit per-user env, and Okiro reads `$HOME/.okiro/config.json`.

2. Enable:

   ```sh
   sudo systemctl daemon-reload
   sudo systemctl enable --now okiro.service
   ```

3. Inspect:

   ```sh
   sudo systemctl status okiro
   sudo journalctl -u okiro -f
   ```

## macOS (launchd)

Install as a LaunchAgent under your user account. This runs Okiro whenever you log in. For always-on operation without a GUI login, use a LaunchDaemon instead (not covered here; launchd daemons need to live under `/Library/LaunchDaemons` and run as root or a system account).

1. Put this plist at `~/Library/LaunchAgents/dev.okiro.plist`, replacing `YOURUSER` with your short username:

   ```xml
   <?xml version="1.0" encoding="UTF-8"?>
   <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
     "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
   <plist version="1.0">
   <dict>
     <key>Label</key>
     <string>dev.okiro</string>
     <key>ProgramArguments</key>
     <array>
       <string>/Users/YOURUSER/.cargo/bin/okiro</string>
     </array>
     <key>RunAtLoad</key>
     <true/>
     <key>KeepAlive</key>
     <dict>
       <key>SuccessfulExit</key>
       <false/>
     </dict>
     <key>StandardOutPath</key>
     <string>/Users/YOURUSER/Library/Logs/okiro.log</string>
     <key>StandardErrorPath</key>
     <string>/Users/YOURUSER/Library/Logs/okiro.log</string>
     <key>EnvironmentVariables</key>
     <dict>
       <key>HOME</key>
       <string>/Users/YOURUSER</string>
       <key>PATH</key>
       <string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
     </dict>
   </dict>
   </plist>
   ```

   `KeepAlive` with `SuccessfulExit=false` restarts on crash but not when Okiro exits cleanly (matches systemd's `Restart=on-failure`). `PATH` matters because Okiro spawns the ACP agent by name if you configured it that way; launchd's default PATH does not include Homebrew.

2. Load it (the modern verb is `bootstrap`; `load` is legacy but still works):

   ```sh
   launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.okiro.plist
   ```

3. Start it now (`RunAtLoad` only fires on login, not on first install):

   ```sh
   launchctl kickstart -k gui/$(id -u)/dev.okiro
   ```

4. Inspect:

   ```sh
   launchctl print gui/$(id -u)/dev.okiro
   tail -f ~/Library/Logs/okiro.log
   ```

5. Stop and uninstall:

   ```sh
   launchctl bootout gui/$(id -u)/dev.okiro
   rm ~/Library/LaunchAgents/dev.okiro.plist
   ```

## Shutdown behaviour

Both systemd `stop` and launchd `bootout` send SIGTERM. Okiro catches it, stops accepting new WebSocket connections, and exits. Live browser sessions drop; the next connect recreates them. If Kiro was mid-turn when Okiro exited, its per-session lockfile may stick around briefly: the next resume attempt detects the dead PID and steals the lock automatically (see `src/session.rs`). No cleanup required from you.

## A note on `--background`

Okiro does not have a `--background` flag and intentionally will not. Daemonising your own process conflicts with how modern init systems track child processes, rotate logs, and decide when to restart. The pattern above is the one every other well-behaved foreground tool uses, and it gives you better observability for free.
