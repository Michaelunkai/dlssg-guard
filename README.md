# dlssg-guard

Stop `DXGI_ERROR_INVALID_CALL` / "present failed" crashes caused by NVIDIA
Streamline **DLSS Frame Generation** (DLSS-G) on Windows, for every game on the machine.
Pure Rust, no dependencies, one small `.exe`.

## The problem

Games that ship the Streamline plugin `sl.dlss_g.dll` (and NVIDIA's
`nvngx_dlssg.dll`) can crash on present with `DXGI_ERROR_INVALID_CALL`.
Without those files Streamline reports the feature as unsupported and the game
presents normally. Game updates and new installs bring the files back, so a one-off
fix does not last.

## How it protects you

| Layer | What it does |
|-------|--------------|
| File layer | Renames every `sl.dlss_g*.dll` / `nvngx_dlssg*.dll` (any case, `.bak` and copy variants, up to 30 folders deep, all drives) to `*.dlssg-disabled`. Reversible. |
| Config layer | Writes `r.Streamline.DLSSG.Enable=0` into every Unreal Engine `Saved/Config/Windows*/Engine.ini` (idempotent, one line, keeps your other settings). |
| Watcher | A scheduled task runs `watch` at startup: single instance, panic-safe, re-applies both layers every minute, logs to `C:\ProgramData\dlssg-guard\guard.log`. |
| Robustness | Clears read-only attributes, retries with backoff on locked files, skips unreadable folders, reports admin status, never crashes. |

OS and driver folders (`Windows`, `DriverStore`, `NVIDIA Corporation`, ...) are never touched.

## Usage

```text
dlssg-guard diag        read-only report: GPU/driver, HAGS, driver-store dlssg files, NGX model dir,
                        Streamline dll versions found in games, Engine.ini status, task + watcher state
dlssg-guard scan        list exposed files (no changes)
dlssg-guard fix         neutralize everything now, exit 0 only if nothing is left exposed
dlssg-guard restore     undo the renames
dlssg-guard install     fix + install the scheduled task (run as administrator)
dlssg-guard uninstall   remove the scheduled task
dlssg-guard watch       the guard loop (used by the task)
dlssg-guard tail [-f]   show / follow the guard log
```

Every command accepts optional root folders after the command name to limit the scope,
for example `dlssg-guard fix D:\Games`. All output is timestamped and live
(drive progress, per-file `[n/N]` lines, summary).

## Build

```text
cargo build --release
cargo test
cargo clippy --all-targets
```

## Notes

* Frame Generation will not be available in the affected games while the guard is active.
  Run `dlssg-guard restore` and `dlssg-guard uninstall` to get it back.
* Works on Windows 10/11 with any NVIDIA RTX driver.

## License

MIT, see [LICENSE](LICENSE).
