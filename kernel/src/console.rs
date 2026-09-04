//! Kernel diagnostic console (D11).
//!
//! The kernel's diagnostics are its only speech. Until now they went straight
//! to COM1, so a machine with no 16550 -- most machines built in the last
//! decade -- boots mute: a successful boot and a hang look identical. This
//! module puts a backend behind the diagnostics, chosen once at boot from what
//! the hardware actually has: serial when a UART is present, the framebuffer
//! otherwise.
//!
//! It is diagnostics only, and that boundary is the whole licence for the
//! kernel touching a pixel at all (Design/real_hardware.md, D11 ruling). It
//! serves no tenant, exposes no syscall, and has no ring-3 caller. A tenant
//! that wants text uses libgfx, exactly as now. The framebuffer backend obeys
//! two limits drawn straight from that ruling's budget:
//!
//!   * **It owns no region a tenant holds.** It draws boot diagnostics until
//!     the framebuffer is first handed to a tenant (`freeze_framebuffer`), then
//!     goes silent for normal output. A one-way latch, not per-grant tracking:
//!     once the screen is a tenant resource the diagnostic console stops
//!     drawing to it during ordinary operation.
//!   * **It may take the screen back to say why the machine died.** The
//!     terminal path (panic, kernel fault, double fault) ignores the latch,
//!     seizes the screen once, and reports -- dying visibly beats dying
//!     silently on the one machine where silence is indistinguishable from a
//!     hang. Nothing short of a crash uses this path.
//!
//! Lock-freedom is a hard requirement, not a nicety: the fault, double-fault,
//! and panic handlers write here with the big kernel lock possibly held, so no
//! writer may block. Backend selection, the cursor, and the seize flag all live
//! in atomics; the framebuffer surface is a write-once `Once`; and each fb
//! write rebuilds a short-lived `FbConsole` rather than locking a shared one.
//! The serial backend keeps its old contract of a fresh port handle per writer.

use crate::fbcon::{FbConsole, Surface};
use crate::serial;
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use spin::Once;
use uart_16550::SerialPort;

const BACKEND_SERIAL: u8 = 0;
const BACKEND_FRAMEBUFFER: u8 = 1;

/// The backend chosen by `select`. Defaults to serial so any diagnostic
/// emitted before selection still has somewhere to go, and so a machine that
/// does have a UART behaves identically whether or not `select` has run yet.
static BACKEND: AtomicU8 = AtomicU8::new(BACKEND_SERIAL);

/// The mapped framebuffer, geometry only (base held as a `usize` so the global
/// stays `Send + Sync` without wrapping a raw pointer). Written once, in
/// `attach_framebuffer`, before any concurrent use.
static FB: Once<GlobalSurface> = Once::new();

/// False once the framebuffer has been handed to a tenant. Normal fb writes
/// stop; the terminal path ignores it.
static FB_ACTIVE: AtomicBool = AtomicBool::new(true);

/// Set the first time the terminal path draws, so the screen is cleared exactly
/// once no matter how many fault/panic lines follow.
static FB_SEIZED: AtomicBool = AtomicBool::new(false);

/// The normal-path cursor, carried between writes.
static NORM_CX: AtomicU32 = AtomicU32::new(0);
static NORM_CY: AtomicU32 = AtomicU32::new(0);

/// The terminal-path cursor, kept separate so a crash mid-boot resumes from a
/// cleared screen rather than wherever normal output happened to leave off.
static FORCE_CX: AtomicU32 = AtomicU32::new(0);
static FORCE_CY: AtomicU32 = AtomicU32::new(0);

#[derive(Clone, Copy)]
struct GlobalSurface {
    base: usize,
    width: u32,
    height: u32,
    stride: u32,
    bytes_per_pixel: u8,
    format: u8,
}

impl GlobalSurface {
    fn surface(&self) -> Surface {
        Surface {
            base: self.base as *mut u8,
            width: self.width,
            height: self.height,
            stride: self.stride,
            bytes_per_pixel: self.bytes_per_pixel,
            format: self.format,
        }
    }
}

