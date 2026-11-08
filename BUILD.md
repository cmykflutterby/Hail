# HAIL build workspace

Hardware Abstraction/Integration Layer - and how to build it

## Quick start

Use **PowerShell 7.4+ on x64 Windows**, with Git and Windows `tar.exe` on PATH.
No global Rust, GCC, Zig, Visual Studio, WSL or Docker installation is needed.

```powershell
.\build.ps1 -Init                       # first time: set up toolchain, linkers and vendored sources only
.\build.ps1 -Build                      # build and package Windows x64 Debug
.\build.ps1 -Test                       # build, package and test Windows x64 Debug
.\build.ps1 -Test -Platform All -Architecture All -Configuration All
.\build.ps1 -Build -Platform Linux -Architecture arm64 -Configuration Release
.\build.ps1 -Init -Build -Platform All -Configuration Release   # set up, then build
```

`-Init` only initializes and prepares for building; it never builds. Nothing is
ever built unless `-Build` or `-Test` is given.
It is idempotent. Initial setup downloads several hundred MB and requires
internet access. Subsequent builds use locked, vendored dependencies offline.
Do not run concurrent builds or empty `_local` while a build is running.

## GitHub Actions release

The manual **Build release** workflow runs on a Windows runner. It initializes
the pinned Rust toolchain, cross-compilers, Zig tools and Cargo crate vendor
directory, then builds all Windows/Linux x64/arm64 Release packages. The CI
initializer intentionally does **not** clone `vendor/snow`: that checkout is a
pinned code reference and provenance input for the local initializer, not a
compile dependency.

Trigger it from the Actions tab with a release tag matching the product
version, such as `v<version>`. The workflow uploads the packages as a workflow
artifact and creates a draft GitHub release for human review; it does not
publish the release automatically.

The authoritative product version is the workspace `version` in
[`Cargo.toml`](Cargo.toml). The prominent `VERSION` constant in
[`source/hail/src/version.rs`](source/hail/src/version.rs) exposes
`env!("CARGO_PKG_VERSION")`, so package names, runtime `--version` output,
service events, tests, and release tags all use the same Cargo value. Update
the Cargo workspace version when preparing a release; no build step mutates
the manifest or lock file.

## Layout and ownership

| Path | Purpose | Git |
|---|---|---|
| `source/` | HAIL application source and its standard config only (no scripts, tests or build files) | tracked |
| `tests/` | Standalone executable test crates and fixtures we own | tracked |
| `build/` | Child scripts, common implementation, dependency/tool configuration | tracked |
| `build.ps1` | Public entry point and family dispatcher | tracked |
| `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml` | Workspace, exact dependency graph, compiler pin | tracked |
| `.cargo/` | Approved metadata-only exception: isolate IDE/direct Cargo builds and use vendored dependencies | tracked |
| `BUILD.md` | Build contract | tracked |
| `README.md` | HAIL usage guide (shipped in every package) | tracked |
| `design-docs/` | Original design/proof of concept; existing ignore rule preserved | ignored |
| `vendor/` | Generated Cargo dependency sources and pinned external checkouts | ignored |
| `drops/` | Final ZIP/tar.gz packages and SHA-256 sidecars for human review | ignored |
| `_local/` | Disposable toolchains, downloads, Cargo/Rustup homes, target artifacts, linker wrappers, staging, caches, test scratch space, logs | ignored |

Never put owned code in `vendor`, build artifacts in `source`, or scratch code
outside `_local`. Never use Cargo's default root `target/`; scripts redirect it.
The persistent `.cargo/config.toml` also redirects the IDE/direct Cargo compiler,
linkers, dependency sources, temporary files and build outputs to this workspace.
Initialize first. Use `build.ps1` for pinned Cargo and complete packaging;
direct Cargo commands may use whichever Cargo executable your PATH selects.
All test code belongs in top-level `tests/`, not `#[test]` modules in app crates.
Test crates use ordinary `main` binaries, not Cargo's test harness; app crates
disable harness/doctest targets. Build scripts compile both app and test binaries.
Tests print PASS/FAIL and exit nonzero on failure; build-driven outputs are
captured in `_local/logs/`. Test scratch directories are unique per run.

