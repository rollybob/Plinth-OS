//! Minimal PCI configuration-space access and device discovery.
//!
//! Stage 1 of storage bring-up. Plinth boots on QEMU q35, where "virtio" means
//! virtio-pci: virtio-mmio is an arm / `microvm` transport that does not use
//! the UEFI boot path Plinth relies on (see Design/filesystem.md, D1). Finding
//! the device needs no new mechanism -- the legacy 0xCF8/0xCFC configuration
//! ports are port I/O, exactly what the PIC/PIT driver already does. ACPI MCFG
//! / ECAM parsing is a later refinement, not needed to locate one device on
//! bus 0.
//!
//! This module is pure discovery: configuration-space reads only, no MMIO
//! mapping and no DMA. Mapping the device's BAR and driving its virtqueue are
//! the next milestones; the virtio capability records printed here are exactly
//! the map those steps consume.

use core::fmt::Write;

use x86_64::instructions::port::Port;

/// Legacy PCI configuration mechanism (CAM): write a CONFIG_ADDRESS dword
/// selecting bus/slot/func/register, then read or write the data at
/// CONFIG_DATA. The two ports are fixed legacy I/O addresses.
const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

/// virtio's PCI vendor id. The block device is virtio device type 2, so its
/// modern PCI device id is 0x1040 + 2 = 0x1042; the legacy/transitional id is
/// 0x1001. We accept either when scanning.
pub const VIRTIO_VENDOR: u16 = 0x1AF4;
const VIRTIO_BLK_MODERN: u16 = 0x1042;
const VIRTIO_BLK_LEGACY: u16 = 0x1001;

/// PCI capability id for a vendor-specific capability (0x09) -- how a modern
/// virtio device points at its structures inside a BAR.
const CAP_ID_VENDOR: u8 = 0x09;

/// virtio `cfg_type` values (virtio 1.x spec, 4.1.4): which structure a given
/// vendor capability describes.
const VIRTIO_CAP_COMMON: u8 = 1;
const VIRTIO_CAP_NOTIFY: u8 = 2;
const VIRTIO_CAP_ISR: u8 = 3;
const VIRTIO_CAP_DEVICE: u8 = 4;

/// Where a device sits in the PCI topology. func is kept for completeness even
/// though virtio-blk is single-function.
#[derive(Clone, Copy)]
pub struct Location {
    pub bus: u8,
    pub slot: u8,
    pub func: u8,
}

/// One virtio structure's location: which BAR it lives in and the byte range
/// within that BAR. The driver maps the BAR and indexes by `offset`.
#[derive(Clone, Copy, Default)]
pub struct VirtioCfg {
    pub bar: u8,
    pub offset: u32,
    pub length: u32,
}

/// The full map of a discovered virtio-blk device: where it sits, and where its
/// four modern structures live. This is exactly what `virtio_blk` consumes to
/// bring the device up.
#[derive(Clone, Copy)]
pub struct VirtioBlkInfo {
    pub loc: Location,
    pub device_id: u16,
    pub common: VirtioCfg,
    pub notify: VirtioCfg,
    /// notify_off_multiplier from the notify capability.
    pub notify_mult: u32,
    pub isr: VirtioCfg,
    pub device: VirtioCfg,
}

/// Read a BAR's base address, decoding I/O vs 32/64-bit memory BARs. `index`
/// is the BAR number (0..6); for a 64-bit BAR this reads both halves.
pub fn read_bar(loc: Location, index: u8) -> u64 {
    let off = 0x10 + index * 4;
    let lo = read32(loc.bus, loc.slot, loc.func, off);
    if lo & 1 == 1 {
        return (lo & 0xFFFF_FFFC) as u64; // I/O BAR
    }
    if (lo >> 1) & 0x3 == 0x2 {
        let hi = read32(loc.bus, loc.slot, loc.func, off + 4);
        ((hi as u64) << 32) | (lo & 0xFFFF_FFF0) as u64
    } else {
        (lo & 0xFFFF_FFF0) as u64
    }
}

