# Installing `msf`

Apple Silicon Mac only (M1 and newer). Intel Macs are not supported.

## One-line install

```sh
curl -fsSL https://raw.githubusercontent.com/marshallku/mac-smart-fans/master/install.sh | sh
```

Installs the latest release binary to `~/.local/bin/msf`. Override with env vars:

| Variable | Default | Purpose |
| --- | --- | --- |
| `MSF_VERSION` | latest release | Tag to install (e.g. `v0.1.0`). |
| `MSF_INSTALL_DIR` | `$HOME/.local/bin` | Install directory. |
| `MSF_NO_VERIFY` | unset | Skip SHA256 checksum verification. |

If `~/.local/bin` is not on your `PATH`, the installer prints the `export PATH=...` line to add to your shell profile.

## Homebrew

```sh
brew install marshallku/tap/mac-smart-fans
```

Installs `msf` to `$(brew --prefix)/bin/msf` (typically `/opt/homebrew/bin/msf` on Apple Silicon). `brew upgrade mac-smart-fans` handles updates.

## After install — grant root for fan control

`msf monitor`, `msf calibrate`, `msf init`, `msf probe`, and `msf status` work as a regular user. `msf set`, `msf run`, `msf install`, and `msf uninstall` require root because the System Management Controller (SMC) rejects fan writes from non-root processes.

To use `sudo msf …` without typing your password each time, add a NOPASSWD sudoers entry:

```sh
# Find your installed binary path
which msf
# e.g. /opt/homebrew/bin/msf (brew) or /Users/<you>/.local/bin/msf (curl|sh)

# Write a sudoers entry (replace <user> and <path> with your values)
sudo tee /etc/sudoers.d/msf-<user> <<EOF
<user> ALL=(root) NOPASSWD: <path>
EOF
sudo chmod 0440 /etc/sudoers.d/msf-<user>
sudo visudo -c -f /etc/sudoers.d/msf-<user>
```

**Caution**: NOPASSWD entries grant your user the ability to run that specific binary as root without authentication. Only do this on your own machine, and only for `msf` itself (not a wrapper script or directory).

## Persistent daemon (curve loop on boot)

Once you have a working curve profile:

```sh
msf init --profile balanced --output ~/.config/msf/curve.toml
sudo msf install --curve ~/.config/msf/curve.toml
msf status
```

By default the daemon controls **all controllable fans** (equivalent to `--fan all`). To target only specific fans, pass a single index (`--fan 0`) or a comma-separated list (`--fan 0,1`).

This writes a LaunchDaemon plist at `/Library/LaunchDaemons/im.toss.mac-smart-fans.plist` (root:wheel 0644) and loads it via `launchctl bootstrap`. The daemon logs to `/var/log/msf/`.

Stop and remove the daemon:

```sh
sudo msf uninstall
```

## Uninstall `msf`

```sh
# Stop daemon if installed
sudo msf uninstall

# Remove the binary
rm "$(which msf)"

# Remove sudoers entry if you added one
sudo rm /etc/sudoers.d/msf-<user>

# Remove brew-installed copy (if applicable)
brew uninstall mac-smart-fans
brew untap marshallku/tap   # only if you installed nothing else from this tap

# Remove logs and config (optional)
sudo rm -rf /var/log/msf
rm -rf ~/.config/msf
```

## Build from source

If you'd rather not run a prebuilt binary, the repo builds with stable Rust 1.94+:

```sh
git clone https://github.com/marshallku/mac-smart-fans.git
cd mac-smart-fans
cargo build --release -p msf
./target/release/msf --version
```