`-Init` creates missing directories without clearing any existing contents.
It never recreates or overwrites `source/`. Missing tracked `build/` or `tests/`
directories can be restored from the Git index by the root script; restore
individual deleted tracked files from Git before initialization.
Deleting `_local`, `vendor`, or `drops` is safe between builds: rerun `-Init`
(or use `-Clean`, below).
Git metadata and tracked sources/configuration are persistent, not disposable.
Initialization does not commit changes or configure a remote.

## Standard flags and flow

| Flag | Values | Default |
|---|---|---|
| `-Init` | switch: initialize/prepare only | off |
| `-Build` | switch: build and package the selection | off |
| `-Platform` | `Windows`, `Linux`, `All` | Windows |
| `-Architecture` | `x64`, `intel64`, `amd64`, `arm64`, `All` | x64 |
| `-Configuration` | `Debug`, `Release`, `All` | Debug |
| `-Test` | switch: build, package and run the standalone tests (implies `-Build`) | off |
| `-Clean` | one or more of `Drops`, `Build`, `Cache`, `Local`, `Vendor`, `All` (comma-separated) | none |
| `-WhatIf` | switch; previews `-Clean` without deleting anything | off |

Intel64 and AMD64 are aliases for **one** x86_64 build, not distinct binaries.
`All` expands to Windows/Linux, x64/ARM64, and/or Debug/Release respectively.

**When a build runs.** `-Clean`, `-Init`, `-Build` and `-Test` are *action*
flags and run in that order. Only `-Build` and `-Test` build. `-Platform`,
`-Architecture` and `-Configuration` are *selection* flags: they choose what
`-Build`/`-Test` build and never start a build themselves.

| Invocation | Result |
|---|---|
| `.\build.ps1` (no flags) | Print a usage summary; nothing is done (exit 0) |
| `.\build.ps1 -Init` | Initialize and verify the toolchain only; nothing is built |
| `.\build.ps1 -Clean <modes>` | Clean only |
| `.\build.ps1 -Clean <modes> -Init` | Clean, then initialize; nothing is built |
| `.\build.ps1 -Build` | Build and package the default selection: Windows x64 Debug |
| `.\build.ps1 -Test` | As `-Build`, then run the standalone tests |
| any action plus `-Build`/`-Test` | ...then build the selection (unspecified selection flags use their defaults) |
| a selection flag without `-Build`/`-Test` (e.g. `-Init -Platform All`) | Error before anything runs (exit 1) |

So `.\build.ps1 -Init -Test` initializes, then builds and tests Windows x64
Debug, and `.\build.ps1 -Init -Build -Platform All` builds every Debug package.
The child scripts accept the identical flag names and values and apply the
same rules. Each defaults `-Platform` to its own family (`windows.ps1` to
Windows, `linux.ps1` to Linux) and refuses another family. They can be invoked
directly, including `-Init` alone. The root initializes once, then dispatches
requested families with `-Build` and explicit selection flags; children compile,
validate, and package.

Flow:

0. Validate flags: selection flags without `-Build`/`-Test` are an error; no
   action flag at all prints usage and stops.
1. With `-Clean`: remove the selected modes (see Cleaning); stop unless `-Init`,
   `-Build` or `-Test` was also given.
2. Establish `_local` paths and isolate environment/caches.
3. With `-Init`: initialize Git if absent; install local Rust and cross-linkers;
   fetch/check out external source revisions declared in `build/project.json`;
   populate `vendor/crates` from `Cargo.lock`; generate local Cargo source config.
4. Check initialization and exact Cargo version. Stop here, without building,
   unless `-Build` or `-Test` was given.
5. Build workspace binaries with
   `--locked --offline`, the selected target, and Debug or Release profile.
