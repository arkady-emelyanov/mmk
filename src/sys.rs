//! Raw Linux input plumbing: evdev ioctls and the uinput virtual keyboard.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::engine::Sink;

pub const EV_SYN: u16 = 0;
pub const EV_KEY: u16 = 1;
pub const EV_LED: u16 = 0x11;
const EV_SIZE: usize = 24; // struct input_event on 64-bit

const fn ioc(dir: u64, ty: u8, nr: u8, size: usize) -> u64 {
    dir << 30 | (size as u64) << 16 | (ty as u64) << 8 | nr as u64
}
const IOC_W: u64 = 1;
const IOC_R: u64 = 2;

const EVIOCGID: u64 = ioc(IOC_R, b'E', 0x02, 8);
const fn eviocgname(len: usize) -> u64 { ioc(IOC_R, b'E', 0x06, len) }
const fn eviocgbit(ev: u8, len: usize) -> u64 { ioc(IOC_R, b'E', 0x20 + ev, len) }
const fn eviocgkey(len: usize) -> u64 { ioc(IOC_R, b'E', 0x18, len) }
const EVIOCGRAB: u64 = ioc(IOC_W, b'E', 0x90, 4);

const UI_SET_EVBIT: u64 = ioc(IOC_W, b'U', 100, 4);
const UI_SET_KEYBIT: u64 = ioc(IOC_W, b'U', 101, 4);
const UI_SET_LEDBIT: u64 = ioc(IOC_W, b'U', 105, 4);
const UI_DEV_SETUP: u64 = ioc(IOC_W, b'U', 3, 92);
const UI_DEV_CREATE: u64 = ioc(0, b'U', 1, 0);
const UI_DEV_DESTROY: u64 = ioc(0, b'U', 2, 0);

/// Apple's USB and Bluetooth vendor IDs.
pub const APPLE_VENDORS: [u16; 2] = [0x05ac, 0x004c];
/// Our virtual device's id (pid.codes test VID), so we never grab ourselves.
const OUR_VENDOR: u16 = 0x1209;
const OUR_PRODUCT: u16 = 0x6d6b;
pub const OUR_NAME: &str = "mmk virtual keyboard";

fn ioctl<T>(f: &File, req: u64, arg: *mut T) -> io::Result<i32> {
    let r = unsafe { libc::ioctl(f.as_raw_fd(), req as libc::Ioctl, arg) };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

fn ioctl_int(f: &File, req: u64, val: libc::c_int) -> io::Result<i32> {
    let r = unsafe { libc::ioctl(f.as_raw_fd(), req as libc::Ioctl, val) };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

pub fn event_bytes(ty: u16, code: u16, value: i32) -> [u8; EV_SIZE] {
    let mut b = [0u8; EV_SIZE];
    b[16..18].copy_from_slice(&ty.to_ne_bytes());
    b[18..20].copy_from_slice(&code.to_ne_bytes());
    b[20..24].copy_from_slice(&value.to_ne_bytes());
    b
}

pub fn parse_event(b: &[u8]) -> (u16, u16, i32) {
    (
        u16::from_ne_bytes([b[16], b[17]]),
        u16::from_ne_bytes([b[18], b[19]]),
        i32::from_ne_bytes([b[20], b[21], b[22], b[23]]),
    )
}

/// Read events from a blocking fd, calling `f(type, code, value)` for each.
pub fn read_events(file: &mut File, mut f: impl FnMut(u16, u16, i32)) -> io::Result<()> {
    let mut buf = [0u8; EV_SIZE * 64];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        for ev in buf[..n].chunks_exact(EV_SIZE) {
            let (t, c, v) = parse_event(ev);
            f(t, c, v);
        }
    }
}

/// An opened evdev input device.
pub struct InputDevice {
    pub file: File,
    pub name: String,
    pub vendor: u16,
}

impl InputDevice {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).custom_flags(libc::O_CLOEXEC).open(path)?;
        let mut id = [0u16; 4];
        ioctl(&file, EVIOCGID, id.as_mut_ptr())?;
        let mut name = [0u8; 256];
        let n = ioctl(&file, eviocgname(name.len()), name.as_mut_ptr()).unwrap_or(0) as usize;
        let name = String::from_utf8_lossy(&name[..n.min(256)]).trim_end_matches('\0').to_string();
        Ok(InputDevice { file, name, vendor: id[1] })
    }

    fn key_bits(&self, req: fn(usize) -> u64) -> io::Result<[u8; 96]> {
        let mut bits = [0u8; 96];
        ioctl(&self.file, req(bits.len()), bits.as_mut_ptr())?;
        Ok(bits)
    }

    /// An Apple keyboard: Apple vendor id and real letter keys (skips the Magic
    /// Keyboard's extra interfaces, trackpads, and our own virtual device).
    pub fn is_apple_keyboard(&self) -> bool {
        if !APPLE_VENDORS.contains(&self.vendor) || self.name == OUR_NAME {
            return false;
        }
        match self.key_bits(|len| eviocgbit(EV_KEY as u8, len)) {
            Ok(b) => has_bit(&b, crate::keys::KEY_A) && has_bit(&b, crate::keys::KEY_SPACE),
            Err(_) => false,
        }
    }

    /// True if no key is currently held down.
    pub fn all_keys_up(&self) -> bool {
        self.key_bits(eviocgkey).map(|b| b.iter().all(|x| *x == 0)).unwrap_or(true)
    }

    pub fn grab(&self) -> io::Result<()> {
        ioctl_int(&self.file, EVIOCGRAB, 1).map(|_| ())
    }

    pub fn write_event(&self, ty: u16, code: u16, value: i32) -> io::Result<()> {
        let mut buf = Vec::with_capacity(EV_SIZE * 2);
        buf.extend_from_slice(&event_bytes(ty, code, value));
        buf.extend_from_slice(&event_bytes(EV_SYN, 0, 0));
        (&self.file).write_all(&buf)
    }
}

