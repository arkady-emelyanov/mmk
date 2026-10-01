# mmk

[![CI](https://github.com/arkady-emelyanov/mmk/actions/workflows/ci.yml/badge.svg)](https://github.com/arkady-emelyanov/mmk/actions/workflows/ci.yml)

Make Linux feel like macOS on an Apple keyboard, so switching between the two is seamless: your Mac shortcuts just work, in every app.

## Install

Download the static binary for your machine from the [latest release](https://github.com/arkady-emelyanov/mmk/releases/latest) and run its installer:

```
curl -Lo /tmp/mmk https://github.com/arkady-emelyanov/mmk/releases/latest/download/mmk-$(uname -m)-linux
chmod +x /tmp/mmk
/tmp/mmk install
```

Builds are available for `x86_64` and `aarch64`; each has a `.sha256` file next to it. To build from source instead:

```
cargo build --release --target x86_64-unknown-linux-musl
target/x86_64-unknown-linux-musl/release/mmk install
```

`mmk install` sets up permissions (one `pkexec` step, only if needed), copies the binary to `~/.local/bin`, writes `~/.config/mmk/config.toml`, adds an autostart entry, warns about other running key remappers, and starts mmk. `mmk uninstall` reverses all of it.

### Why the installer may ask for your password

mmk runs as your normal user, never as root. To work it needs two devices that a stock Linux system only lets root use:

| Device | Why mmk needs it | Default permissions |
|---|---|---|
| Your Apple keyboard, `/dev/input/eventN` | read its keys exclusively, so the original keystrokes don't also reach the desktop | root and the `input` group |
| `/dev/uinput` | create the virtual keyboard that mmk types the remapped keys through | root only |

If you can't open both, `mmk install` shows exactly what it is about to write and asks for your password once (through `pkexec`, or `sudo` if `pkexec` isn't available). It then, as root:

- writes `/etc/udev/rules.d/70-mmk.rules`, which gives the logged-in user access to `/dev/uinput` and to input devices with Apple's vendor IDs (`05ac`, `004c`) only. Other keyboards and mice stay off-limits, unlike adding yourself to the `input` group, which exposes every input device;
- writes `/etc/modules-load.d/mmk.conf` so the `uinput` kernel module loads at boot;
- loads the module and reloads the udev rules, so no re-login is needed.

That is the only privileged step. Running mmk, editing the config, updating and uninstalling all happen as your user; `mmk uninstall` asks for the password again only to delete those two files.

The step is skipped if you already have access. That happens, for example, when you're in the `input` group, or when another package's udev rules already grant `/dev/uinput`. `mmk doctor` shows which access is in place.

## Use

- Edit `~/.config/mmk/config.toml`; it reloads on save.
- `mmk doctor` checks permissions, keyboards, focus tracking and conflicts.
- `mmk focus` shows which profile the focused app gets.
- Tray icon: enable/disable, current profile, reload, open config, quit.
- Emergency exit: hold **Esc + Backspace + Enter**.
- Log: `~/.local/state/mmk/mmk.log`.

## Known limitations

- VS Code's integrated terminal: mmk can't tell it apart from the editor, and VS Code on Linux uses some keystrokes differently there. Opt+Shift+←/→ resizes the terminal pane (it selects words in the editor), Cmd+Backspace doesn't delete to line start (use Ctrl+U), and copy/paste is Cmd+Shift+C / Cmd+Shift+V.
- Cmd+click is a plain click (Cmd is only translated together with a key); physical Ctrl+click works as usual.
- Focus tracking (per-app profiles) works on X11. On Wayland sessions every app gets the global profile for now.

## Develop

```
cargo test                      # engine and config unit tests
python3 tests/e2e.py            # end-to-end through a fake Apple keyboard (needs /dev/uinput access)
```

The end-to-end test never touches the real keyboard or the desktop: it runs mmk against a fake uinput keyboard and grabs mmk's output device.
