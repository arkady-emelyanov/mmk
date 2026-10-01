#!/usr/bin/env python3
"""End-to-end test of the real binary, without touching the real keyboard or desktop.

Creates a fake (non-Apple, so an installed mmk ignores it) keyboard with uinput, runs `mmk run --device <fake>`, grabs mmk's
own virtual keyboard (so the desktop never sees the output), types through the fake
keyboard, and checks what mmk emits.

usage: tests/e2e.py [path/to/mmk]
"""
import fcntl, os, select, struct, subprocess, sys, tempfile, time

MMK = sys.argv[1] if len(sys.argv) > 1 else "target/debug/mmk"
HERE = os.path.dirname(os.path.abspath(__file__))

def IOC(d, t, nr, size): return (d << 30) | (size << 16) | (ord(t) << 8) | nr
UI_SET_EVBIT, UI_SET_KEYBIT = IOC(1, 'U', 100, 4), IOC(1, 'U', 101, 4)
UI_DEV_SETUP, UI_DEV_CREATE, UI_DEV_DESTROY = IOC(1, 'U', 3, 92), IOC(0, 'U', 1, 0), IOC(0, 'U', 2, 0)
EVIOCGRAB = IOC(1, 'E', 0x90, 4)
EV_SYN, EV_KEY = 0, 1

KEYS = {"esc": 1, "backspace": 14, "tab": 15, "q": 16, "t": 20, "u": 22, "enter": 28, "leftctrl": 29,
        "a": 30, "c": 46, "v": 47, "grave": 41, "leftshift": 42, "leftalt": 56, "space": 57,
        "home": 102, "left": 105, "right": 106, "end": 107, "f4": 62, "f19": 189,
        "leftmeta": 125, "rightmeta": 126, "rightctrl": 97}
NAMES = {v: k for k, v in KEYS.items()}

def ev(t, c, v): return struct.pack("llHHi", 0, 0, t, c, v)

class FakeKeyboard:
    def __init__(self):
        self.fd = os.open("/dev/uinput", os.O_WRONLY | os.O_NONBLOCK)
        fcntl.ioctl(self.fd, UI_SET_EVBIT, EV_KEY)
        for k in range(1, 256):
            fcntl.ioctl(self.fd, UI_SET_KEYBIT, k)
        name = b"mmk-e2e test keyboard"  # not Apple: an installed mmk must ignore it
        fcntl.ioctl(self.fd, UI_DEV_SETUP, struct.pack("HHHH80sI", 3, 0x1209, 0x0001, 1, name, 0))
        fcntl.ioctl(self.fd, UI_DEV_CREATE)
        self.node = wait_for_node(name.decode())

    def send(self, code, value):
        os.write(self.fd, ev(EV_KEY, code, value) + ev(EV_SYN, 0, 0))

    def close(self):
        fcntl.ioctl(self.fd, UI_DEV_DESTROY)
        os.close(self.fd)

def nodes_named(name):
    out = set()
    for e in os.listdir("/sys/class/input"):
        try:
            if e.startswith("event") and open(f"/sys/class/input/{e}/device/name").read().strip() == name:
                out.add(f"/dev/input/{e}")
        except OSError:
            pass
    return out

def wait_for_node(name, exclude=frozenset(), timeout=5):
    """Wait for a new event node called `name` that is not in `exclude`."""
    end = time.time() + timeout
    while time.time() < end:
        for e in sorted(os.listdir("/sys/class/input")):
            if not e.startswith("event"):
                continue
            try:
                path = f"/dev/input/{e}"
                if path not in exclude and open(f"/sys/class/input/{e}/device/name").read().strip() == name:
                    os.close(os.open(path, os.O_RDONLY))  # wait for udev permissions
                    return path
            except OSError:
                pass
        time.sleep(0.05)
    raise RuntimeError(f"device {name!r} did not appear")

def read_output(fd, quiet=0.15):
    """Collect key events until the device is quiet for `quiet` seconds."""
    out, buf = [], b""
    while select.select([fd], [], [], quiet)[0]:
        buf += os.read(fd, 24 * 64)
        while len(buf) >= 24:
            _, _, t, c, v = struct.unpack("llHHi", buf[:24])
            buf = buf[24:]
            if t == EV_KEY:
                out.append(("-+="[v]) + NAMES.get(c, f"key{c}"))
    return " ".join(out)