6. With `-Test`, copy the app, test binary and default config into
   `_local/test-bundles/<package-name>/`, then run the standalone tests on
   Windows x64 and log results. Foreign-target tests are compiled and bundled
   but reported as **not executed**; copy the bundle to the target to run them.
7. Stage under `_local/packages`, package into `drops`, and emit SHA-256 sidecars.
   Build provenance (compiler/linker identities, target, test status, Snow
   revision, file hashes, project revision or `uncommitted`) is written to
   `_local/logs/package-<package-name>.json`, not into the package.
   Compilation/test failures stop the build with a nonzero exit code.

Release uses fat LTO and one codegen unit, matching Snow's release LTO policy.

### Cleaning (`-Clean`)

```powershell
.\build.ps1 -Clean All -WhatIf        # preview what would be removed and its size
.\build.ps1 -Clean Build              # clean only, no build
.\build.ps1 -Clean Cache,Drops -Test  # clean, then build, package and test Windows x64 Debug
.\build.ps1 -Clean All                # source-only workspace (portable/archivable)
.\build.ps1 -Clean All -Init          # wipe everything, then fully re-saturate (no build)
.\build.ps1 -Clean All -Init -Build   # wipe, re-saturate, then build Windows x64 Debug
```

| Mode | Removes | Next build needs `-Init`? |
|---|---|---|
| `Drops` | Contents of `drops/` | no |
| `Build` | `_local/target`, `packages`, `package-validation`, `test-bundles`, `test-runs`, `tmp`, and any stray root `target/` | no |
| `Cache` | `Build`, plus `_local/downloads`, `logs`, Zig/zigbuild caches, and Cargo `registry`/`git` caches. Keeps the toolchain, linkers, tools and `vendor/` | no |
| `Local` | Everything in `_local/` (toolchain and tools included) and root `target/` | yes |
| `Vendor` | Everything in `vendor/` (crate sources and the Snow checkout) | yes |
| `All` | `Local` + `Vendor` + `Drops` | yes |

Rules:

- Modes can be combined (`-Clean Build,Drops`). Cleaning always runs first.
- `-Clean` alone only cleans. With `-Init` it then initializes; it builds only if
  `-Build` or `-Test` is also given.
  Child scripts follow the same rule; the root does not forward `-Clean` to them.
- `vendor/`, `drops/` and `_local/` are left as empty folders.
- `source/`, `tests/`, `build/`, `.cargo/`, `.git/`, `design-docs/` and the
  tracked root files are never touched; the clean refuses any path outside the
  workspace or inside these folders.
- Locked files (IDE/rust-analyzer, a running hail or test) are reported
  together at the end with a nonzero exit; close them and rerun.
- No clean log is written, so `-Clean All` really leaves `_local/` empty.

After `-Clean All`, the workspace holds only tracked source and configuration
(well under 1 MB plus `.git`), suitable for a source-only archive: `source/`,
`tests/`, the build system (`build.ps1`, `build/`, `.cargo/`, `Cargo.toml`,
`Cargo.lock`, `rust-toolchain.toml`) and empty `_local/`, `vendor/`, `drops/`.
The build system is kept because it is what makes the archive rebuildable with
`.\build.ps1 -Init`. `.git/` (history) and `design-docs/` (user-owned, ignored)
are kept because they cannot be regenerated; exclude them when archiving, or
use `git archive`, which includes only tracked files.

### Packages in `drops/`

Names are `hail-<version>-<os>-<arch>-<configuration>` with `.zip` for Windows
and `.tar.gz` for Linux, plus a `.sha256` sidecar:

| Package | Runs on |
|---|---|
| `hail-<version>-windows-x64-<cfg>.zip` | 64-bit Intel/AMD Windows |
| `hail-<version>-windows-arm64-<cfg>.zip` | ARM64 Windows |
| `hail-<version>-linux-x64-<cfg>.tar.gz` | 64-bit Intel/AMD Linux, glibc 2.28+ |
| `hail-<version>-linux-arm64-<cfg>.tar.gz` | 64-bit ARM Linux, including 64-bit Raspberry Pi OS on Pi 4/5 |