fn has_bit(bits: &[u8], n: u16) -> bool {
    bits.get(n as usize / 8).is_some_and(|b| b & (1 << (n % 8)) != 0)
}

/// The virtual keyboard everything is emitted through.
pub struct Uinput {
    file: File,
    buf: Vec<u8>,
}

#[repr(C)]
struct UinputSetup {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
    name: [u8; 80],
    ff_effects_max: u32,
}

impl Uinput {
    pub fn create() -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open("/dev/uinput")?;
        ioctl_int(&file, UI_SET_EVBIT, EV_KEY as i32)?;
        ioctl_int(&file, UI_SET_EVBIT, EV_LED as i32)?;
        // Keyboard keys only: skip the BTN_* ranges so nothing treats us as a mouse or joystick.
        for code in (1..0x100).chain(0x160..0x2c0) {
            ioctl_int(&file, UI_SET_KEYBIT, code)?;
        }
        for led in 0..=10 {
            ioctl_int(&file, UI_SET_LEDBIT, led)?;
        }
        let mut setup = UinputSetup {
            bustype: 0x06, // BUS_VIRTUAL
            vendor: OUR_VENDOR,
            product: OUR_PRODUCT,
            version: 1,
            name: [0; 80],
            ff_effects_max: 0,
        };
        setup.name[..OUR_NAME.len()].copy_from_slice(OUR_NAME.as_bytes());
        ioctl(&file, UI_DEV_SETUP, &mut setup)?;
        ioctl_int(&file, UI_DEV_CREATE, 0)?;
        Ok(Uinput { file, buf: Vec::with_capacity(EV_SIZE * 32) })
    }

    /// A second handle for reading LED state written to the virtual keyboard.
    pub fn reader(&self) -> io::Result<File> {
        self.file.try_clone()
    }

    /// Write all buffered events in one syscall.
    pub fn flush(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let r = (&self.file).write_all(&self.buf);
        self.buf.clear();
        r
    }
}

impl Sink for Uinput {
    fn key(&mut self, code: u16, value: i32) {
        // A SYN after every key event: each key is its own frame, so ordering is preserved.
        self.buf.extend_from_slice(&event_bytes(EV_KEY, code, value));
        self.buf.extend_from_slice(&event_bytes(EV_SYN, 0, 0));
    }
}

impl Drop for Uinput {
    fn drop(&mut self) {
        let _ = ioctl_int(&self.file, UI_DEV_DESTROY, 0);
    }
}
