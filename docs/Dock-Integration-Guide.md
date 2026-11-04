# Dock Integration Guide (Host Link v1)

For the **host side** (Dock, launcher, settings panel, script). The provider
side is ModularClipboard; read `HostLink-Protocol.md` first. This guide is
intentionally app-agnostic: implement a generic `HostLinkProvider` driven by
the manifest, not by a hard-coded application name.

## 1. What to build

A small provider with four responsibilities:

1. **Discover** - find `host-link.json` through the documented channels.
2. **Validate** - schema, exe existence, fingerprints, freshness.
3. **Act** - spawn `commands.activate` on click; `commands.probe` for health.
4. **Recover** - follow `repair` from the probe report; never invent paths.

Keep the provider generic. It should work for any application publishing
`mc.host-link/v1`, with `app.id` as the only stable key.

## 2. Discovery implementation

Order matters, but **absence of one channel is not failure**:

```text
1. MODULAR_CLIPBOARD_HOST_LINK  (file path or directory)
2. %APPDATA%/modular-clipboard/host-link.json
3. <exe dir>/host-link.json
4. <Start Menu Programs>/host-link.json
5. HKCU\Software\ModularClipboard :: HostLinkPath -> absolute path
6. <Start Menu Programs>/ModularClipboard.lnk  (target args: --host-activate)
```

Practical notes:

* Read the manifest's own `discovery` block for logging; do not duplicate the
  literal paths in your own logic beyond the bootstrap entries.
* Channel 6 exists so that a launcher which only scans `.lnk` files still sees
  the application. Its arguments are `--host-activate`, not an empty command
  line, because the UI process is spawned on demand.
* If no channel yields a valid manifest, that is `LINK_NOT_FOUND`. It means
  "not published yet", not "not installed": the app may simply have never been
  started. Offer a rescan or a repair, not a fatal error.

## 3. Calling the commands

The exe is a GUI-subsystem binary. `Command::new(...).output()` still works -
the provider writes JSON with Win32 `GetStdHandle` + `WriteFile` - but always
implement the file fallback:

* `%APPDATA%/modular-clipboard/host-last.json` - result of the last command.
* `%APPDATA%/modular-clipboard/host-probe.json` - last probe verbatim.

Use the `exe` and `args` from `commands.*` verbatim. Never re-build a command
line from `endpoints.internal_ipc`; that pipe is private and marked
`external_use: "not-supported"`.

Exit codes: `0` = ok, `1` = problem found (read `error.code`), `2` = bad CLI
arguments. Treat a non-zero exit with valid JSON as a normal health result, not
as a crash.

## 4. Minimal provider sketch (Rust-flavored pseudocode)

```text
struct HostLink { path, exe, id, commands, capabilities }

fn discover() -> Result<HostLink, ErrorCode> {
    for p in bootstrap_paths() {
        if let Some(v) = read_json_if_valid_schema(p, "mc.host-link/v1") {
            return Ok(HostLink::from(v));
        }
    }
    Err(LINK_NOT_FOUND)
}

fn probe(link) -> ProbeReport {
    spawn(link.commands.probe)          // or read host-probe.json
        .and_then(parse_json)
        .unwrap_or_else(file_fallback)
}

fn on_click(link) {
    if !probe(link).ok {
        for step in probe.repair { spawn(link.commands[step.command]); }
    }
    spawn(link.commands.activate);
}
```

Keep a cache of the last successful link, but re-read it whenever the probe
reports `EXE_CHANGED`, `LINK_STALE`, or `EXE_MISSING`.

## 5. Error handling contract

| Code | Action |
| --- | --- |
| `LINK_NOT_FOUND` | Rescan channels; if still nothing, run `publish` only when you know the exe path (e.g. from channel 6) |
| `LINK_UNREADABLE` | Run `doctor`; do not delete files yourself |
| `LINK_SCHEMA_UNSUPPORTED` | Degrade to `executable.path` + `app.id`; never guess new fields |
| `LINK_STALE` | Run `publish`, then re-read |
| `EXE_MISSING` / `EXE_CHANGED` | Run `publish` from the exe you actually found, then re-probe |
| `DAEMON_NOT_RUNNING` | Run `activate` (it starts the daemon if needed) |
| `PUBLISH_FAILED` | Surface `diagnostics` (usually permissions) |
| `ACTIVATE_FAILED` | Surface `error.message`; rescan |

Always prefer `probe.repair` over a locally coded recovery table: the provider
side knows which action is currently valid.

## 6. Acceptance tests (fault injection)

A host integration is done only when all of these pass. Each row simulates a
real way the Dock can fail to see the application.

| # | Inject | Expected host behaviour |
| --- | --- | --- |
| 1 | Delete the primary `host-link.json` | Still discovers via exe-dir / Start Menu copy; no "app missing" state |
| 2 | Write invalid JSON into every manifest | Probe reports `LINK_UNREADABLE`; `doctor` restores; a rescan finds it again |
| 3 | Move the whole application folder | `publish` from the moved exe rewrites `executable.path`; fingerprint check passes |
| 4 | Kill the daemon | Probe reports `DAEMON_NOT_RUNNING`; `activate` starts it; next probe is green |
| 5 | Replace/upgrade the exe | Probe reports `EXE_CHANGED`; `publish` refreshes size/mtime/SHA-256 |
| 6 | Write a manifest with `schema: mc.host-link/v99` | Host degrades gracefully, no panic, no guessed fields |
| 7 | Launch from a host with no console/pipe | Host falls back to `host-last.json` / `host-probe.json` |
| 8 | Click the Start Menu `.lnk` | Main window appears (args `--host-activate` are preserved) |
| 9 | Hide the window so the UI process exits | Host does not treat `ui.running: false` as an error |

## 7. Things that will break the integration

* Scanning only Start Menu `.lnk` files and ignoring the manifest.
* Connecting to `\\.\pipe\ModularClipboard.Daemon.v1` (private UI channel).
* Treating `ui.running: false` as "not installed".
* Hard-coding a version, an install directory, or a window title.
* Parsing human-readable strings instead of `app.id`.
* Assuming `--host-*` commands can be combined with other CLI flags.

## 8. Privacy boundary

v1 capabilities are `discovery`, `health.ping`, `ui.toggle`. No clipboard
content is read or written. A future schema may add read capabilities; the host
must check `capabilities` before enabling any UI that displays history, and
must not fall back to scraping the internal IPC pipe.
