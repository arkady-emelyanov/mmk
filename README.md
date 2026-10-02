# mmk <sub>my magic keyboard</sub>

[![CI](https://github.com/arkady-emelyanov/mmk/actions/workflows/ci.yml/badge.svg)](https://github.com/arkady-emelyanov/mmk/actions/workflows/ci.yml)

Make Linux feel like macOS on an Apple keyboard, so switching between the two is seamless: your Mac shortcuts just work, in every app.

## Supported desktops

| Desktop | Status |
|---|---|
| Cinnamon (Linux Mint), X11 | Tested, the default config targets it |
| MATE, Xfce, KDE Plasma, GNOME on X11 | Should work, untested. A few system shortcuts depend on the desktop's own bindings (`Cmd+Space` → `Super+Space`, `Cmd+Ctrl+F` → `Alt+F10`, `Cmd+Shift+3/4` → `Print`). GNOME needs the AppIndicator extension for the tray icon. |
| Any Wayland session | Remapping works, but per-app profiles don't yet: every app gets the global profile. |

mmk reads the keyboard below the display server (evdev/uinput), so remapping doesn't depend on the desktop. Only the per-app profiles (X11 window focus) and the tray icon (StatusNotifierItem) do.

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
- Emergency exit: hold `Esc+Backspace+Enter`.
- Log: `~/.local/state/mmk/mmk.log`.

## Configure

The config lives in `~/.config/mmk/config.toml` and reloads on save; if it has an error, mmk keeps the previous config and logs why. `mmk check` validates it without running. The file written by `mmk install` (or `mmk init`) is the full default config with comments, so start from it: settings you leave out fall back to their defaults, but `[keys]` and `[[app]]` sections are not merged with the defaults. The file defines all rules and profiles.

Rules map a physical trigger to an action:

```toml
[keys]
"cmd-left"      = "home"                  # single chord, held while the key is held
"cmd-backspace" = "shift-home backspace"  # sequence of chords, tapped in order
"cmd-shift-k"   = "exec:notify-send hi"   # run a command on press
"cmd-h"         = "none"                  # swallow the key
```

Triggers use the Mac modifiers `cmd`, `opt`, `ctrl`, `shift` and `fn`; actions use `ctrl`, `shift`, `alt` and `super`. Keys use evdev names (`a`, `1`, `f12`, `left`, `pageup`, `backspace`, `grave`, `leftbrace`, `comma`, `dot`, …). `Cmd+key` without a rule becomes `Ctrl+key` (the `cmd` setting).

App profiles override the global rules for matching windows. The first profile whose `class` matches wins:

```toml
[[app]]
name  = "terminal"
class = ["kitty", "org.gnome.terminal", "*ghostty*"]  # globs, case-insensitive
cmd   = "ctrl-shift"         # Cmd+key without a rule becomes Ctrl+Shift+key here
[app.keys]
"opt-left" = "alt-b"

[[app]]
name  = "raw"
class = ["virt-manager"]
raw   = true                 # pass keys through untouched (VMs, remote desktops)
```

### Finding an app's WM_CLASS

Focus the app while `mmk focus` is running. It prints each focused window's class and instance, and the profile it gets:

```
$ mmk focus
backend: x11  (Ctrl+C to stop)
app_id="kitty" instance="kitty" -> profile terminal
app_id="FreeCAD" instance="freecad" -> profile global
```

A `class` glob matches either value. Without mmk you can run `xprop WM_CLASS` and click the window; it prints `WM_CLASS(STRING) = "instance", "Class"`. Focus tracking works on X11 only, so on Wayland every app gets the global profile.

## Known limitations

- VS Code's integrated terminal: mmk can't tell it apart from the editor, and VS Code on Linux uses some keystrokes differently there. `Opt+Shift+←/→` resizes the terminal pane (it selects words in the editor), `Cmd+Backspace` doesn't delete to line start (use `Ctrl+U`), and copy/paste is `Cmd+Shift+C` / `Cmd+Shift+V`.
- `Cmd+click` is a plain click (Cmd is only translated together with a key); physical `Ctrl+click` works as usual.
- Focus tracking (per-app profiles) works on X11. On Wayland sessions every app gets the global profile for now.

## Develop

```
cargo test                      # engine and config unit tests
python3 tests/e2e.py            # end-to-end through a fake Apple keyboard (needs /dev/uinput access)
```

The end-to-end test never touches the real keyboard or the desktop: it runs mmk against a fake uinput keyboard and grabs mmk's output device.