/// Enable memory-space decoding and bus mastering in the command register --
/// required before the device can respond to MMIO and before it may DMA.
/// Enumeration needs neither; the virtqueue needs both.
pub fn enable_bus_master(loc: Location) {
    let dword = read32(loc.bus, loc.slot, loc.func, 0x04);
    // Set Memory Space Enable (bit 1) + Bus Master Enable (bit 2). Keep the
    // high half (status, write-1-to-clear) zero so we clear nothing.
    let cmd = (dword as u16) | (1 << 1) | (1 << 2);
    write32(loc.bus, loc.slot, loc.func, 0x04, cmd as u32);
}

/// Compose a CONFIG_ADDRESS value: enable bit (31), then bus/slot/func and the
/// dword-aligned register offset (low two bits ignored by the addressing).
fn address(bus: u8, slot: u8, func: u8, offset: u8) -> u32 {
    0x8000_0000
        | ((bus as u32) << 16)
        | ((slot as u32) << 11)
        | ((func as u32) << 8)
        | ((offset as u32) & 0xFC)
}

/// Read a 32-bit configuration register.
pub fn read32(bus: u8, slot: u8, func: u8, offset: u8) -> u32 {
    // SAFETY: 0xCF8/0xCFC are the fixed legacy PCI config ports. Selecting a
    // register and reading its dword is side-effect free on the device.
    unsafe {
        Port::<u32>::new(CONFIG_ADDRESS).write(address(bus, slot, func, offset));
        Port::<u32>::new(CONFIG_DATA).read()
    }
}

/// Write a 32-bit configuration register.
pub fn write32(bus: u8, slot: u8, func: u8, offset: u8, value: u32) {
    // SAFETY: 0xCF8/0xCFC are the fixed legacy PCI config ports. We only ever
    // write the command register (to enable MMIO + bus mastering), a defined
    // configuration operation.
    unsafe {
        Port::<u32>::new(CONFIG_ADDRESS).write(address(bus, slot, func, offset));
        Port::<u32>::new(CONFIG_DATA).write(value);
    }
}

/// Read a 16-bit field, extracted from its containing dword.
fn read16(bus: u8, slot: u8, func: u8, offset: u8) -> u16 {
    let dword = read32(bus, slot, func, offset);
    (dword >> ((offset as u32 & 2) * 8)) as u16
}

/// Read an 8-bit field, extracted from its containing dword.
fn read8(bus: u8, slot: u8, func: u8, offset: u8) -> u8 {
    let dword = read32(bus, slot, func, offset);
    (dword >> ((offset as u32 & 3) * 8)) as u8
}

/// Scan for the virtio-blk device. q35 places it on bus 0, but we sweep all
/// 256 buses x 32 slots anyway (cheap, and it avoids hard-coding placement),
/// function 0 only -- virtio-blk is not multifunction. Returns the first match.
pub fn find_virtio_blk() -> Option<Location> {
    for bus in 0u16..256 {
        let bus = bus as u8;
        for slot in 0u8..32 {
            if read16(bus, slot, 0, 0x00) != VIRTIO_VENDOR {
                continue;
            }
            let device = read16(bus, slot, 0, 0x02);
            if device == VIRTIO_BLK_MODERN || device == VIRTIO_BLK_LEGACY {
                return Some(Location { bus, slot, func: 0 });
            }
        }
    }
    None
}

/// Print the device's BARs, decoded. The next milestone maps one of these
/// (the modern device's MMIO BAR) into the kernel address space.
fn report_bars<W: Write>(out: &mut W, loc: Location) {
    let Location { bus, slot, func } = loc;
    let mut i = 0u8;
    while i < 6 {
        let off = 0x10 + i * 4;
        let bar = read32(bus, slot, func, off);
        if bar == 0 {
            i += 1;
            continue;
        }
        if bar & 1 == 1 {
            // I/O-space BAR (legacy virtio transport lives here).
            let port = bar & 0xFFFF_FFFC;
            let _ = writeln!(out, "plinth:   bar{i} io   port 0x{port:x}");
            i += 1;
        } else {
            let kind = (bar >> 1) & 0x3;
            let prefetch = (bar >> 3) & 1;
            if kind == 0x2 {
                // 64-bit memory BAR: the high half is the next dword.
                let hi = read32(bus, slot, func, off + 4);
                let addr = ((hi as u64) << 32) | (bar & 0xFFFF_FFF0) as u64;
                let _ = writeln!(out, "plinth:   bar{i} mem64 0x{addr:x} prefetch {prefetch}");
                i += 2;
            } else {
                let addr = (bar & 0xFFFF_FFF0) as u64;
                let _ = writeln!(out, "plinth:   bar{i} mem32 0x{addr:x} prefetch {prefetch}");
                i += 1;
            }
        }
    }
}

