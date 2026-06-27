//! Framebuffer discovery (Design/display.md).
//!
//! The bootloader hands the kernel a UEFI GOP linear framebuffer in `BootInfo`
//! -- a memory-mapped pixel region with a base, geometry, and pixel format. The
//! kernel's whole job here is to *find* that region and capture its physical
//! base + geometry, so it can later hand the region to a graphics library OS as
//! a `Framebuffer` capability (`framebuffer_cap`, granted in main.rs; mapped via
//! the `fb_map` syscall). Drawing -- fonts, layout, the test pattern, the
//! determinism hash -- is all library-OS policy now; the kernel never touches a
//! pixel. (Stage 1 drew + hashed here as a bring-up scaffold; Stage 2 retired
//! that and moved it into `libgfx`, leaving only discovery.)

use bootloader_api::info::{FrameBuffer, PixelFormat};
use core::fmt::Write;
use spin::Mutex;
use uart_16550::SerialPort;

use crate::capability::CapObject;
use crate::memory;

/// Pixel-format codes shared with userspace (mirrored in libplinth as
/// FB_FMT_*). The kernel ships the raw format; the graphics libOS does the
/// channel-order arithmetic, exactly as it decodes raw scancodes.
const FMT_RGB: u8 = 0;
const FMT_BGR: u8 = 1;
const FMT_U8: u8 = 2;
const FMT_OTHER: u8 = 3;

/// The discovered framebuffer: physical base plus the geometry a holder needs to
/// draw. Captured once at boot; `None` if the bootloader gave us no framebuffer.
#[derive(Clone, Copy)]
pub struct FbRegion {
    pub phys_base: u64,
    pub width: u32,
    pub height: u32,
    /// Pixels per row (>= width); rows are `stride * bytes_per_pixel` bytes apart.
    pub stride: u32,
    pub bytes_per_pixel: u8,
    pub format: u8,
}

static REGION: Mutex<Option<FbRegion>> = Mutex::new(None);

/// Discover the framebuffer: capture its physical base + geometry and report it.
/// Draws nothing -- the graphics libOS owns pixels.
pub fn init(serial: &mut SerialPort, fb: Option<&mut FrameBuffer>) {
    let Some(fb) = fb else {
        // No display adapter / GOP. The smoke asserts "framebuffer present", so
        // this turns a missing framebuffer into a visible failure rather than a
        // silent skip (it means the QEMU -vga pin regressed).
        let _ = writeln!(serial, "plinth: framebuffer absent");
        return;
    };

    let info = fb.info();
    // The bootloader mapped the framebuffer at this kernel virtual address;
    // translate it to the physical base so the region can be re-mapped into a
    // user address space later. GOP framebuffers are physically contiguous, so
    // base + byte_len covers the whole region.
    let va = fb.buffer_mut().as_ptr() as u64;
    let Some(phys_base) = memory::kernel_phys_of(va) else {
        let _ = writeln!(serial, "plinth: framebuffer present but not mapped");
        return;
    };

    let region = FbRegion {
        phys_base,
        width: info.width as u32,
        height: info.height as u32,
        stride: info.stride as u32,
        bytes_per_pixel: info.bytes_per_pixel as u8,
        format: fmt_code(info.pixel_format),
    };
    *REGION.lock() = Some(region);

    let _ = writeln!(serial, "plinth: framebuffer present");
    // Geometry detail, deliberately NOT asserted by the smoke: the resolution
    // can shift across QEMU versions, like the PCI BAR / LAPIC-base lines.
    let _ = writeln!(
        serial,
        "plinth: framebuffer {}x{} stride {} bpp {} fmt {}",
        info.width,
        info.height,
        info.stride,
        info.bytes_per_pixel,
        fmt_name(info.pixel_format),
    );
}

/// The framebuffer as a capability object (whole screen), or `None` if no
/// framebuffer was discovered. main.rs mints this into the graphics libOS.
pub fn framebuffer_cap() -> Option<CapObject> {
    let r = (*REGION.lock())?;
    Some(CapObject::Framebuffer {
        phys_base: r.phys_base,
        width: r.width,
        height: r.height,
        stride: r.stride,
        bytes_per_pixel: r.bytes_per_pixel,
        format: r.format,
    })
}

/// The discovered framebuffer region, if any. main.rs reads it to size sub-region
/// (band) grants.
pub fn region() -> Option<FbRegion> {
    *REGION.lock()
}

/// A capability for a horizontal BAND of the framebuffer -- `rows` rows starting
/// at row `y0` (Design/display.md Stage 4). The band is the same `Framebuffer`
/// variant with the base offset to the band's first row and the height reduced;
/// `stride` (and thus the full row pitch) is unchanged, so a band is a contiguous
/// physical sub-range, disjoint from every other band. fb_map maps exactly the
/// band -- a holder's address space contains only its rows, so the boundary
/// between two band holders is enforced by paging, not a cooperative check (the
/// display analogue of disjoint `BlockRange`s). Returns `None` if no framebuffer
/// exists or the band runs past the screen.
pub fn framebuffer_cap_band(y0: u32, rows: u32) -> Option<CapObject> {
    let r = (*REGION.lock())?;
    if rows == 0 || y0.checked_add(rows)? > r.height {
        return None;
    }
    let row_bytes = r.stride as u64 * r.bytes_per_pixel as u64;
    Some(CapObject::Framebuffer {
        phys_base: r.phys_base + y0 as u64 * row_bytes,
        width: r.width,
        height: rows,
        stride: r.stride,
        bytes_per_pixel: r.bytes_per_pixel,
        format: r.format,
    })
}

fn fmt_code(format: PixelFormat) -> u8 {
    match format {
        PixelFormat::Rgb => FMT_RGB,
        PixelFormat::Bgr => FMT_BGR,
        PixelFormat::U8 => FMT_U8,
        _ => FMT_OTHER,
    }
}

fn fmt_name(format: PixelFormat) -> &'static str {
    match format {
        PixelFormat::Rgb => "rgb",
        PixelFormat::Bgr => "bgr",
        PixelFormat::U8 => "u8",
        _ => "other",
    }
}
