# Host Link Protocol v1

A toolkit-agnostic protocol that lets an external host (Dock, launcher,
script, status bar) **discover, health-check and activate** ModularClipboard
without guessing install directories, scanning for a window, or connecting to
the private UI IPC pipe.

* Schema: `mc.host-link/v1`
* Capability set: `discovery`, `health.ping`, `ui.toggle`
* This version does **not** expose clipboard history or write access.
* Canonical source of this document: `docs/HostLink-Protocol.md` in the
  repository (stable link, always available). The Wiki is an optional mirror:
  run `scripts/sync-hostlink-wiki.sh` after creating the first Wiki page in a
  browser once (GitHub does not create `.wiki.git` until then).

## 1. The problem

Typical hosts discover applications by scanning Start Menu `.lnk` files,
hard-coding an install path, or looking for a running window. All three fail
here:

| Host assumption | Reality |
| --- | --- |
| There is a Start Menu shortcut | Portable deployment + HKCU Run autostart; no installer, no `.lnk` |
| The exe lives in a known directory | The user can move the whole folder at any time |
| A window is always present | The UI process is spawned on demand and exits when hidden |
| `println!` output is readable | The binary is `windows_subsystem = "windows"` (no console) |
| Any named pipe is fair game | `\\.\pipe\ModularClipboard.Daemon.v1` is the private UI<->daemon channel |

So the application publishes a **self-describing manifest** to several stable
locations, and the host only has to find it, validate it, and invoke the
commands listed inside it.

## 2. Discovery order

Try these in order; stop at the first manifest whose `schema` equals
`mc.host-link/v1` and which parses as JSON.

1. Environment variable `MODULAR_CLIPBOARD_HOST_LINK` - absolute path to a
   manifest file, or a directory containing `host-link.json`.
2. `%APPDATA%\modular-clipboard\host-link.json` (primary).
3. `<directory of modular-clipboard.exe>\host-link.json` (portable / moved
   installation).
4. `%APPDATA%\Microsoft\Windows\Start Menu\Programs\host-link.json`.
5. Registry fallback: `HKCU\Software\ModularClipboard` value `HostLinkPath`
   holds the absolute path of the primary manifest.
6. Start Menu shortcut `%APPDATA%\Microsoft\Windows\Start Menu\Programs\
   ModularClipboard.lnk`. It targets the exe with `--host-activate`, so a host
   that only scans `.lnk` files still finds and can launch the app.

The manifest itself repeats all of these in `discovery`, so a host never has to
hard-code them. If a path is unavailable, skip it and continue - never treat a
single missing path as "application not installed".

## 3. Manifest fields

| Field | Meaning |
| --- | --- |
| `schema` | `mc.host-link/v1`. Unknown value -> degrade, do not guess fields |
| `spec` | URL of this protocol document |
| `published_unix` | Unix seconds when the manifest was written |
| `app.id` | Stable identity, use this for de-duplication (`com.mcheng404.modular-clipboard`) |
| `app.name`, `app.version` | Display only; never use for lookup |
| `publisher.pid`, `publisher.role` | Process that wrote the manifest |
| `executable.path` | **Absolute path of the current exe**, derived from `current_exe()` at publish time |
| `executable.size`, `modified_unix`, `sha256` | Fingerprint for detecting upgrades / replacements / moves |
| `capabilities` | Negotiation list; v1 is discover + health + activate only |
| `commands.*` | `{ "exe": "...", "args": ["--host-probe"] }` - spawn these, never construct your own command line |
| `endpoints.cli` | Process channel; stdout is JSON |
| `endpoints.files` | `link`, `probe`, `last_result` - pure file channel, no process spawn needed |
| `endpoints.internal_ipc` | **Informational only**. `external_use: "not-supported"`; do not connect |
| `discovery.*` | All discovery channels, for logging and diagnostics |
| `limits.stale_after_secs` | 7 days; older manifests should be refreshed, not rejected |
| `limits.probe_timeout_ms` | Suggested timeout for a probe call |

Manifests are written atomically (same-directory `.tmp` then rename) and read
back after writing, so a host never observes a half-written JSON document.

## 4. Commands

Every `--host-*` switch must be used **alone** (no `--daemon`, `--ui`,
`--data-dir`, ...). These commands run before the single-instance lock,
configuration loading, logging and tray creation, so they keep working when
the config is broken or the daemon has crashed.

| Switch | Effect | Exit code |
| --- | --- | --- |
| `--host-link` | Print the manifest JSON, no writes | 0 |
| `--host-probe` | Full health check, JSON report | 0 ok / 1 problem found |
| `--host-publish` | Re-publish manifest to all points + registry + Start Menu `.lnk` | 0 ok / 1 all writes failed |
| `--host-doctor` | `probe` -> `publish` on failure -> `probe` again, one JSON report | 0 ok / 1 still failing |
| `--host-activate` | Show the main window; spawn the daemon if the pipe is down | 0 ok / 1 failure |

**Output channels.** The JSON goes to stdout via Win32 `GetStdHandle` +
`WriteFile` (a GUI-subsystem binary has no Rust `println!` console), with
`AttachConsole` as a fallback. The same result is mirrored to:

* `%APPDATA%\modular-clipboard\host-last.json` - last command of any kind,
  wrapped as `mc.host-result/v1` with `command`, `checked_unix`, `result`.
* `%APPDATA%\modular-clipboard\host-probe.json` - last probe report verbatim.

A host that cannot spawn processes can poll those two files instead.

## 5. Probe report

`--host-probe` returns `mc.host-probe/v1`:

```json
{
  "schema": "mc.host-probe/v1",
  "ok": false,
  "checked_unix": 1793800000,
  "source": "C:\\Users\\...\\modular-clipboard\\host-link.json",
  "app": { "id": "...", "name": "...", "version": "..." },
  "checks": [
    { "name": "link.found",      "ok": true,  "detail": "..." },
    { "name": "link.fresh",      "ok": true,  "detail": "..." },
    { "name": "exe.exists",      "ok": true,  "detail": "..." },
    { "name": "exe.fingerprint", "ok": true,  "detail": "..." },
    { "name": "daemon.pipe",     "ok": false, "detail": "no listener" },
    { "name": "ui.running",      "ok": false, "detail": "not running (normal when hidden)" }
  ],
  "error": { "code": "DAEMON_NOT_RUNNING", "message": "..." },
  "diagnostics": ["..."],
  "repair": [ { "command": "activate", "reason": "..." } ]
}
```

`ui.running` is informational: a hidden UI is normal. `error.code` is the
**first** failing check, and `repair` always contains executable next steps.

| Code | Meaning | Host response |
| --- | --- | --- |
| `LINK_NOT_FOUND` | No manifest in any discovery path | Run `commands.publish` |
| `LINK_UNREADABLE` | Manifest exists but is invalid JSON | Run `commands.doctor` |
| `LINK_SCHEMA_UNSUPPORTED` | Manifest parses but `schema` is unknown | Degrade: use only `executable.path` if present; do not guess new fields |
| `LINK_STALE` | Older than `limits.stale_after_secs` | Run `commands.publish` |
| `EXE_MISSING` | Recorded exe no longer exists | Run `commands.publish` from the current exe |
| `EXE_CHANGED` | Size / mtime / SHA-256 differ (upgrade or replacement) | Run `commands.publish` |
| `DAEMON_NOT_RUNNING` | Private IPC pipe has no listener | Run `commands.activate` |
| `PUBLISH_FAILED` | Every publish target failed | Check directory permissions; surface diagnostics |
| `ACTIVATE_FAILED` | Could not show or start | Check `executable.path`, then `publish` |

## 6. Failure matrix

| Failure class | Example | Detected by | Self-healing |
| --- | --- | --- | --- |
| Directory error | App moved, primary manifest deleted, path hard-coded wrongly | `link.found` / `exe.exists` | Other discovery paths still work; `publish` rewrites every path from `current_exe()` |
| Logic error | Half-written JSON, wrong schema, host connecting to the private pipe | `LINK_UNREADABLE`, `LINK_SCHEMA_UNSUPPORTED` | Atomic writes; `doctor` rewrites; `external_use` flag forbids the private pipe |
| Program error | Daemon crashed, config corrupt, UI process exited | `DAEMON_NOT_RUNNING` | `--host-*` runs before config/lock; `activate` restarts; `doctor` republishes |
| Stale state | Old exe upgraded in place | `exe.fingerprint` | `publish` refreshes size / mtime / SHA-256 |
| Console error | Host sees empty stdout from a GUI subsystem binary | `endpoints.files` | Every result is mirrored to `host-last.json` |

## 7. Rules for hosts

Do:

* Treat `app.id` as the identity and `commands.*` as the only supported actions.
* Validate `schema`, `executable.path`, and at least one fingerprint before use.
* Use `repair` from the probe report instead of inventing your own recovery.
* Cache the manifest, but re-read it on `EXE_CHANGED` or `LINK_STALE`.
* Implement this as a generic `HostLinkProvider`; do not special-case the app name.

Do **not**:

* Connect to `\\.\pipe\ModularClipboard.Daemon.v1` (private UI channel, single-UI
  semantics, `Sync{known}` state machine).
* Assume a window, a tray icon, or a running UI process.
* Hard-code `%APPDATA%\modular-clipboard` as the only location.
* Treat "one discovery path missing" as "application absent".
* Parse display strings or the exe file name to identify the app.

## 8. Compatibility

* The manifest `schema` is versioned. A host that sees an unknown version must
  degrade to the fields it understands and must not fabricate commands.
* `capabilities` is additive: new entries may appear; never assume a capability
  exists just because the app is installed.
* v1 exposes no clipboard data. A future version adding read access will use a
  new schema or a new capability, and hosts must opt in explicitly.

## 9. Minimal host pseudocode

```text
path   = first_existing(discovery_paths_from_env_and_registry)
link   = parse_json(path)
if link.schema != "mc.host-link/v1": degrade()
report = spawn(link.commands.probe) -> parse stdout JSON
if not report.ok:
    for step in report.repair:
        spawn(link.commands[step.command])
    report = spawn(link.commands.probe) again
on_click = spawn(link.commands.activate)
```

See `docs/Dock-Integration-Guide.md` for the full integration checklist and
fault-injection acceptance tests.