Each archive holds one `hail-<...>/` folder containing **only** the release
payload: `hail` (`hail.exe` on Windows; mode 0755 in tar archives), the
standard `hail.toml`, and the root `README.md` (HAIL usage guide).
Installer or service-registration scripts may be added in a later phase.
Test binaries, build docs, license texts and build metadata are never packaged.
The package payload is defined by `package` in `build/project.json`.

## Toolchain compatibility and external sources

Snow reference: <https://github.com/twvd/snow>, pinned to
`ce1b3c6baf7f0c9d01c6a8b71928156c7511e7bc`.
Its `rust-toolchain.toml` requires **Rust 1.95.0**; HAIL uses that exact official
compiler/Cargo release. Initialization verifies the checked-out Snow pin agrees.
Changing the pin requires updating `rust-toolchain.toml`, `build/project.json`,
the pinned compiler paths in `.cargo/config.toml`, and
checking upstream requirements again.

| Target | Compiler/linker used here |
|---|---|
| Windows x64 | Rust 1.95.0, portable WinLibs MinGW GCC 16.2.0 (MSVCRT, POSIX/SEH) |
| Windows ARM64 | Rust 1.95.0, LLVM-MinGW 20261104 Clang/LLD |
| Linux x64 | Rust 1.95.0, Zig 0.15.2 Clang/LLD, GNU ABI / glibc 2.28 baseline |
| Linux ARM64 | Same, AArch64 GNU ABI; suitable for **64-bit** Raspberry Pi OS on Pi 4/5 |

Windows uses static CRT flags, as Snow's Windows workflow does. Snow's Windows
CI actually installs unpinned MSYS2 Rust and GCC/LLVM-MinGW; its Linux CI uses
native distribution linkers. Therefore exact upstream linker versions cannot
be claimed. This project's Windows linker families match upstream; the Linux
Zig cross-linker is an explicitly approved deviation enabling builds on Windows.
This does not install or build Snow's GUI/system dependencies.
Linux linker wrappers invoke pinned cargo-zigbuild 0.24.1's `zig cc` adapter to
translate Rust linker arguments and Windows response-file quoting correctly.
Generated wrappers use `.cmd` rather than `.bat` to avoid Rust's extra batch
launcher quoting issue in paths with spaces. Its cache is also under `_local`.
For GCC 16's unquoted default-manifest specs expansion in paths with spaces,
the build generates a local endfile specs override using the linker's equivalent
`-l:default-manifest.o` lookup. The same manifest object is retained; installed
tool sources are not edited.

Downloaded GCC, LLVM-MinGW, Zig and cargo-zigbuild archives are SHA-256 pinned in project config.
Rustup bootstrap uses the official HTTPS download and published checksum;
the Rust compiler version is pinned. Toolchains and installers remain `_local`.
External repository URL, commit and submodule policy are explicit configuration.
`vendor/snow` is a source reference, not an owned workspace member and not yet
linked into HAIL. Snow's CPU instruction test-fixture submodules are excluded:
they are not required to compile this POC. Configured required submodules can be
enabled with `submodules: true`. Cargo's full transitive dependency graph is
vendored with versioned directories; do not edit generated vendor sources.
Uncommitted changes in external checkouts are not overwritten to change pins.

## HAIL service (`source/hail`)

`source/` contains only the HAIL application: Rust source plus its standard
configuration `source/hail/config/hail.toml`. **How to run and configure HAIL**
(command line, exit codes, `hail.toml`, events, actions, disk images,
`hail.cfg`, automount, examples) is documented in the root
[`README.md`](README.md), which is shipped in every package. Keep it in sync
with code changes. The notes below cover only implementation details that
affect building, testing and porting.

