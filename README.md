# mac-smart-fans (`msf`)

Temperature-driven fan control for Apple Silicon Macs. Reads PMU/SOC temperatures via the private HID event system, drives the SMC directly to set fan RPM along a user-defined curve, and installs as a LaunchDaemon so curve control survives reboots and sleep/wake cycles.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/marshallku/mac-smart-fans/master/install.sh | sh
```

Or via Homebrew:

```sh
brew install marshallku/tap/mac-smart-fans
```

See [INSTALL.md](./INSTALL.md) for sudoers setup, daemon install, and uninstall steps.

## Quick tour

```sh
msf monitor                                      # stream HID temperatures
msf probe                                        # SMC fan capability report
msf calibrate add cpu <hid-id>                   # tag sensors for curve trip logic
msf init --profile balanced -o curve.toml        # generate a starter curve
sudo msf set 0 2500 --duration-secs 5            # one-shot fan set (auto-restore)
sudo msf run --curve curve.toml --fan 0          # foreground curve loop
sudo msf install --curve curve.toml --fan 0      # install as launchd daemon
msf status                                       # daemon health
```

## Supported hardware

- Apple Silicon (M1, M1 Pro/Max/Ultra, M2, M3, M4 — anything with `AppleSMC` direct-mode unlock and HID PMU temperature events).
- macOS 13+ (older versions untested).

Intel Macs are **not** supported — SMC key data types and HID enumeration differ.

## Safety

- Direct-mode SMC writes only. Restores fan mode + previous RPM on exit (Ctrl-C, SIGTERM, or sensor trip).
- Sensor-based trip: if any tagged temperature exceeds 90°C the loop restores fans to firmware control.
- Sleep/wake re-arm: per-tick `F{N}Md` readback detects mode drift and re-arms; 3 consecutive failures trip cleanly.
- No telemetry, no cloud, no auto-update.

## License

See [LICENSE](./LICENSE) if present, or treat as all rights reserved until one is added.