/// Choose the diagnostic backend from hardware presence. Call once, early in
/// boot. Idempotent and lock-free; it only probes a port and stores a byte.
pub fn select() {
    // The fbcon_shell / fbcon_panic lanes simulate a no-serial machine: force the
    // framebuffer diagnostic backend even though QEMU provides a UART, so the paths
    // a serial-less machine takes -- the fbcon->tenant handoff (fbcon_shell) and the
    // terminal seize-and-draw on a crash (fbcon_panic) -- are actually exercised
    // under QEMU. Each lane's proof still leaves the guest over a *direct* serial
    // handle (see main.rs), which the harness reads; the `||` short-circuits
    // `probe()` away only when a feature is on, so the default build still selects
    // from real hardware presence.
    let backend = if cfg!(feature = "fbcon_shell") || cfg!(feature = "fbcon_panic") || !serial::probe() {
        BACKEND_FRAMEBUFFER
    } else {
        BACKEND_SERIAL
    };
    BACKEND.store(backend, Ordering::Relaxed);
}

/// Record the mapped framebuffer so the fb backend can draw. Called from
/// `framebuffer::init` once the GOP region is mapped, before any tenant grant.
/// On a machine using the fb backend it also blanks the screen so diagnostics
/// start from a known state; on a serial machine it stores the geometry and
/// draws nothing, leaving the screen untouched for tenants.
pub fn attach_framebuffer(surface: Surface) {
    FB.call_once(|| GlobalSurface {
        base: surface.base as usize,
        width: surface.width,
        height: surface.height,
        stride: surface.stride,
        bytes_per_pixel: surface.bytes_per_pixel,
        format: surface.format,
    });
    if BACKEND.load(Ordering::Relaxed) == BACKEND_FRAMEBUFFER {
        if let Some(gs) = FB.get() {
            FbConsole::with_cursor(gs.surface(), 0, 0).clear();
        }
        NORM_CX.store(0, Ordering::Relaxed);
        NORM_CY.store(0, Ordering::Relaxed);
    }
}

/// Stop the normal fb path from drawing: the framebuffer has become a tenant's
/// to hold. Idempotent; the terminal path is unaffected.
///
/// Called only from the userspace demo path, which the test build compiles out;
/// hence unused there.
#[cfg_attr(feature = "tests", allow(dead_code))]
pub fn freeze_framebuffer() {
    FB_ACTIVE.store(false, Ordering::Relaxed);
}

/// A fresh, lock-free writer for ordinary diagnostics. The fb backend draws
/// only while the framebuffer is still the kernel's to use.
pub fn writer() -> Console {
    match BACKEND.load(Ordering::Relaxed) {
        BACKEND_FRAMEBUFFER => Console::FbNormal,
        _ => Console::Serial(serial::init()),
    }
}

/// A writer for terminal states -- panic, kernel fault, double fault. On the fb
/// backend it seizes the screen (clearing once) and reports regardless of
/// whether a tenant held the framebuffer, because the machine is going down and
/// a silent death is indistinguishable from a hang. Serial is unchanged.
pub fn terminal_writer() -> Console {
    match BACKEND.load(Ordering::Relaxed) {
        BACKEND_FRAMEBUFFER => Console::FbTerminal,
        _ => Console::Serial(serial::init()),
    }
}

/// A diagnostic sink. `Serial` owns its port for the lifetime of one writer so
/// a multi-line report reuses a single handle, byte-for-byte as the pre-console
/// code did. The framebuffer variants hold nothing: each `write_str` rebuilds a
/// short-lived `FbConsole` from the global cursor, so no writer holds a lock.
pub enum Console {
    Serial(SerialPort),
    FbNormal,
    FbTerminal,
}

impl Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        match self {
            Console::Serial(port) => port.write_str(s),
            Console::FbNormal => {
                fb_normal_write(s);
                Ok(())
            }
            Console::FbTerminal => {
                fb_terminal_write(s);
                Ok(())
            }
        }
    }
}