def main():
    cfg = open(os.path.join(HERE, "..", "src", "default.toml")).read()
    cfg = cfg.replace('tray           = true', 'tray           = false')
    cfg = cfg.replace('window_backend = "auto"', 'window_backend = "none"')
    cfgfile = tempfile.NamedTemporaryFile("w", suffix=".toml", delete=False)
    cfgfile.write(cfg)
    cfgfile.close()

    # An installed mmk may already be running with its own "mmk virtual keyboard". Never
    # touch that one: only the node that appears after we start our instance is ours.
    existing = nodes_named("mmk virtual keyboard")
    kbd = FakeKeyboard()
    env = dict(os.environ, XDG_RUNTIME_DIR=tempfile.mkdtemp())  # separate instance lock
    log = tempfile.NamedTemporaryFile("w+", suffix=".log", delete=False)
    mmk = subprocess.Popen([MMK, "run", "-c", cfgfile.name, "--device", kbd.node], stderr=log, env=env)
    out_fd = None
    failures = 0
    try:
        node = wait_for_node("mmk virtual keyboard", exclude=existing)
        out_fd = os.open(node, os.O_RDONLY | os.O_NONBLOCK)
        fcntl.ioctl(out_fd, EVIOCGRAB, 1)  # keep mmk's output away from the desktop
        end = time.time() + 5
        while "grabbed" not in open(log.name).read():
            if time.time() > end:
                raise RuntimeError("mmk did not grab the fake keyboard:\n" + open(log.name).read())
            time.sleep(0.05)

        # Handshake: a key typed on the fake keyboard must come out of the node we grabbed.
        # If it doesn't, we grabbed someone else's device: let go immediately.
        kbd.send(194, 1); kbd.send(194, 0)
        got = read_output(out_fd)
        if got != "+key194 -key194":
            fcntl.ioctl(out_fd, EVIOCGRAB, 0)
            raise RuntimeError(f"output device handshake failed ({got!r}); aborting without touching it")

        def case(name, events, expected):
            nonlocal failures
            read_output(out_fd, 0.05)
            for e in events.split():
                kbd.send(KEYS[e[1:]], {"+": 1, "-": 0, "=": 2}[e[0]])
            got = read_output(out_fd)
            ok = got == expected
            failures += not ok
            print(f"{'ok  ' if ok else 'FAIL'} {name}: {got}" + ("" if ok else f"\n     expected: {expected}"))

        case("plain typing", "+a -a", "+a -a")
        case("cmd+c -> ctrl+c", "+leftmeta +c -c -leftmeta", "+leftctrl +c -c -leftctrl")
        case("right cmd+v -> right ctrl+v", "+rightmeta +v -v -rightmeta", "+rightctrl +v -v -rightctrl")
        case("physical ctrl+c untouched", "+leftctrl +c -c -leftctrl", "+leftctrl +c -c -leftctrl")
        case("cmd+left -> home", "+leftmeta +left -left -leftmeta", "+home -home")
        case("opt+left -> ctrl+left", "+leftalt +left -left -leftalt", "+leftalt +leftctrl -leftalt +left -left -leftctrl")
        case("opt tap is a plain alt tap", "+leftalt -leftalt", "+leftalt -leftalt")
        case("cmd+backspace sequence", "+leftmeta +backspace -backspace -leftmeta", "+leftshift +home -home -leftshift +backspace -backspace")
        case("cmd+tab switcher with arrows", "+leftmeta +tab -tab +right -right +tab -tab -leftmeta",
             "+leftalt +tab -tab +right -right +tab -tab -leftalt")
        case("cmd+space layout switch", "+leftmeta +space -space -leftmeta", "+leftmeta +space -space -leftmeta")
        case("cmd+q -> alt+f4", "+leftmeta +q -q -leftmeta", "+leftalt +f4 -f4 -leftalt")
        case("held key repeats", "+leftmeta +left =left =left -left -leftmeta", "+home =home =home -home")

        # latency: fake key press -> mmk output event
        lat = []
        for _ in range(200):
            read_output(out_fd, 0.0)
            t0 = time.perf_counter()
            kbd.send(KEYS["a"], 1)
            select.select([out_fd], [], [], 1)
            lat.append((time.perf_counter() - t0) * 1e6)
            kbd.send(KEYS["a"], 0)
            read_output(out_fd, 0.01)
        lat.sort()
        p50, p99 = lat[len(lat) // 2], lat[int(len(lat) * 0.99)]
        ok = p99 < 2000
        failures += not ok
        print(f"{'ok  ' if ok else 'FAIL'} latency through mmk: p50 {p50:.0f} us, p99 {p99:.0f} us (includes kernel + python)")

        # panic chord: mmk exits and releases everything
        for k in ("esc", "backspace", "enter"):
            kbd.send(KEYS[k], 1)
        try:
            mmk.wait(timeout=3)
            print("ok   panic chord exits")
        except subprocess.TimeoutExpired:
            failures += 1
            print("FAIL panic chord did not exit")
    finally:
        if mmk.poll() is None:
            mmk.terminate()
            mmk.wait(timeout=3)
        if out_fd is not None:
            os.close(out_fd)
        kbd.close()
        os.unlink(cfgfile.name)
    if failures:
        print("\nmmk log:\n" + open(log.name).read())
    print(f"\n{'PASS' if not failures else f'{failures} FAILED'}")
    sys.exit(1 if failures else 0)

if __name__ == "__main__":
    main()