| Module | Responsibility |
|---|---|
| `config.rs` | `hail.toml` parsing/validation; `[emulator] type`; `[emulators]` short-name registry; `[service] emulator_session`, `custom_emulators`, `stop_on_remove`, `stop_timeout_ms`; actions `log.write`, `emulator.start`, `emulator.stop` (handler, or tracked emulators by name / `*`), `emulator.automount`, `emulator.launch` (`LaunchTarget`: `[emulators]` name or a hail.cfg-style `{ custom, name, path, args }` table; `path` may be per platform, `platform_path_template` checks absolute paths for the platform's OS), `process.run`, `process.stop` (serde aliases `process_run`, `process_stop`), `api.call`, `rpc.call`, `hail.quit`, `hail.restart`; per-action `on_error` (`OnError`) |
| `service.rs` | Event dispatch, action chains (`on_error` continue/stop/fail), tracked-disk registry, the `ProcessTable` of launched programs (reaped before each chain and between waits; disk-owned entries stopped on `usb.removed` before its chain), `emulator.launch`/`process.run` (`tracked_request`: executable's folder, `HAIL_<FIELD>` environment), exit requests (codes 0 / 1 / 75), stop handle (`Exit::Stop`), config file watch/reload (`watch_config`, `check_config`: validate, then swap; rejected files keep the running config; the process table survives reloads), `run()`, which switches between the interval and os-events loops when a reload changes `[polling] mode`, and `shutdown()`, which stops every tracked program when hail exits (`hail exiting: <reason>`) |
| `platform.rs` | Host platform names (`windows`/`linux`, `x64`/`arm64`; on Windows `host_arch` is the native machine from `IsWow64Process2`, and `host_runs_x64` asks `IsWow64GuestMachineSupported` whether an Arm host runs x64 programs, `Win32_System_SystemInformation`), `is_absolute_on` (absolute-path rules per OS, so a Windows host can validate Linux paths and vice versa) and `PerPlatform<T>`: a value given once or per `<os>` / `<os>.<arch>` (`emulator.path.<os>.<arch>` in hail.cfg, `path = { <os> = { <arch> = ... } }` in actions); `for_host()`/`for_platform()` pick the first defined of `candidates()`: `os.arch`, `os`, then `os.x64` on an Arm host with x64 emulation |
| `process.rs` | `ProcessTable`/`TrackedProcess`: programs hail started (kind, name, pid, session, owning disk, `stop_on_remove`), reaping, and two-step stops (graceful close, wait up to `stop_timeout_ms`, kill) with one log line per stop; `stop()` keeps stopping the rest if logging fails |
| `remote.rs` | `[apis]` + `api.call` (`ureq` 3 with `rustls`/`ring` and bundled `webpki-roots`; `ca_file` replaces the roots), `[rpc]` + `rpc.call` (JSON-RPC 2.0 over HTTP POST, TCP, Windows named pipe / Unix socket; newline-delimited framing), `JsonTemplate` (TOML values whose strings are templates, JSON-escaped on render), `percent_encode`, `rpc_request`/`rpc_response` |
| `winservice.rs` | Windows only (`windows-sys` `Win32_System_Services`): SCM dispatcher and control handler (Stop/Shutdown set the stop flag), status reporting (non-zero exit codes reported as service-specific errors), `--install` (auto-start, LocalSystem, recovery actions with `fFailureActionsOnNonCrashFailures`) and `--uninstall` |
| `main.rs` | CLI; `serve()` runs the service for both console and `--service` modes and calls `Service::shutdown` on every exit path (quit, restart, stop, error, `--once`). `mod signals` turns Ctrl+C/Ctrl+Break/console close/logoff/shutdown (`SetConsoleCtrlHandler`, `Win32_System_Console`) and SIGINT/SIGTERM/SIGHUP (`libc::signal`) into a stop request; a second one ends hail at once. Config reload is opt-in and Windows-only: `--watch-config` (rejected on Linux and with `--once`/`--check-config`/`--uninstall`) calls `watch_config`; `--install --watch-config` adds it to the registered service command (`winservice::service_command`). Hidden Windows helper `--close-windows <pid>` (exit code = windows closed), started in the user's session when a tracked program there must be closed from session 0 |
| `media.rs` | Read-only volume-root scan, image classification (floppy/hard disk/CD-ROM, `.cue` sheets), `hail.cfg` (`select` strategies, `mode` flags, `savefile`, `emulator` as a short name or a `{ custom, name, path, args }` table with host or disk-relative `CustomPath`, optionally per platform as `PerPlatform<CustomPath>`), mount plan (`MountPlan::NoHostEmulator` when the disk has no emulator for this host) |
| `emulator/` | `Emulator` handler trait (`start`, `stop`, `mount`, running state) and `Kind` (`[emulator] type`) mapping each emulator to its handler. `snow.rs` is the Snow handler (`SnowMount` maps an image and its `mode` to Snow emulator commands, and `SnowRpc` to the calls hail will send: Snow `EmulatorCommand` names with JSON params, logged as `rpc:` lines marked `not sent: stub`; start/stop log `rpc: Run {}`/`rpc: Quit {}`), currently a stub that writes `[debug]` log entries (`snow::IS_STUB`); once real, these entries are written only in debug builds. A new emulator needs a `Kind` variant and a handler; action names and config stay the same. `[emulators]` names other than a `Kind` are external emulators: `MountPlan::External` and `launch_external` (renders the entry's `args` templates, `{image}`/`{savefile}`, default `"{image}"`; sets `HAIL_IMAGE`/`HAIL_SAVEFILE`; logs an `[emulator]` or `[error]` entry). `launch.rs` starts the process per `[service] emulator_session`: on Windows from session 0 via `WTSQueryUserToken` + `CreateEnvironmentBlock` + `CreateProcessAsUserW` on `winsta0\default` (`windows-sys` `Win32_System_RemoteDesktop`, `Win32_System_Environment`, `Win32_System_Threading`); on Linux as root by picking the active graphical systemd-logind session and dropping to that user (`setgroups`/`setgid`/`setuid` in `pre_exec`); otherwise in hail's own session. `launch.rs` also owns the `Process` handle (`try_exit`, `request_close`: `WM_CLOSE` to the pid's top-level windows, via `--close-windows` across sessions, or `SIGTERM`; `kill`). `CustomEmulators` (`[service] custom_emulators`: none/defined/disk/any) checks hail.cfg custom executables against canonicalized `[emulators]` paths or the disk root before `launch_external`; refusals are logged by `refuse_custom`. Pure helpers (Windows argument quoting, environment blocks, logind session parsing/picking) are public so tests can check them on any host |
| `discovery/`, `watch/` | USB volume discovery (per OS, `lsblk` parsing) and OS event subscription (per OS, uevent filter) |
| `log.rs`, `event.rs`, `template.rs` | Host-only log file (optional, time-token file names that roll over, local/UTC clock), event fields, calendar maths and template rendering |

Image classification follows Snow at the pinned revision: floppy loader
extensions (`floppy/src/loaders/auto.rs`), hard disks `.hda`/`.img`, CD-ROM
`.iso`/`.toast`/`.cue` with BINARY/WAVE tracks (`core/src/mac/scsi/cdrom`).
Generic `.img`/`.dsk`/`.image`/`.raw` files are treated as floppies up to
2 MiB and as hard disks above that. Recheck these lists when the Snow pin
changes.

Detection modes (`[polling] mode`):

- **`interval`**: scan every `interval_ms`.
- **`os-events`**: sleep until the OS reports a hardware/mount change, then
  rescan 3 times, `settle_ms` apart. Detection is always a diff of volume
  snapshots, so duplicate notifications never produce duplicate events.
  - **Windows**: `CM_Register_Notification` for `GUID_DEVINTERFACE_VOLUME`
    arrival/removal. Needs no window or message loop, so it works when run as
    a Windows service (session 0).
  - **Linux/Raspberry Pi**: `poll()` on `/proc/self/mountinfo` plus the kernel
    `NETLINK_KOBJECT_UEVENT` group filtered to `SUBSYSTEM=block` (via `libc`).
    Both work without root. If netlink is unavailable HAIL warns and continues
    with mountinfo only.

Discovery:

- **Windows**: native Win32 APIs (`GetLogicalDrives`, `GetVolumeInformationW`,
  and `IOCTL_STORAGE_QUERY_PROPERTY` with zero access rights) select volumes
  whose storage bus is USB, including USB hard disks.
- **Linux/Raspberry Pi**: the `usb-disk-probe` 0.2.0 crate plus `lsblk` JSON
  (util-linux 2.37+ for `MOUNTPOINTS`). Partitions inherit `TRAN=usb`, which also
  covers the Pi's SoC USB controller. The OS must mount the filesystem first.

**Host-only log / read-only USB policy:** HAIL never logs onto, or opens for
writing, a detected USB volume; disk scans only list and read files.

The original sketch `design-docs/proof-of-concept/diskmon-basic.md` referenced
nonexistent `usb-disk-probe` 0.5 APIs and wrote logs onto the drive; HAIL
replaces it with the behaviour above. `windows-sys` is held at 0.59 because
0.60+ uses `raw-dylib`, whose GNU `dlltool` step fails in paths with spaces.

HTTPS uses `rustls` with the `ring` provider (no OpenSSL, no system TLS
library), so every target cross-compiles from Windows. `ring` builds C and
assembly with the `cc` crate; `build/Common.psm1` therefore sets
`CC_<target>`/`AR_<target>` to the same pinned toolchains used as linkers (GCC for Windows x64, LLVM-MinGW Clang for Windows
ARM64, `zig cc` wrappers for Linux), so `ring` uses the same toolchains as the
rest of the build.

### Running tests on a target device

Copy `_local/test-bundles/<package-name>/` to the device, then:

```powershell
$env:HAIL_APP = (Resolve-Path .\hail.exe).Path
$env:HAIL_DEFAULT_CONFIG = (Resolve-Path .\hail.toml).Path
$env:HAIL_TEST_ROOT = "$env:TEMP\hail-test-$(New-Guid)"
.\hail-tests.exe
```

```sh
HAIL_APP="$PWD/hail" HAIL_DEFAULT_CONFIG="$PWD/hail.toml" \
HAIL_TEST_ROOT="$(mktemp -d)/run" ./hail-tests
```

## Validation coverage

`tests/hail-tests` checks snapshot insert/remove/remount/no-change, lsblk
parsing (Pi USB inheritance, probe matches, malformed input), default config
loading, rejection of unknown actions/events/fields/placeholders/polling types,
missing/unknown modes, mode-specific `interval_ms`/`settle_ms` rules and
ranges, path-like log names, template rendering, timestamps, ordered
`log.write` chains, `polling.failed` continuation, `hail.quit`/`hail.restart`
(chain stop, no further dispatch, exit codes 0/75 from the executable),
`emulator.automount` placement rules, `[emulator]` type selection and
validation (optional section, required by any `emulator.*` action, unknown
types/keys rejected), `emulator.start`/`emulator.stop` running-state
transitions, `[emulators]` parsing (default list with Snow first, blank
entries, relative paths, invalid and case-duplicate names), `hail.cfg`
`emulator` rules and external launches (a fake emulator, a copy of the test
binary in a folder with a space, records that it was started with the image
in its own folder; a missing executable is logged as `[error]`; blank,
unlisted, missing image, Snow by name), config reload (valid edit applied with
new log file, invalid edit rejected and logged once per edit, polling-mode
switch inside `run()`), the stop handle, `--service` outside the service
manager and invalid flag combinations (including `--watch-config`), no reload without `--watch-config` and a live reload with it against the running
executable (Windows), no-op repeats and stub log entries, image classification (floppy, hard disk,
CD-ROM `.iso`/`.toast`/`.cue`), cue sheet parsing and missing-track rejection,
`hail.cfg` parsing/validation (every `select` value, `mode` flags and their
conflicts, `savefile` rules: writeback only, not with `select = "all"`, on the
disk, differs from `image`), Snow savefile mapping (hard disk branch on first
use / resume from an existing savefile, floppy and CD-ROM notes), `[emulators]`
table form (`args` templates, `{savefile}` arguments dropped without a
savefile, `{image}` required, unknown keys/placeholders rejected),
`emulator_session` parsing, Windows argument quoting and environment-block
building/merging, logind session parsing and picking (root, greeter, inactive
and tty sessions skipped; seated sessions preferred), a savefile launch whose
fake emulator records its arguments and `HAIL_IMAGE`/`HAIL_SAVEFILE`,
hail.cfg custom emulators (table form, host and disk paths, args, every
rejection), `custom_emulators` parsing and checks for each policy (resolved
paths, `.` components, missing executables), launches of a disk-provided and
a host custom emulator with their own args, and refusals under `"defined"`
and `"none"`, per-platform `emulator.path.<os>.<arch>` (parsing, unknown
OS/arch and per-OS absolute-path rejections, host selection with an OS-only
fallback, the Windows on Arm `windows.x64` fallback via `for_platform`/`candidates` for any host, a launch from a multi-platform disk, the skip log for a disk without
an emulator for this host), process tracking (`calls.rs`, using a `fake-process` copy of the
test binary with modes `window [--ignore-close]` (Windows: an off-screen,
never-activated window), `sleep` and `exit <code>`, which records its
arguments and `HAIL_*` environment): `emulator.launch` from `[emulators]` and
custom tables, `process.run` detached / `wait` / timeout kill / non-zero exit,
`single`, `emulator.stop`/`process.stop` by name and `*` (graceful close,
kill after `stop_timeout_ms`, terminate without a window), reaping,
`on_error` continue/stop/fail, stop on `usb.removed` (including automount
launches and `stop_on_remove = false`), per-platform action paths (and a
logged failure on a host with no entry), `Service::shutdown` stopping every
tracked program (and nothing on a second call), the executable stopping its
programs on `hail.quit` and `--once`, and every parse rejection; `api.call`
and `rpc.call` against in-process mock servers (HTTP, HTTPS with the
`tests/hail-tests/fixtures/tls` CA/server certificates (100-year validity,
regenerate with openssl if ever needed), JSON-RPC over TCP, HTTP and a named
pipe / Unix socket): methods, JSON/text bodies, headers, percent-encoding,
HTTP errors, timeouts, refused connections, trusted/untrusted TLS, results,
JSON-RPC errors, id mismatches, notifications and parse rejections,
image selection (ties, missing timestamps, creation-time fallback,
cue data sizes), Snow command mapping for each media kind and mode (and matching `rpc:` calls, logged for mounts, start and stop), read-only
volume scans, mount plans for every automount/select/image case, disk tracking on insert/remove, the stub mount
debug log, log append, `logging = false`, time-token log names (every
token, ISO week-year edges, local offset sanity) and hour/day rollover
including the temporary fallback, temporary
fallback, refusal to log onto a USB volume, the os-events settle schedule,
kernel uevent filtering, the event-driven loop (scripted signals: scans only
after signals, exact rescan count, subscription failure raises `polling.failed`),
a live native OS event subscription, and the executable's CLI,
`--check-config`, native `--once` polling and run-until-killed in both modes. It writes only to its scratch
directory. Physical USB attach/detach, the Linux event watcher and process
stops at runtime, closing a program in a user session from a session-0
service (`--close-windows`), foreign-target execution, Snow
integration and a real Windows service install (`--install` needs an elevated
prompt, so the tests never register a service) must be checked on the intended
hardware.