# HAIL

### Hardware Abstraction/Integration Layer

HAIL is the host-side integration service for a emulators like [Snow](https://github.com/twvd/snow)
and SNES9X. It runs until it is stopped and watches for USB storage being
plugged in or removed. Each change triggers the **action chains** set in its
startup configuration `hail.toml`. Inserted disks are scanned (read-only) for
emulator disk images and an optional per-disk `hail.cfg` that can ask for those
images to be mounted in the emulator automatically.

> Snow control is currently a **stub**: HAIL decides what to mount and writes
> the full mount call to its log, but does not yet attach images to Snow.
> Other emulators listed in `[emulators]` are really started with the image,
> on the logged-in user's desktop.

## Contents of a package

| File | Purpose |
|---|---|
| `hail` / `hail.exe` | The service |
| `hail.toml` | Standard startup configuration (fully commented) |
| `README.md` | This file |

## Running

```text
hail [--config <file>] [--once]
hail [--config <file>] --check-config
hail [--config <file>] --install      (Windows, elevated)
hail --uninstall                      (Windows, elevated)
hail --help | --version
```

| Option | Meaning |
|---|---|
| `--config <file>` | Startup configuration. Default: `hail.toml` beside the executable. |
| `--once` | Scan once, run the triggered chains, then exit (no waiting for changes). |
| `--check-config` | Validate the configuration and exit. |
| `--install` | Windows: validate the configuration, register HAIL as the native Windows service `HAIL` and start it (see [Windows](#windows)). |
| `--uninstall` | Windows: stop and remove the `HAIL` service. |
| `--watch-config` | Windows: reload the configuration file when it changes, without a restart (see [Applying configuration changes](#applying-configuration-changes)). With `--install`, the service is registered to run with it. Not with `--once`, `--check-config` or `--uninstall`. |
| `--service` | Windows: used only by the service manager when it starts HAIL. Run from a console it exits with an error. |

Examples:

```powershell
.\hail.exe                                  # run as a service with .\hail.toml (Windows)
.\hail.exe --check-config                   # validate .\hail.toml
.\hail.exe --config D:\hail\kiosk.toml      # use another configuration
.\hail.exe --once                           # report currently mounted USB volumes and exit
```

```sh
./hail                                      # run with ./hail.toml (Linux / Raspberry Pi)
./hail --config /etc/hail/hail.toml --check-config
```

While running, HAIL prints its log location, detection mode and each event to
standard output, for example:

```text
hail: log C:\hail\hail.log.txt
hail: log C:\hail\hail-2026-11-08.log (pattern hail-{date}.log, new file per period, local time)
hail: detection usb-volumes, OS hardware events (settle 500 ms)
hail: usb.inserted device="E:\\" hail_cfg="found" images="2" label="MACDISKS" mount_point="E:\\"
```

Press Ctrl+C (or close the console window; on Linux send `SIGTERM`) to stop
HAIL: it first stops the programs it started (see
[Programs HAIL starts](#programs-hail-starts)) and exits with code `0`.

### Exit codes

| Code | Meaning |
|---|---|
| `0` | Normal exit (`--help`, `--version`, `--check-config`, `--once`), a `hail.quit` action, a Windows service stop, Ctrl+C, or `SIGINT`/`SIGTERM`/`SIGHUP` |
| `1` | Error: bad arguments or configuration, a failing action, or loss of the OS event subscription |
| `75` | A `hail.restart` action: whatever supervises HAIL should start it again |

Whatever starts HAIL at boot should start it again after codes `75` and `1`,
and leave it stopped after `0`. The install guide below sets this up. To try
it by hand, a simple supervisor loop works on either OS:

```powershell
do { & .\hail.exe; $code = $LASTEXITCODE } while ($code -eq 75)
```

```sh
while ./hail; [ $? -eq 75 ]; do :; done
```

## Installing as a system service

These steps make HAIL start with the computer and keep running in the
background like any other service. Both set-ups:

- install the package into one folder (`hail.toml` and the log live beside the
  executable);
- check the configuration before starting HAIL;
- restart HAIL after `hail.restart` (75) or an error (1), but not after
  `hail.quit` (0).

They run HAIL as a system account (Windows `SYSTEM`, Linux `root`), before
anyone logs in. Emulators that HAIL launches (see [`[emulators]`](#emulators))
still open on the desktop of the user who is logged in: by default
(`emulator_session = "user"` in [`[service]`](#service)) HAIL starts them in
that user's desktop session, as that user. If nobody is logged in, the launch
is skipped and logged. See
[Running in a user's desktop session](#running-in-a-users-desktop-session).

### Windows

`hail.exe` is a native Windows service: it installs itself with the Windows
service manager, which starts it at boot and restarts it after `hail.restart`
(75) or an error (1). Run the following in an **elevated** PowerShell (Run as
administrator), using `windows-arm64` instead of `windows-x64` on ARM PCs.

1. Install the files:

   ```powershell
   $package = 'hail-0.1.0-windows-x64-release'
   $dir     = 'C:\Program Files\HAIL'
   Expand-Archive "$package.zip" "$env:TEMP\hail-install" -Force
   New-Item -ItemType Directory $dir -Force | Out-Null
   Copy-Item "$env:TEMP\hail-install\$package\*" $dir -Force
   Remove-Item "$env:TEMP\hail-install" -Recurse -Force
   ```

2. Edit `C:\Program Files\HAIL\hail.toml` as needed, then install and start
   the service:

   ```powershell
   & 'C:\Program Files\HAIL\hail.exe' --install
   ```

   `--install` checks the configuration first and refuses to install if it is
   invalid. It registers the service `HAIL` ("HAIL Snow integration service")
   to start automatically as `LocalSystem`, with the command
   `"C:\Program Files\HAIL\hail.exe" --service`. To use a configuration file
   somewhere else, add `--config <file>`; the absolute path is stored in the
   service command. Add `--watch-config` to have the service pick up edits to
   `hail.toml` without a restart (see
   [Applying configuration changes](#applying-configuration-changes)); to
   change this later, run `--uninstall` and `--install` again.

   Recovery is set to restart the service 2 seconds after the 1st and 2nd
   failure and 10 seconds after later ones (the count resets after a day).
   Windows counts any non-zero exit code as a failure, so `hail.restart` (75)
   and errors (1) restart HAIL, while `hail.quit` (0) and stopping the service
   leave it stopped.

3. Check that it is running and look at the log:

   ```powershell
   Get-Service HAIL
   Get-Content 'C:\Program Files\HAIL\hail.log.txt' -Tail 20 -Wait
   ```

   Plug in a USB drive: a `usb.inserted` entry should appear in the log within
   a second or two.

Managing the service (also possible in the Services app, `services.msc`):

| Task | Command (elevated) |
|---|---|
| Stop / start / restart | `Stop-Service HAIL` / `Start-Service HAIL` / `Restart-Service HAIL` |
| Status and last exit code | `sc.exe query HAIL` (`SERVICE_EXIT_CODE` is the HAIL exit code) |
| Apply a changed `hail.toml` | `& 'C:\Program Files\HAIL\hail.exe' --check-config; if ($?) { Restart-Service HAIL }`, or just save it if installed with `--watch-config` (see [Applying configuration changes](#applying-configuration-changes)) |
| Disable / enable at boot | `Set-Service HAIL -StartupType Disabled` / `Set-Service HAIL -StartupType Automatic` |
| Show recovery settings | `sc.exe qfailure HAIL` |

A service has no console, so HAIL's standard output is discarded; use the log.
Errors that stop HAIL are written to the log as `[error]` entries when the
log is open. The exit code is also recorded in Event Viewer (Windows Logs >
System, source Service Control Manager). Stopping the service (or shutting
down Windows) stops the programs HAIL started first (see
[Programs HAIL starts](#programs-hail-starts)).

**Upgrading:** `Stop-Service HAIL`, copy only `hail.exe` (and `README.md`)
from the new package over the installed ones so your `hail.toml` is kept, then
`Start-Service HAIL`.

**Uninstalling:**

```powershell
& 'C:\Program Files\HAIL\hail.exe' --uninstall
Remove-Item 'C:\Program Files\HAIL' -Recurse -Force
```

`--uninstall` stops the service (waiting up to 20 seconds) and removes it.
### Linux (including Raspberry Pi 4/5)

HAIL runs as a **systemd** service. Use the `linux-arm64` package on a
Raspberry Pi 4/5 running a 64-bit OS, and `linux-x64` on Intel/AMD PCs.

HAIL lists USB volumes with `lsblk` (part of `util-linux`, installed by default
on Raspberry Pi OS and most distributions) and only sees volumes that are
**mounted**. Desktop editions mount USB drives automatically, but only while a
user is logged in to the desktop. On a headless system or Raspberry Pi OS Lite,
add a udev rule so drives are mounted as soon as they are plugged in, for
example under `/run/media/system/<label>`:

```sh
sudo tee /etc/udev/rules.d/99-hail-usb-mount.rules >/dev/null <<'EOF'
ACTION=="add", SUBSYSTEMS=="usb", SUBSYSTEM=="block", ENV{ID_FS_USAGE}=="filesystem", RUN{program}+="/usr/bin/systemd-mount --no-block --automount=no --collect $devnode"
EOF
sudo udevadm control --reload-rules
```

systemd unmounts the volume when the drive is unplugged. Volumes mounted this
way belong to `root`. If a disk's `hail.cfg` uses `savefile` (or other
writeback), the emulator, running as the desktop user, must be able to write
to the drive. For FAT/exFAT drives, add that user's IDs to the mount, for
example `--options=uid=1000,gid=1000` before `$devnode` (see `id <user>`).
Drives mounted by the desktop already belong to the logged-in user.

If disks bring their own Linux emulator (`hail.cfg` `emulator.custom = true`
with `custom_emulators = "disk"` or `"any"`), the drive must be mounted
without `noexec`. Desktop automounting (udisks) mounts FAT/exFAT drives with
`showexec`, which marks only `.exe`, `.com` and `.bat` files executable, so a
disk-provided Linux emulator only runs from drives mounted by the udev rule
above (or another mount without `showexec`).

1. Install the files:

   ```sh
   sudo mkdir -p /opt/hail
   sudo tar -xzf hail-0.1.0-linux-arm64-release.tar.gz -C /opt/hail --strip-components=1
   ```

2. Edit `/opt/hail/hail.toml` as needed (`sudo nano /opt/hail/hail.toml`), then
   check it:

   ```sh
   sudo /opt/hail/hail --check-config
   ```

3. Create the unit file `/etc/systemd/system/hail.service`:

   ```sh
   sudo tee /etc/systemd/system/hail.service >/dev/null <<'EOF'
   [Unit]
   Description=HAIL Snow integration service
   After=local-fs.target
   # Give up after 5 failed starts within a minute (e.g. a broken hail.toml).
   StartLimitIntervalSec=60
   StartLimitBurst=5

   [Service]
   Type=simple
   WorkingDirectory=/opt/hail
   ExecStartPre=/opt/hail/hail --check-config
   ExecStart=/opt/hail/hail
   # Restarts after hail.restart (75) and errors (1), not after hail.quit (0).
   Restart=on-failure
   RestartSec=2
   # On stop, only HAIL gets SIGTERM: it stops the programs it started itself
   # (logged); anything left after TimeoutStopSec is killed.
   KillMode=mixed
   TimeoutStopSec=30

   [Install]
   WantedBy=multi-user.target
   EOF
   ```

4. Enable it at boot and start it now:

   ```sh
   sudo systemctl daemon-reload
   sudo systemctl enable --now hail
   ```

5. Check that it is running and look at the log:

   ```sh
   systemctl status hail
   journalctl -u hail -f               # HAIL's console output
   tail -f /opt/hail/hail.log.txt      # HAIL's log file
   ```

   Plug in a USB drive: a `usb.inserted` entry should appear within a second
   or two.

Managing the service:

| Task | Command |
|---|---|
| Stop / start / restart | `sudo systemctl stop hail` / `start hail` / `restart hail` |
| Apply a changed `hail.toml` | `sudo /opt/hail/hail --check-config && sudo systemctl restart hail` |
| Disable / enable at boot | `sudo systemctl disable hail` / `sudo systemctl enable hail` |
| After repeated failures | fix the cause, then `sudo systemctl reset-failed hail` and start it |

After `hail.quit` (exit 0) the service stays stopped until it is started again
or the system reboots. Stopping or restarting the service stops the programs
HAIL started (see [Programs HAIL starts](#programs-hail-starts)).

**Upgrading:** `sudo systemctl stop hail`, extract the new package without
overwriting your configuration, then start the service again:

```sh
sudo tar -xzf hail-<version>-linux-arm64-release.tar.gz -C /opt/hail --strip-components=1 --exclude='*/hail.toml'
```

**Uninstalling:**

```sh
sudo systemctl disable --now hail
sudo rm /etc/systemd/system/hail.service
sudo systemctl daemon-reload
sudo rm -rf /opt/hail
sudo rm -f /etc/udev/rules.d/99-hail-usb-mount.rules && sudo udevadm control --reload-rules
```

### Running in a user's desktop session

Emulators started by HAIL appear on the logged-in user's desktop by default,
even when HAIL itself runs as a service (`emulator_session = "user"`):

- **Windows:** the HAIL service runs in session 0, which has no desktop. For
  each launch it finds the active desktop session (the console first, then a
  connected Remote Desktop session) and starts the emulator there as the
  logged-in user, with that user's environment and on their desktop
  (`winsta0\default`).
- **Linux:** when HAIL runs as `root`, it picks the active graphical login
  session from systemd-logind (X11 or Wayland, not the login screen or
  `root`) and starts the emulator as that user, with their groups, `HOME`,
  `XDG_RUNTIME_DIR`, `DISPLAY`/`WAYLAND_DISPLAY` and session D-Bus.
- When HAIL already runs as the desktop user (set-ups below), the emulator is
  started in HAIL's own session.

The log entry for each launch has a `session:` line saying where the emulator
was started, for example `user session 1 (ana)` on Windows or
`user pi (uid 1000), logind session 3 (wayland)` on Linux. With nobody logged
in, the entry says `not started: no user is logged in`. Set
`emulator_session = "service"` to start emulators in HAIL's own session and
account instead (no visible window when HAIL is a service).

Alternatively, HAIL itself can run as the user logged in to the desktop rather
than as a system account:

- **Windows:** services run in a separate session without a desktop, so
  instead of `--install`, start HAIL from **Task Scheduler** when the user logs
  on. Install into a folder that user can write to (such as `C:\HAIL`, otherwise
  the log goes to their temporary folder), then create a supervisor script that
  restarts HAIL after `hail.restart` (75) or an error, and register it:

  ```powershell
  $dir = 'C:\HAIL'
  Set-Content "$dir\hail-session.ps1" @'
  # Keeps hail running: restart after hail.restart (75) or an error, stop after hail.quit (0).
  $hail = Join-Path $PSScriptRoot 'hail.exe'
  while ($true) {
      & $hail
      $code = $LASTEXITCODE
      if ($code -eq 0) { break }
      Start-Sleep -Seconds $(if ($code -eq 75) { 2 } else { 5 })
  }
  '@
  $action    = New-ScheduledTaskAction -Execute 'powershell.exe' -WorkingDirectory $dir `
                 -Argument "-NoProfile -NonInteractive -WindowStyle Hidden -ExecutionPolicy Bypass -File `"$dir\hail-session.ps1`""
  $trigger   = New-ScheduledTaskTrigger -AtLogOn -User "$env:COMPUTERNAME\<user>"
  $principal = New-ScheduledTaskPrincipal -UserId "$env:COMPUTERNAME\<user>" -LogonType Interactive -RunLevel Limited
  $settings  = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) `
                 -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -MultipleInstances IgnoreNew
  Register-ScheduledTask -TaskName 'HAIL (session)' -Action $action -Trigger $trigger `
      -Principal $principal -Settings $settings
  ```

  Remove it with `Unregister-ScheduledTask -TaskName 'HAIL (session)'`
  (after `Stop-ScheduledTask` and ending `hail.exe`). Add `--watch-config` to
  the task's arguments to reload configuration changes without a restart.
- **Linux:** install into a folder the user owns (such as `~/hail`), and put
  the unit in `~/.config/systemd/user/hail.service` with the paths changed and
  `WantedBy=default.target`. Then run `systemctl --user daemon-reload` and
  `systemctl --user enable --now hail`. Add
  `sudo loginctl enable-linger <user>` if HAIL should start at boot before that
  user logs in.

## Log

The log is a plain text file named by `[service] log_file`, written **beside the
executable**. If that folder is not writable, or is itself on a USB volume,
HAIL uses the OS temporary folder instead. HAIL never writes to a USB drive: it
only lists and reads files on inserted volumes. Entries are appended, so
restarting HAIL continues the same file.

### Dated log files

`log_file` may contain time tokens. Before each entry is written, HAIL works
out the file name for the current time. When that name differs from the
previous entry's (a new hour, day, week, month or year has started, depending
on the tokens used), HAIL starts the new file. Old files are kept: HAIL never
deletes or renames them.

| Token | Value | Example |
|---|---|---|
| `{year}` | 4-digit year | `2026` |
| `{month}` | month, 2 digits | `11` |
| `{day}` | day of the month, 2 digits | `08` |
| `{hour}` | hour (24-hour clock), 2 digits | `03` |
| `{minute}` | minute, 2 digits | `30` |
| `{date}` | `{year}-{month}-{day}` | `2026-11-08` |
| `{week}` | ISO 8601 week number, 2 digits | `45` |
| `{week_year}` | year the ISO week belongs to (use with `{week}`) | `2026` |
| `{weekday}` | `mon` ... `sun` | `sun` |

| `log_file` | Files |
|---|---|
| `"hail.log.txt"` | one file, never rotated |
| `"hail-{date}.log"` | one file per day: `hail-2026-11-08.log` |
| `"hail-{year}-{month}.log"` | one file per month: `hail-2026-11.log` |
| `"hail-{week_year}-W{week}.log"` | one file per ISO week: `hail-2026-W45.log` |
| `"hail-{date}_{hour}.log"` | one file per hour: `hail-2026-11-08_03.log` |
| `"hail-{weekday}.log"` | seven files reused every week (entries keep being appended) |

Tokens use local time by default; set `log_time = "utc"` to use UTC instead.
The timestamps HAIL writes inside entries (`{timestamp}`, `[debug]` lines) are
always UTC. Use `{{` and `}}` for literal braces in the name. A new file starts
only when an entry is written, so quiet periods create no files.

### Turning logging off

`logging = false` disables the log file: `log.write` actions and `[debug]`
entries are discarded and no file is created. Events are still printed to
standard output. `log_file` may then be left out. If it is given, it is still
checked, so turning logging back on cannot fail at startup.

## Startup configuration (`hail.toml`)

TOML. Unknown sections, keys, events, actions or placeholders are rejected at
startup, so `--check-config` catches typos.

### Applying configuration changes

By default the configuration is read only at startup: check the edited file
with `--check-config`, then restart HAIL (or its service).

- **Windows:** `Restart-Service HAIL` (or stop and start HAIL in its console).
  With `--watch-config` (on the command line, or given to `--install` for the
  service) HAIL instead checks its configuration file every 2 seconds while it
  runs, so no restart is needed. When the file has changed, it is
  validated exactly like at startup. A valid file replaces the running
  configuration (including the log file name, detection mode, `[emulators]`
  and all action chains) and HAIL logs
  `[config] reloaded <file> (...)`. An invalid file is ignored: HAIL keeps
  running with the previous configuration and logs
  `[config] rejected changes to <file>; keeping the running configuration: <error>`
  once, until the file is changed again. Disks already inserted stay tracked,
  `service.started` does not run again, and the emulator state is kept unless
  `[emulator] type` changed.
- **Linux:** `--watch-config` is not available. Check the edited file and
  restart HAIL:
  `/opt/hail/hail --check-config && sudo systemctl restart hail`.

### `[service]`

| Key | Meaning |
|---|---|
| `logging` | `false` disables the log file (see [Turning logging off](#turning-logging-off)). Default `true`. |
| `log_file` | Log file name only (no folders), optionally with time tokens (see [Dated log files](#dated-log-files)). Required unless `logging = false`. |
| `log_time` | Clock used for the time tokens in `log_file`: `"local"` (default) or `"utc"`. |
| `emulator_session` | Where [`[emulators]`](#emulators) executables are started: `"user"` (default), the logged-in user's desktop session, even when HAIL runs as a service; or `"service"`, HAIL's own session and account. See [Running in a user's desktop session](#running-in-a-users-desktop-session). |
| `custom_emulators` | Which emulators a disk's `hail.cfg` may define itself (`emulator.custom = true`): `"none"`, `"defined"` (default; only executables already in `[emulators]`), `"disk"` (only executables on the inserted disk) or `"any"`. See [Emulators defined by the disk](#emulators-defined-by-the-disk-emulatorcustom). Emulators that `hail.toml` defines itself (in `emulator.launch`) are not restricted. |
| `stop_on_remove` | `true` (default): a program started by a disk's `usb.inserted` chain (automount, `emulator.launch`, `process.run`) is stopped when that disk is removed. Actions can override it. See [Programs HAIL starts](#programs-hail-starts). |
| `stop_timeout_ms` | How long a program asked to close may take before it is killed. 100-60000, default 5000. |

### `[polling]`

| Key | Meaning |
|---|---|
| `type` | What is watched. Currently `"usb-volumes"` (mounted USB mass-storage volumes). |
| `mode` | Required. `"os-events"` or `"interval"`. |
| `settle_ms` | `os-events` only, optional. 50-60000, default 500. |
| `interval_ms` | `interval` only, required. 100-3600000. |

- **`os-events`** (recommended): HAIL sleeps until the OS reports a hardware or
  mount change, then rescans 3 times, `settle_ms` apart, because drives are
  usually mounted (or given a drive letter) shortly after they appear. It uses
  no CPU while idle.
  - Windows: device volume arrival/removal notifications. These work without a
    desktop session. A drive letter changed without the volume itself being
    added or removed is picked up at the next device event.
  - Linux / Raspberry Pi: kernel block-device events plus mount-table change
    notifications. No root needed.
- **`interval`**: rescans every `interval_ms`. A drive inserted and removed
  between two scans can be missed.

In both modes, volumes already mounted when HAIL starts are reported as
`usb.inserted`, and a volume moved to a new mount point is reported as
`usb.removed` followed by `usb.inserted`.

### `[emulator]`

| Key | Meaning |
|---|---|
| `type` | Which emulator the `emulator.*` actions control. Currently `"snow"`. |

The section is optional, but any `emulator.*` action requires it. Each
emulator type has its own handler, so other emulators can be added later
without changing the action names. HAIL tracks whether the emulator is running.

### `[emulators]`

Short names for emulator executables, so a disk's `hail.cfg` can say which
emulator its image is for (`emulator = "SNES"`). Each key is a short name
(1-32 letters, digits, `-` or `_`, matched ignoring case) and each value is the
**full path** to that emulator's executable. A blank value leaves the entry
unset. The section is optional.

```toml
[emulators]
Snow = "C:\\Emulators\\Snow\\snow.exe"     # the [emulator] "snow" handler
SNES = "C:\\Emulators\\snes9x\\snes9x-x64.exe"
Genesis = ""                               # listed but not set up yet
```

On Linux use paths like `"/usr/bin/snes9x"`. In TOML strings, write Windows
backslashes as `\\`, or use single quotes: `SNES = 'C:\Emulators\snes9x.exe'`.

The standard `hail.toml` lists blank entries for the major systems, with Snow
first: Snow, NES, SNES, N64, GameCube, Wii, GB, GBC, GBA, NDS, MasterSystem,
Genesis, SegaCD, GameGear, Saturn, Dreamcast, PS1, PS2, PSP, Atari2600,
Atari7800, Lynx, Jaguar, PCEngine, NeoGeo, 3DO, ColecoVision, Intellivision,
Vectrex, MAME, AppleII, C64, Amiga, AtariST, MSX, ZXSpectrum, AmstradCPC and
DOS. Add any other names you need.

- A short name equal to an `[emulator]` type (`Snow`) means that handler; its
  path is recorded for when HAIL launches Snow itself.
- For any other emulator, `emulator.automount` **starts** its executable, by
  default with the image as its only argument, `"<executable>" "<image>"`, in
  the executable's folder (most emulators open a game or disk image given this
  way), in the logged-in user's desktop session (see `emulator_session` in
  [`[service]`](#service)). HAIL tracks the process: by default it is stopped
  when the disk is removed (see [Programs HAIL starts](#programs-hail-starts)).
- Each launch is logged as an `[emulator]` entry with the process ID, the
  command, the session, the disk, the image, the savefile and the disk's
  `hail.cfg`. If the executable cannot be started (for example, the path is
  wrong), an `[error]` entry says why and HAIL keeps running.
- A `hail.cfg` naming an emulator that is not listed, or listed with a blank
  path, mounts nothing; the reason is logged.

To start an emulator with other arguments, give a table with `path` and `args`
instead of a plain path:

```toml
[emulators]
SNES = { path = "C:\\RetroArch\\retroarch.exe", args = ["-L", "cores\\snes9x_libretro.dll", "--save={savefile}", "{image}"] }
```

| Placeholder | Replaced with |
|---|---|
| `{image}` | Full path of the image (required in at least one argument). |
| `{savefile}` | Full path of the disk's `hail.cfg` [`savefile`](#saving-guest-writes-savefile). An argument that uses it is **left out** when the disk has no savefile, so write options as one argument (`--save={savefile}`, not `"--save", "{savefile}"`). |

Use `{{` and `}}` for literal braces. Every emulator HAIL starts also gets the
environment variables `HAIL_IMAGE` and `HAIL_SAVEFILE` (empty without a
savefile), which wrapper scripts can use. `mode` flags are not passed on by
themselves; the log notes when they are not applied. A `writeback` mode with a
savefile that the emulator receives in its `args` is logged as applied.

### `[apis]`

HTTP or HTTPS services that `api.call` actions send requests to. Each
`[apis.<name>]` table (name: 1-32 letters, digits, `-` or `_`) has:

| Key | Meaning |
|---|---|
| `url` | Required. Base URL, `http://` or `https://`. An action's `path` is appended to it. |
| `timeout_ms` | Whole-request timeout. 100-300000, default 10000. |
| `headers` | Optional table of headers sent with every request (e.g. an API key). |
| `ca_file` | Optional full path of a PEM file with the CA certificate(s) that signed the server's certificate (for self-signed or private servers). When set, **only** these certificates are trusted for this API; without it the standard public CAs (built into HAIL) are trusted. |

```toml
[apis.frontend]
url = "http://127.0.0.1:8080/api"
timeout_ms = 3000
headers = { Authorization = "Bearer change-me" }

[apis.lab]
url = "https://lab.example:8443"
ca_file = "C:\\HAIL\\lab-ca.pem"
```

### `[rpc]`

JSON-RPC 2.0 listeners that `rpc.call` actions call. Each `[rpc.<name>]` table
has a `transport` and the keys for it:

| `transport` | Keys | Endpoint |
|---|---|---|
| `"http"` | `url` (required), `headers`, `ca_file` (as in `[apis]`) | Each request is an HTTP `POST` to `url`. |
| `"tcp"` | `address` (required, `host:port`) | A new TCP connection per request. |
| `"local"` | `path` (required) | Windows: a named pipe, `\\.\pipe\<name>`. Linux: the full path of a Unix socket. |

All transports also take `timeout_ms` (100-300000, default 5000), covering
connecting, sending and waiting for the response.

```toml
[rpc.snow]
transport = "local"
path = '\\.\pipe\snow-rpc'          # Linux: path = "/run/snow/rpc.sock"

[rpc.frontend]
transport = "tcp"
address = "127.0.0.1:7600"

[rpc.web]
transport = "http"
url = "http://127.0.0.1:8080/rpc"
```

See [Writing a JSON-RPC listener](#writing-a-json-rpc-listener) for exactly
what HAIL sends and expects.

### Events and placeholders

Each event can have an action chain, written as `[[events."<event>"]]` entries
that run **in the order listed**. Events without a chain are ignored.

| Event | When | Placeholders |
|---|---|---|
| `service.started` | HAIL has started | `{event} {timestamp} {version} {config} {mode} {emulator}` |
| `usb.inserted` | A USB volume appeared (and has been scanned) | `{event} {timestamp} {device} {label} {mount_point} {hail_cfg} {images}` |
| `usb.removed` | A USB volume disappeared | `{event} {timestamp} {device} {label} {mount_point}` |
| `polling.failed` | A scan or the OS event subscription failed | `{event} {timestamp} {error}` |

`{timestamp}` is UTC RFC 3339 (`2026-11-08T02:55:21Z`). `{mode}` is
`os-events` or `interval`. `{emulator}` is the `[emulator] type` or `none`. `{hail_cfg}` is `absent`, `found` or `invalid`.
`{images}` is the number of mountable images in the volume root. Write `{{` and
`}}` for literal braces.

### Actions

| Action | Parameters | Effect |
|---|---|---|
| `log.write` | `message = "<template>"` | Appends the message and a newline to the log. |
| `emulator.start` | none | Starts the `[emulator]`. Does nothing if it is already running. |
| `emulator.stop` | optional `emulator` | Without `emulator` (or with the `[emulator]` type, `"snow"`): stops the `[emulator]`; does nothing if it is not running. With a name: stops the emulators HAIL started under that name (`emulator.launch`, automount); `"*"` stops all of them. |
| `emulator.automount` | none (`usb.inserted` only) | Mounts what the disk's `hail.cfg` asks for (see below). |
| `emulator.launch` | `emulator`, optional `image`, `savefile`, `single`, `stop_on_remove`, `on_error` | Starts an `[emulators]` entry or an emulator defined in the action, and tracks it. See [Programs HAIL starts](#programs-hail-starts). |
| `process.run` (or `process_run`) | `command`, optional `args`, `name`, `session`, `wait`, `timeout_ms`, `single`, `stop_on_remove`, `on_error` | Starts any program, and tracks it or waits for it to finish. |
| `process.stop` (or `process_stop`) | `name` | Stops the programs `process.run` started under that name; `"*"` stops all of them. |
| `api.call` | `api`, optional `method`, `path`, `json` or `body`, `content_type`, `headers`, `on_error` | Sends an HTTP(S) request to an [`[apis]`](#apis) entry. See [`api.call`](#apicall). |
| `rpc.call` | `rpc`, `method`, optional `params`, `notify`, `on_error` | Calls a JSON-RPC 2.0 method on an [`[rpc]`](#rpc) listener. See [`rpc.call`](#rpccall). |
| `hail.quit` | none | Stops the chain and exits with code 0. |
| `hail.restart` | none | Stops the chain and exits with code 75. |

Text parameters are templates: they can use the event's placeholders (see the
table above). `process_run` and `process_stop` are accepted spellings of
`process.run` and `process.stop`; they behave identically and are logged under
the dotted names.

**Failures.** Every failure is logged as an `[error]` entry. What happens next
is set per action with `on_error` (for `emulator.launch`, `process.run`,
`api.call` and `rpc.call`):

| `on_error` | Effect |
|---|---|
| `"continue"` | Default. The chain goes on with the next action. |
| `"stop"` | The rest of this chain is skipped (logged as `[chain] <event> chain stopped: <action> failed (on_error = "stop")`). HAIL keeps running. |
| `"fail"` | HAIL stops with exit code 1, like a service that cannot work without the call. |

Other actions that fail (for example an unwritable log) stop HAIL with exit
code 1. An emulator from `[emulators]` that automount cannot start is only
logged (see above). After `hail.quit` or `hail.restart`, no further actions or
events are processed.

The `emulator.*` actions are currently stubs: they don't launch or control
Snow yet. Each call writes a `[debug]` entry to the log naming the emulator,
whether it was running, the event that triggered it, the Snow command it would
send (`rpc:` lines: the `EmulatorCommand` name and its parameters as JSON,
marked `not sent: stub`) and the result. Once the real handlers exist these
entries will appear only in debug builds.

```text
2026-11-08T03:20:00Z [debug] emulator.start (stub: not yet connected to Snow)
  emulator: snow running=false
  trigger: service.started
  rpc: Run {} (not sent: stub)
  result: started
2026-11-08T03:25:10Z [debug] emulator.stop (stub: not yet connected to Snow)
  emulator: snow running=true
  trigger: usb.removed
  rpc: Quit {} (not sent: stub)
  result: stopped
```

### Programs HAIL starts

`emulator.launch`, `process.run` and automount (for `[emulators]` and
`hail.cfg` custom emulators) start programs and **track** them: HAIL keeps
each one's name, process ID, session and the disk whose `usb.inserted` chain
started it, until it exits. Tracked programs can be stopped by
`emulator.stop`/`process.stop`, and are stopped automatically when their disk
is removed.

**`emulator.launch`** starts an emulator. `emulator` is either an
[`[emulators]`](#emulators) short name or a table in the same format as a
disk's [`hail.cfg`](#emulators-defined-by-the-disk-emulatorcustom), which
defines a new emulator right in the action (`path` may also be given per
platform, `path = { windows = { x64 = '...' }, linux = { arm64 = "..." } }`):

```toml
# An [emulators] entry, with an image from the inserted disk.
[[events."usb.inserted"]]
action = "emulator.launch"
emulator = "SNES"
image = "{mount_point}\\game.sfc"
savefile = "{mount_point}\\game.srm"

# An emulator defined here (it does not need an [emulators] entry).
[[events."service.started"]]
action = "emulator.launch"
emulator = { custom = true, name = "Frontend", path = "C:\\Frontend\\frontend.exe", args = ["--fullscreen"] }
single = true
```

| Key | Meaning |
|---|---|
| `emulator` | `"<short name>"` from `[emulators]` (not the `[emulator]` type: use `emulator.start` for Snow), `{ name = "<short name>" }`, or `{ custom = true, name = "...", path = "...", args = [...] }`. `path` must be a full path (or built from placeholders such as `{mount_point}`), or a table of such paths per platform (`path.<os>.<arch>`, as in `hail.cfg`; the action fails, logged, on a host with no entry); `args` default to `["{image}"]` and may use `{image}` and `{savefile}`. The name is what `emulator.stop` uses. |
| `image`, `savefile` | Optional full paths (placeholders allowed) passed as `{image}`/`{savefile}`. An argument that uses a placeholder with no value is left out. |
| `single` | Default `true`: if an emulator with this name is already running, it is not started again (logged). |
| `stop_on_remove` | `usb.inserted` chains only. Overrides `[service] stop_on_remove`. |
| `on_error` | See [Actions](#actions). |

Emulators defined in `hail.toml` are trusted like `[emulators]` entries:
`[service] custom_emulators` applies only to emulators a disk's `hail.cfg`
defines.

**`process.run`** starts any program (a script, a front end, a tool):

```toml
[[events."usb.inserted"]]
action = "process.run"
name = "backup"
command = "C:\\Tools\\copy-saves.exe"
args = ["{mount_point}", "--label={label}"]
wait = true
timeout_ms = 60000
on_error = "stop"
```

| Key | Meaning |
|---|---|
| `command` | Required. Full path of the program (placeholders allowed). Scripts need their interpreter: `command = "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe"`, `args = ["-File", "C:\\HAIL\\on-insert.ps1"]`, or `command = "/bin/sh"`, `args = ["/opt/hail/on-insert.sh"]`. |
| `args` | Optional array of argument templates. |
| `name` | Name for `process.stop` and the log. Default: the program's file name without extension. |
| `session` | `"user"` or `"service"`; default `[service] emulator_session`. |
| `wait` | Default `false`: start it, track it and go on. `true`: wait until it exits. Exit code 0 is success, any other code is a failure. |
| `timeout_ms` | `wait = true` only. 100-3600000, default 30000. A program still running then is killed and the action fails. |
| `single` | Without `wait`. Default `false`. `true`: not started again while one with this name runs. |
| `stop_on_remove` | Without `wait`, `usb.inserted` chains only. Overrides `[service] stop_on_remove`. |
| `on_error` | See [Actions](#actions). |

Programs started by `emulator.launch` and `process.run` run in their
executable's folder and get every placeholder of the event as an environment
variable `HAIL_<PLACEHOLDER>` (`HAIL_EVENT`, `HAIL_TIMESTAMP`,
`HAIL_MOUNT_POINT`, `HAIL_LABEL`, ...), plus `HAIL_IMAGE` and `HAIL_SAVEFILE`
(empty when not set).

**Stopping.** `emulator.stop` with a name, `process.stop`, and disk removal
stop a program in two steps:

1. HAIL asks it to close: on Windows it posts `WM_CLOSE` to the program's
   windows (as clicking their close button does; when HAIL runs as a service
   it does this through a short-lived `hail --close-windows <pid>` started in
   the user's session); on Linux it sends `SIGTERM`.
2. If the program has not exited after `[service] stop_timeout_ms`, or it has
   no window to close, HAIL kills it.

With `stop_on_remove` (the default), removing a disk stops the programs its
`usb.inserted` chain started **before** the `usb.removed` chain runs. Each stop
is logged:

```text
2026-11-08T06:10:02Z [emulator] emulator.launch SNES started (pid 4120)
  command: "C:\Emulators\snes9x\snes9x-x64.exe" "E:\game.sfc"
  session: user session 1 (cmyk)
  trigger: usb.inserted
2026-11-08T06:42:17Z [emulator] stopped SNES (pid 4120) (usb.removed E:\): WM_CLOSE posted to 1 window(s); exited after 140 ms (exit code 0)
2026-11-08T06:42:20Z [process] stopped tool (pid 5532) (process.stop in the usb.removed chain): WM_CLOSE posted to 1 window(s); still running after 5000 ms, killed (exit code 1)
```

Tracking lasts while HAIL runs, and survives configuration reloads.

**When HAIL exits** it stops every program it started that is still running,
the same way (close request, then kill after `stop_timeout_ms`), whatever the
reason: `hail.quit`, `hail.restart`, `--once`, an error, a Windows service
stop, Ctrl+C/Ctrl+Break or closing the console window, logoff/shutdown, or
`SIGINT`/`SIGTERM`/`SIGHUP` on Linux (which is what `systemctl stop` sends).
Each is logged with the reason:

```text
2026-11-08T07:00:03Z [emulator] stopped SNES (pid 4120) (hail exiting: hail.quit): WM_CLOSE posted to 1 window(s); exited after 210 ms (exit code 0)
2026-11-08T07:00:03Z [process] stopped tool (pid 5532) (hail exiting: stop request): no visible window to close; terminated; exited after 15 ms (exit code 1)
```

A second Ctrl+C (or signal) while HAIL is stopping ends it at once. Only a
forced kill (Task Manager's End task, `kill -9`) gives HAIL no chance to stop
its programs.

### `api.call`

Sends one HTTP request to an [`[apis]`](#apis) entry.

```toml
[[events."usb.inserted"]]
action = "api.call"
api = "frontend"
method = "POST"
path = "/disks"
json = { event = "{event}", label = "{label}", mount = "{mount_point}", images = "{images}" }

[[events."usb.removed"]]
action = "api.call"
api = "frontend"
method = "DELETE"
path = "/disks/{label}"
```

| Key | Meaning |
|---|---|
| `api` | Required. The `[apis]` entry. |
| `method` | `GET`, `POST`, `PUT`, `PATCH` or `DELETE`. Default `GET`, or `POST` with a body. |
| `path` | Optional, starts with `/` or `?`; appended to the API `url`. Placeholder values in it are URL-encoded (`{label}` = `My Disk` becomes `My%20Disk`). |
| `json` | A TOML table or array sent as JSON (`application/json`). Strings in it are templates; their values are JSON-escaped. Numbers and booleans stay as they are. |
| `body` | Text body (template), sent with `content_type` (default `text/plain; charset=utf-8`). Use `json` or `body`, not both. |
| `headers` | Optional table of extra headers (templates), added to the API's headers. |
| `on_error` | See [Actions](#actions). |

A status of 400 or more, a timeout or a connection failure is a failure. Each
call is logged with its status and duration, the request body and the start
of the response (up to 1000 characters):

```text
2026-11-08T05:47:20Z [api] api.call frontend POST http://127.0.0.1:8080/api/disks: 201 Created (2 ms)
  request: {"event":"usb.inserted","images":"2","label":"GAMES","mount":"E:\\"}
  response: {"ok":true}
```

### `rpc.call`

Calls a JSON-RPC 2.0 method on an [`[rpc]`](#rpc) listener.

```toml
[[events."usb.inserted"]]
action = "rpc.call"
rpc = "snow"
method = "disk.inserted"
params = { label = "{label}", mount_point = "{mount_point}" }

[[events."usb.removed"]]
action = "rpc.call"
rpc = "snow"
method = "disk.removed"
params = ["{mount_point}"]
notify = true
```

| Key | Meaning |
|---|---|
| `rpc` | Required. The `[rpc]` entry. |
| `method` | Required. Method name; names starting with `rpc.` are reserved by JSON-RPC. |
| `params` | Optional table (named parameters) or array (positional). Strings are templates. |
| `notify` | `true` sends a JSON-RPC notification: no `id`, no response expected. Default `false`. |
| `on_error` | See [Actions](#actions). |

A JSON-RPC `error` response, a response that does not match the request, a
timeout or a connection failure is a failure. The log shows the request and
the result (or error):

```text
2026-11-08T06:10:02Z [rpc] rpc.call snow disk.inserted (id 3) via local \\.\pipe\snow-rpc: result {"mounted":1} (4 ms)
  request: {"id":3,"jsonrpc":"2.0","method":"disk.inserted","params":{"label":"GAMES","mount_point":"E:\\"}}
```

### Writing a JSON-RPC listener

For authors of programs (such as a future Snow integration) that HAIL calls
with `rpc.call`. HAIL is the client; it sends standard
[JSON-RPC 2.0](https://www.jsonrpc.org/specification) requests:

```json
{"id":3,"jsonrpc":"2.0","method":"disk.inserted","params":{"label":"GAMES","mount_point":"E:\\"}}
```

- `id` is a positive integer that grows with every `rpc.call` HAIL makes (it
  starts at 1 when HAIL starts). Notifications (`notify = true`) have no `id`
  and get no response.
- `params` is an object or array, or absent. Key order is not significant.
- A successful reply is `{"jsonrpc":"2.0","id":<same id>,"result":<any JSON>}`;
  a failure is `{"jsonrpc":"2.0","id":<same id>,"error":{"code":<integer>,"message":"...","data":<optional>}}`.
  HAIL logs the result or the code, message and data.
- Batches are never sent.

Framing by transport:

| Transport | Listener |
|---|---|
| `tcp` | Accept a connection; read one request: a single line of UTF-8 JSON ending with `\n`. Write one response as a single line ending with `\n` (no newlines inside the JSON). HAIL opens a new connection for each request and closes it after the response (or right after sending a notification). |
| `local` (Windows) | A byte-mode named pipe (`\\.\pipe\<name>`) with the same line framing as `tcp`. Create the next pipe instance as soon as a client connects, so calls arriving close together are not refused (HAIL retries a busy pipe until its timeout); flush before disconnecting. When HAIL runs as a service (LocalSystem) and the listener runs as the desktop user, the pipe's security descriptor must allow SYSTEM to connect (the default does). |
| `local` (Linux) | A Unix stream socket at `path`, same line framing. HAIL (often root) must be able to open it. |
| `http` | Accept `POST` with `Content-Type: application/json`; the body is the request. Reply `200` with the JSON-RPC response as the body (`application/json`). For a notification any `2xx` with an empty body is fine. A non-2xx status is a failure. |

Responses larger than 1 MiB are rejected. A listener that does not answer
within the `[rpc]` entry's `timeout_ms` makes the call fail; HAIL does not
retry.

## Disks, images and `hail.cfg`

When a USB volume is inserted, HAIL lists its root folder (without writing
anything) and keeps a record of the disk until it is removed:

- **Disk images** in the root folder, in Snow's formats. Hidden files (names
  starting with `.`, including macOS `._*` files) are skipped.

  | Kind | Extensions |
  |---|---|
  | Floppy | `.moof .a2r .dc42 .dart .pfi .pri .woz .2mg .2img .po`, and `.img .dsk .image .raw` up to 2 MiB |
  | Hard disk | `.hda`, and `.img .dsk .image .raw` over 2 MiB |
  | CD-ROM | `.iso .toast`, and `.cue` sheets |

  A `.cue` sheet is read to find its track files (`FILE ... BINARY` or `WAVE`;
  track modes `AUDIO`, `MODE1/2352`, `MODE1/2048`, `MODE2/2352`). Every track
  file must exist on the same disk, otherwise the sheet is not offered for
  mounting. Track `.bin`/`.wav` files are not images by themselves.
- **`hail.cfg`** in the root folder (any letter case), which says whether and
  what to mount:

  | Key | Meaning |
  |---|---|
  | `automount` | `true` to allow automounting this disk. Default `false`. |
  | `emulator` | Optional. An `[emulators]` short name (e.g. `"SNES"`) to open the image with. Anything other than the `[emulator]` handler (`"snow"`) requires `image`. Default: the `[emulator]` handler. Can also be written as a table (`emulator.name = "SNES"`), which allows the keys below. |
  | `emulator.name` | The short name, same as `emulator = "..."`. Required in the table form. For a custom emulator it is only the name used in the log. |
  | `emulator.custom` | `true` makes the disk define the emulator itself with `emulator.path` and `emulator.args` instead of using an `[emulators]` entry. Requires `image`. Default `false`. See [Emulators defined by the disk](#emulators-defined-by-the-disk-emulatorcustom). |
  | `emulator.path` | Custom emulators only. A full path on the host (e.g. `'C:\Emulators\MyEmu\myemu.exe'`), or a path relative to the disk root for an emulator stored on the disk (e.g. `"Emulator/myemu.exe"`; no `..`). |
  | `emulator.path.<os>.<arch>` | Custom emulators only, instead of `emulator.path`: one path per host platform, so a disk can carry emulators for several. `<os>` is `windows` or `linux`, `<arch>` is `x64` or `arm64`; `emulator.path.<os>` covers every architecture of that OS. The entry for the host HAIL runs on is used (`<os>.<arch>`, then `<os>`, then on Windows on Arm with x64 emulation `windows.x64`); without one the disk is not mounted (logged). |
  | `emulator.args` | Custom emulators only. Arguments, with the same `{image}` and `{savefile}` placeholders as [`[emulators]`](#emulators) `args`. Default `["{image}"]`. |
  | `image` | Optional. Path of the one image to mount, relative to the disk root (`/` or `\`; it may be in a subfolder; no `..`). |
  | `select` | Used when `image` is not set: which root-folder images to mount (see below). Default `"first"`. Cannot be combined with `image`. |
  | `mode` | Optional. Comma-separated flags telling HAIL how to set up the emulator for each mounted image, e.g. `"readonly"` or `"readonly,nowriteback"`. Empty or missing means the emulator's defaults. |
  | `savefile` | Optional, only with `mode = "writeback"`. File where the emulator keeps guest writes, relative to the disk root like `image` (e.g. an SNES `.srm` battery save). It need not exist yet. Not allowed with `select = "all"`, and must differ from `image`. See [Saving guest writes](#saving-guest-writes-savefile). |

`emulator.automount` then mounts:

| `hail.cfg` | Mounted |
|---|---|
| missing, invalid, or `automount = false` | nothing |
| `image = "..."` | that image, if it exists and is a supported type |
| `emulator = "<name>"` and `image = "..."` | that file (any type) opened with the `[emulators]` executable `<name>` |
| `emulator.custom = true` and `image = "..."` | that file (any type) opened with `emulator.path`, if [`custom_emulators`](#service) permits it |
| `select = "first"` | the first image in the root folder, by name (ignoring case) |
| `select = "all"` | every image in the root folder |
| `select = "last-modified"` | the most recently modified image |
| `select = "oldest-created"` | the image with the oldest creation time (modification time if the filesystem does not record creation) |
| `select = "largest"` | the largest image |
| `select = "smallest"` | the smallest image |
| `select = "none"` | nothing |

Sizes count a `.cue` sheet together with its track files. When images tie, the
first by name wins. Images whose timestamp is unavailable are chosen last.

`mode` flags apply to every image mounted from the disk. HAIL never changes
the image on the USB disk unless guest writes are explicitly allowed to reach it:

| Flag | Floppy | Hard disk | CD-ROM |
|---|---|---|---|
| (none) | Snow default: writable in the guest, writes kept in memory | guest writes go to the image file | read-only |
| `readonly` | write-protected in the guest | Snow has no write-protected hard disks, so this is applied as `nowriteback` | read-only |
| `writeback` | guest writes are saved to the image file (if its format supports it) | same as the default | ignored |
| `nowriteback` | writes kept in memory (same as the default) | writes go to a temporary copy on the host; the image is not modified | read-only |

`writeback` cannot be combined with `readonly` or `nowriteback`. Unknown,
empty or repeated flags make the `hail.cfg` invalid (nothing is mounted).

### Saving guest writes (`savefile`)

With `mode = "writeback"`, a `hail.cfg` can name a `savefile`: a file on the
same disk where the emulator persists what the guest writes. Different
emulators keep these differently (SNES battery saves in `.srm` files, `.sav`
files, hard disk write copies). HAIL itself never writes it; the emulator
does. HAIL does not create folders either, so a savefile in a subfolder needs
that folder to exist on the disk (the log notes a missing folder).

| Emulator | What the savefile is used for |
|---|---|
| `[emulators]` entry | Passed in arguments that use `{savefile}` (see [`[emulators]`](#emulators)) and always in `HAIL_SAVEFILE`. |
| Snow, hard disk | First use: the image is copied to the savefile, and guest writes go there (`ScsiBranchHdd`). Later inserts: the savefile is attached directly and continues where it left off. The image itself is not modified. |
| Snow, floppy | Not used: Snow floppy writeback saves into the image file itself. |
| Snow, CD-ROM | Ignored: CD-ROM media is read-only. |

Each mount is written to the log as a `[debug]` entry with the disk, the image
(kind, size, data size including tracks, timestamps, CD-ROM tracks), the mount
mode, the Snow commands the mount maps to (`snow:` lines, and as the calls HAIL
sends: `rpc:` lines with the command name and JSON parameters, where
`"next-free"` lets Snow pick the drive or SCSI ID and `"same"` means the one the
previous command used), and the disk's `hail.cfg`. When nothing is mounted,
a `[debug]` entry gives the reason. While mounting is a stub these entries are
always written. Once real mounting exists they will appear only in debug builds.

```text
2026-11-08T02:55:21Z [debug] emulator.mount (stub: not yet connected to Snow)
  emulator: snow running=true
  disk: device=E:\ label=MACDISKS mount_point=E:\ inserted_at=2026-11-08T02:55:21Z
  image: path=E:\Game.cue kind=cd-rom size=67 bytes data=2419 bytes modified=2026-10-30T18:04:11Z created=2026-10-30T18:04:11Z
    file: BINARY E:\Game.bin (2352 bytes)
    track 01: MODE1/2352
  mode: readonly
  hail.cfg: E:\hail.cfg (automount=true emulator=- image=- select=last-modified mode=readonly savefile=-)
    | automount = true
    | select = "last-modified"
    | mode = "readonly"
  snow: ScsiAttachCdrom(id=next free)
  snow: ScsiLoadMedia(id, "E:\Game.cue")
  rpc: ScsiAttachCdrom {"id":"next-free"} (not sent: stub)
  rpc: ScsiLoadMedia {"id":"same","path":"E:\\Game.cue"} (not sent: stub)
  note: CD-ROM media is always read-only
2026-11-08T02:55:22Z [debug] emulator.automount F:\: nothing to mount (no hail.cfg on disk)
```

### Emulators defined by the disk (`emulator.custom`)

A disk can bring its own emulator instead of naming an
[`[emulators]`](#emulators) entry: one installed on the host that HAIL's
configuration does not list, or one stored on the disk itself.

```toml
# The emulator is on this disk, in the Emulator folder.
automount = true
image = "game.rom"
emulator.custom = true
emulator.name = "MyEmu"
emulator.path = "Emulator/myemu.exe"
emulator.args = ["--fullscreen", "{image}"]
```

One disk can carry the emulator for several kinds of host. Give
`emulator.path.<os>.<arch>` for each (`<os>`: `windows` or `linux`;
`<arch>`: `x64` or `arm64`, the Raspberry Pi 4/5 being `linux.arm64`), or
`emulator.path.<os>` for every architecture of that OS. HAIL uses the entry for
the host it runs on, looking for:

1. `<os>.<arch>`: a build for the machine itself (on Windows the real machine,
   even if an x64 build of HAIL runs there under emulation);
2. `<os>`: a build for any architecture;
3. on Windows on Arm (arm64) only, when Windows can run x64 programs (x64
   emulation, Windows 11): `windows.x64`. Linux has no such fallback.

So a disk that only carries `windows.x64` also works on a Windows 11 Arm PC,
while one with `windows.arm64` too gets the native build there.

```toml
# Windows and Linux builds of the emulator, all stored on this disk.
automount = true
image = "game.rom"
emulator.custom = true
emulator.name = "MyEmu"
emulator.path.windows.x64 = "Emulator/windows-x64/myemu.exe"
emulator.path.windows.arm64 = "Emulator/windows-arm64/myemu.exe"
emulator.path.linux.x64 = "Emulator/linux-x64/myemu"
emulator.path.linux.arm64 = "Emulator/linux-arm64/myemu"   # Raspberry Pi 4/5
emulator.args = ["--fullscreen", "{image}"]
```

Each entry is a path on the disk, or a full host path for its own OS (a
`windows` entry may be `'C:\Emulators\myemu.exe'`, a `linux` entry
`"/usr/bin/myemu"`), whatever the current host is. A disk with no entry for
the host is not mounted, and that is always logged:

```text
2026-11-08T05:12:40Z [emulator] emulator.launch MyEmu skipped: emulator.path: no entry for this host (linux.arm64; looked for linux.arm64, linux); defined for linux.x64, windows.x64
  disk: device=/dev/sda1 label=MYGAME mount_point=/media/pi/MYGAME inserted_at=2026-11-08T05:12:40Z
  hail.cfg: /media/pi/MYGAME/hail.cfg (...)
```

The same keys can be written as a table at the end of the file:

```toml
automount = true
image = "game.rom"

[emulator]
custom = true
name = "MyEmu"
path = 'C:\Emulators\MyEmu\myemu.exe'   # an emulator installed on the host
args = ["{image}"]
```

Running a program from a disk someone plugged in is a security decision, so
`custom_emulators` in [`[service]`](#service) says which custom emulators HAIL
may start:

| `custom_emulators` | Custom emulators started |
|---|---|
| `"none"` | None. |
| `"defined"` (default) | Only executables already listed in `[emulators]`. The disk can choose its own `args`, but cannot run anything new. |
| `"disk"` | Only executables stored on the inserted disk itself. |
| `"any"` | Any executable, on the host or on the disk. |

HAIL compares paths after resolving links and `.`/`..`, so a link on the disk
cannot point to an executable elsewhere. A launch is logged like an
`[emulators]` launch, with an extra line saying the emulator came from
`hail.cfg` and why it was permitted:

```text
2026-11-08T05:10:31Z [emulator] emulator.launch MyEmu started (pid 5528)
  emulator: custom, defined by hail.cfg (permitted by custom_emulators = "disk": executable is on the inserted disk)
  command: "E:\Emulator\myemu.exe" "--fullscreen" "E:\game.rom"
  session: user session 1 (ana)
  disk: device=E:\ label=MYGAME mount_point=E:\ inserted_at=2026-11-08T05:10:31Z
  image: path=E:\game.rom size=2048 bytes
  mode: default
  hail.cfg: E:\hail.cfg (automount=true emulator=MyEmu custom=disk:Emulator\myemu.exe args=2 image=game.rom select=first mode=default savefile=-)
    | ...
```

A refused emulator is not started and is logged:

```text
2026-11-08T05:11:02Z [emulator] emulator.launch MyEmu refused: custom_emulators = "defined" and E:\Emulator\myemu.exe is not an [emulators] executable
  executable: E:\Emulator\myemu.exe
  disk: device=E:\ label=MYGAME mount_point=E:\ inserted_at=2026-11-08T05:11:02Z
  hail.cfg: E:\hail.cfg (...)
```

Things to know:

- Custom emulators start in the session `emulator_session` selects, the same
  way as `[emulators]` entries. With the default `"user"`, a disk-provided
  emulator runs as the logged-in user. With `emulator_session = "service"`
  and HAIL running as a service, it would run as `SYSTEM` or `root`: do not
  combine that with `"disk"` or `"any"`.
- The emulator's working folder is the folder of its executable (on the disk
  for disk-provided emulators), so it can find files stored next to it.
- The executable must suit the host: a Windows `.exe` on Windows, a Linux
  program built for the host's CPU (x64 or arm64) on Linux. Use
  `emulator.path.<os>.<arch>` to carry one for each.
- On Linux the drive must be mounted without `noexec`, and FAT/exFAT drives
  without `showexec` (desktop automounting uses `showexec`, which only marks
  `.exe`, `.com` and `.bat` files executable). See the
  [Linux install guide](#linux-including-raspberry-pi-45).

### Example `hail.cfg` files

```toml
# Mount the first image (by name) in the drive's root folder.
automount = true
```

```toml
# Mount one specific image, which may be in a subfolder.
automount = true
image = "Systems/System 7.5.5.hda"
```

```toml
# Mount every floppy, hard disk and CD-ROM image in the root folder.
automount = true
select = "all"
```

```toml
# Mount the image changed most recently; the guest may not change it.
automount = true
select = "last-modified"
mode = "readonly"
```

```toml
# Mount the largest image (e.g. the system hard disk), letting the guest write
# to a throwaway copy so the image on the USB disk stays unchanged.
automount = true
select = "largest"
mode = "nowriteback"
```

```toml
# Mount the smallest image (e.g. a floppy) and save guest writes back to it.
automount = true
select = "smallest"
mode = "writeback"
```

```toml
# Keep this disk's images available but never mount them automatically.
automount = false
```

```toml
# Open a Super Nintendo game with the [emulators] entry "SNES".
automount = true
emulator = "SNES"
image = "Games/Chrono Trigger.sfc"
mode = "readonly"
```

Inserting that disk starts `snes9x-x64.exe "E:\Games\Chrono Trigger.sfc"`
and logs:

```text
2026-11-08T04:17:02Z [emulator] emulator.launch SNES started (pid 7412)
  command: "C:\Emulators\snes9x\snes9x-x64.exe" "E:\Games\Chrono Trigger.sfc"
  session: user session 1 (ana)
  disk: device=E:\ label=GAMES mount_point=E:\ inserted_at=2026-11-08T04:17:02Z
  image: path=E:\Games\Chrono Trigger.sfc size=4194304 bytes
  mode: readonly (not applied: SNES only receives its [emulators] args)
  hail.cfg: E:\hail.cfg (automount=true emulator=SNES image=Games/Chrono Trigger.sfc select=first mode=readonly savefile=-)
    | automount = true
    | emulator = "SNES"
    | image = "Games/Chrono Trigger.sfc"
    | mode = "readonly"
```

```toml
# Play a Super Nintendo game and keep its battery save (.srm) on the disk.
automount = true
emulator = "SNES"
image = "Games/Chrono Trigger.sfc"
mode = "writeback"
savefile = "Saves/Chrono Trigger.srm"
```

The SNES emulator learns the `.srm` location from its `[emulators]` arguments.
For example, RetroArch's `--save` option (documented as deprecated but still
supported) sets the save RAM file:

```toml
[emulators]
SNES = { path = "C:\\RetroArch\\retroarch.exe", args = ["-L", "cores\\snes9x_libretro.dll", "--save={savefile}", "{image}"] }
```

Inserting the disk (with a `Saves` folder on it) starts RetroArch on the
user's desktop and logs:

```text
2026-11-08T05:02:44Z [emulator] emulator.launch SNES started (pid 9120)
  command: "C:\RetroArch\retroarch.exe" "-L" "cores\snes9x_libretro.dll" "--save=E:\Saves\Chrono Trigger.srm" "E:\Games\Chrono Trigger.sfc"
  session: user session 1 (ana)
  disk: device=E:\ label=GAMES mount_point=E:\ inserted_at=2026-11-08T05:02:44Z
  image: path=E:\Games\Chrono Trigger.sfc size=4194304 bytes
  savefile: E:\Saves\Chrono Trigger.srm (new, passed in args and HAIL_SAVEFILE)
  mode: writeback (applied: SNES saves to the savefile)
  hail.cfg: E:\hail.cfg (automount=true emulator=SNES image=Games/Chrono Trigger.sfc select=first mode=writeback savefile=Saves/Chrono Trigger.srm)
    | automount = true
    | emulator = "SNES"
    | image = "Games/Chrono Trigger.sfc"
    | mode = "writeback"
    | savefile = "Saves/Chrono Trigger.srm"
```

The next time the disk is inserted, the line reads `(exists, ...)` and the
game continues from that save. Emulators that cannot be told a save path can
be started through a small wrapper script that reads `HAIL_SAVEFILE`.

```toml
# A Snow hard disk whose changes are kept in a separate file on the disk:
# the first insert copies the image there, later inserts continue from it.
automount = true
image = "Systems/System 7.5.5.hda"
mode = "writeback"
savefile = "Systems/System 7.5.5 (saved).hda"
```

## Example startup configurations

Minimal: log insertions and removals as soon as the OS reports them.

```toml
[service]
log_file = "hail.log.txt"

[polling]
type = "usb-volumes"
mode = "os-events"

[[events."usb.inserted"]]
action = "log.write"
message = "{timestamp} + {label} at {mount_point} ({images} images, hail.cfg {hail_cfg})"

[[events."usb.removed"]]
action = "log.write"
message = "{timestamp} - {label} from {mount_point}"
```

Automount with timed polling every 2 seconds and one log file per day:

```toml
[service]
log_file = "hail-{date}.log"

[polling]
type = "usb-volumes"
mode = "interval"
interval_ms = 2000

[emulator]
type = "snow"

[[events."service.started"]]
action = "log.write"
message = "{timestamp} HAIL {version} started ({mode}, {emulator})"

[[events."usb.inserted"]]
action = "log.write"
message = "{timestamp} {label} inserted"

[[events."usb.inserted"]]
action = "emulator.automount"
```

Run the emulator only while a disk is inserted: start it and mount the disk's
images on insertion, and stop it on removal:

```toml
[service]
log_file = "hail.log.txt"

[polling]
type = "usb-volumes"
mode = "os-events"

[emulator]
type = "snow"

[[events."usb.inserted"]]
action = "emulator.start"

[[events."usb.inserted"]]
action = "emulator.automount"

[[events."usb.removed"]]
action = "emulator.stop"
```

Restart HAIL whenever a disk is removed (for example so a supervisor can reset
the emulator session), and quit if detection stops working:

```toml
[service]
log_file = "hail.log.txt"

[polling]
type = "usb-volumes"
mode = "os-events"
settle_ms = 1000

[emulator]
type = "snow"

[[events."usb.inserted"]]
action = "emulator.automount"

[[events."usb.removed"]]
action = "log.write"
message = "{timestamp} {label} removed; restarting"

[[events."usb.removed"]]
action = "hail.restart"

[[events."polling.failed"]]
action = "log.write"
message = "{timestamp} detection failed: {error}"

[[events."polling.failed"]]
action = "hail.quit"
```

The packaged `hail.toml` is a complete, commented example that selects Snow,
logs every event and automounts inserted disks.

Launch a front end at startup, run a game while its disk is inserted (closed
again on removal), and tell other programs about each disk over HTTP and
JSON-RPC:

```toml
[service]
log_file = "hail-{date}.log"
stop_timeout_ms = 3000

[polling]
type = "usb-volumes"
mode = "os-events"

[emulators]
SNES = "C:\\Emulators\\snes9x\\snes9x-x64.exe"

[apis.frontend]
url = "http://127.0.0.1:8080/api"
timeout_ms = 2000

[rpc.snow]
transport = "local"
path = '\\.\pipe\snow-rpc'      # Linux: path = "/run/snow/rpc.sock"

[[events."service.started"]]
action = "emulator.launch"
emulator = { custom = true, name = "Frontend", path = "C:\\Frontend\\frontend.exe", args = ["--fullscreen"] }

[[events."usb.inserted"]]
action = "api.call"
api = "frontend"
path = "/disks"
json = { label = "{label}", mount_point = "{mount_point}", images = "{images}" }

[[events."usb.inserted"]]
action = "rpc.call"
rpc = "snow"
method = "disk.inserted"
params = { label = "{label}", mount_point = "{mount_point}" }
on_error = "stop"              # the game is not started if Snow did not answer

[[events."usb.inserted"]]
action = "emulator.launch"
emulator = "SNES"
image = "{mount_point}\\game.sfc"

[[events."usb.removed"]]
action = "api.call"
api = "frontend"
method = "DELETE"
path = "/disks/{label}"

[[events."usb.removed"]]
action = "rpc.call"
rpc = "snow"
method = "disk.removed"
params = ["{mount_point}"]
notify = true
```