/// Draw ordinary diagnostics, but only while the framebuffer is the kernel's.
fn fb_normal_write(s: &str) {
    if !FB_ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    let Some(gs) = FB.get() else {
        // No framebuffer mapped yet: early boot on a serial-less machine is
        // dark until the GOP is up. Disclosed, not fixable here.
        return;
    };
    let mut con = FbConsole::with_cursor(
        gs.surface(),
        NORM_CX.load(Ordering::Relaxed),
        NORM_CY.load(Ordering::Relaxed),
    );
    let _ = con.write_str(s);
    let (cx, cy) = con.cursor();
    NORM_CX.store(cx, Ordering::Relaxed);
    NORM_CY.store(cy, Ordering::Relaxed);
}

/// Draw a terminal report, seizing the screen on first use.
fn fb_terminal_write(s: &str) {
    let Some(gs) = FB.get() else {
        return;
    };
    // Clear exactly once across a whole crash, however many lines it prints.
    if !FB_SEIZED.swap(true, Ordering::AcqRel) {
        FbConsole::with_cursor(gs.surface(), 0, 0).clear();
        FORCE_CX.store(0, Ordering::Relaxed);
        FORCE_CY.store(0, Ordering::Relaxed);
    }
    let mut con = FbConsole::with_cursor(
        gs.surface(),
        FORCE_CX.load(Ordering::Relaxed),
        FORCE_CY.load(Ordering::Relaxed),
    );
    let _ = con.write_str(s);
    let (cx, cy) = con.cursor();
    FORCE_CX.store(cx, Ordering::Relaxed);
    FORCE_CY.store(cy, Ordering::Relaxed);
}

/// Forced-console self-test (D11 ruling; `force_console` feature). Draws a known
/// string to the *real* mapped framebuffer through the blitter and returns an
/// FNV-1a hash of a fixed origin square, so `xtask console` can assert the
/// no-serial rendering path end to end under QEMU without disturbing the serial
/// harness. Returns `None` if no framebuffer was discovered.
#[cfg(feature = "force_console")]
pub fn self_test_hash() -> Option<u64> {
    let gs = FB.get()?;
    let surface = gs.surface();
    let mut con = FbConsole::with_cursor(surface, 0, 0);
    con.clear();
    // Wraps at 8 columns, so this also drives the wrap path on the real fb.
    let _ = con.write_str("PLINTH CONSOLE OK 0123456789");
    Some(hash_origin_square(&surface, 128))
}

/// Read back an FNV-1a hash of the current framebuffer origin square WITHOUT
/// drawing anything (`fbcon_shell` lane, first-metal-boot D4). After a tenant has
/// seized the framebuffer the console was drawing to, this reports what is now on
/// screen so the harness can confirm the tenant's pixels (not leftover console
/// text) landed there -- the fbcon->tenant handoff. Returns `None` if no
/// framebuffer was discovered.
#[cfg(any(feature = "fbcon_shell", feature = "fbcon_panic"))]
pub fn origin_square_hash(side: u32) -> Option<u64> {
    let gs = FB.get()?;
    Some(hash_origin_square(&gs.surface(), side))
}

/// FNV-1a over the top-left `side` x `side` pixels, read back from the mapped
/// framebuffer. A fixed square keeps the value independent of the panel's total
/// resolution, exactly as the gfx demos hash theirs.
#[cfg(any(feature = "force_console", feature = "fbcon_shell", feature = "fbcon_panic"))]
fn hash_origin_square(surface: &Surface, side: u32) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let bpp = surface.bytes_per_pixel as usize;
    let w = side.min(surface.width);
    let h = side.min(surface.height);
    let mut hash = FNV_OFFSET;
    for y in 0..h {
        for x in 0..w {
            let offset = (y as usize * surface.stride as usize + x as usize) * bpp;
            for i in 0..bpp {
                // SAFETY: offset + i < stride*height*bpp, the mapped fb length.
                let byte = unsafe { surface.base.add(offset + i).read_volatile() };
                hash ^= byte as u64;
                hash = hash.wrapping_mul(FNV_PRIME);
            }
        }
    }
    hash
}