/// Walk the PCI capability list, filling in the virtio structure locations.
/// The list is a device-supplied linked structure, so the walk is bounded
/// against a malformed (or cyclic) chain.
fn fill_caps(info: &mut VirtioBlkInfo) {
    let Location { bus, slot, func } = info.loc;
    // Status register bit 4: the capability list is present.
    if read16(bus, slot, func, 0x06) & (1 << 4) == 0 {
        return;
    }
    let mut ptr = read8(bus, slot, func, 0x34) & 0xFC;
    let mut guard = 0;
    while ptr != 0 && guard < 48 {
        let cap_id = read8(bus, slot, func, ptr);
        let next = read8(bus, slot, func, ptr + 1) & 0xFC;
        if cap_id == CAP_ID_VENDOR {
            let cfg_type = read8(bus, slot, func, ptr + 3);
            let cfg = VirtioCfg {
                bar: read8(bus, slot, func, ptr + 4),
                offset: read32(bus, slot, func, ptr + 8),
                length: read32(bus, slot, func, ptr + 12),
            };
            match cfg_type {
                VIRTIO_CAP_COMMON => info.common = cfg,
                VIRTIO_CAP_NOTIFY => {
                    info.notify = cfg;
                    // The notify capability carries one extra dword past the
                    // 16-byte cap header: the notify_off_multiplier.
                    info.notify_mult = read32(bus, slot, func, ptr + 16);
                }
                VIRTIO_CAP_ISR => info.isr = cfg,
                VIRTIO_CAP_DEVICE => info.device = cfg,
                _ => {} // VIRTIO_CAP_PCI and any others: not needed here.
            }
        }
        ptr = next;
        guard += 1;
    }
}

/// Find the virtio-blk device and read its full structure map. Pure
/// config-space reads; no MMIO mapping or DMA.
pub fn discover() -> Option<VirtioBlkInfo> {
    let loc = find_virtio_blk()?;
    let mut info = VirtioBlkInfo {
        loc,
        device_id: read16(loc.bus, loc.slot, loc.func, 0x02),
        common: VirtioCfg::default(),
        notify: VirtioCfg::default(),
        notify_mult: 0,
        isr: VirtioCfg::default(),
        device: VirtioCfg::default(),
    };
    fill_caps(&mut info);
    Some(info)
}

/// Print a discovered device's BARs and virtio structure map.
fn report<W: Write>(out: &mut W, info: &VirtioBlkInfo) {
    let loc = info.loc;
    let kind = if info.device_id == VIRTIO_BLK_MODERN {
        "virtio-blk modern"
    } else {
        "virtio-blk legacy/transitional"
    };
    let _ = writeln!(
        out,
        "plinth: pci {:02x}:{:02x}.{} vendor {:04x} device {:04x} ({kind})",
        loc.bus, loc.slot, loc.func, VIRTIO_VENDOR, info.device_id
    );
    report_bars(out, loc);
    for (name, cfg) in [
        ("common", info.common),
        ("isr", info.isr),
        ("device", info.device),
        ("notify", info.notify),
    ] {
        let _ = writeln!(
            out,
            "plinth:   virtio cap {name:7} bar {} offset 0x{:x} len 0x{:x}",
            cfg.bar, cfg.offset, cfg.length
        );
    }
    let _ = writeln!(out, "plinth:   notify multiplier 0x{:x}", info.notify_mult);
}

/// Stage 1 discovery entry point: find virtio-blk and report what was found.
/// Returns the device map on success. Pure config-space reads; call once at
/// boot.
pub fn init<W: Write>(out: &mut W) -> Option<VirtioBlkInfo> {
    let _ = writeln!(out, "plinth: scanning PCI bus");
    match discover() {
        Some(info) => {
            report(out, &info);
            let loc = info.loc;
            let _ = writeln!(
                out,
                "plinth: virtio-blk found at {:02x}:{:02x}.{}",
                loc.bus, loc.slot, loc.func
            );
            Some(info)
        }
        None => {
            let _ = writeln!(out, "plinth: virtio-blk not found");
            None
        }
    }
}
