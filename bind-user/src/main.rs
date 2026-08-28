//! Direct-binding slice 4 demo: a library OS writes its OWN virtqueue descriptors.
//!
//! Slice 3 had the kernel write the descriptor and the libOS only ring + drain.
//! Here the kernel is off the submit path entirely: `bind_device` maps the notify
//! page, the desc/avail/used rings, the header/status buffer, and a data buffer,
//! and returns the IOVAs (`data_iova`, `buf_iova`) to name. This process builds the
//! descriptor chain itself, publishes it, rings the doorbell, drains the used ring,
//! and verifies the read. The IOMMU is the whole defense -- a descriptor naming an
//! IOVA the device's domain does not map faults (the negative check below).

#![no_std]
#![no_main]

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

use libos::ring;
use libplinth::{sys_cap_release, sys_exit, sys_write, BIND_SLOT, MAP_BASE};

/// The 6-page window bind_device maps into us.
const BASE: u64 = MAP_BASE + 0x8000;
const NOTIFY: u64 = BASE;
const DESC: u64 = BASE + 0x1000;
const AVAIL: u64 = BASE + 0x2000;
const USED: u64 = BASE + 0x3000;
const BUF: u64 = BASE + 0x4000;
const DATA: u64 = BASE + 0x5000;

/// Our chosen sub-layout within the buffer page (the libOS owns it, so any
/// non-overlapping offsets work): request header at +0, status byte at +64.
const HDR_OFF: u64 = 0;
const STATUS_OFF: u64 = 64;

// virtio split-ring descriptor flags and the virtio-blk read request type.
const F_NEXT: u16 = 1;
const F_WRITE: u16 = 2;
const AVAIL_NO_INTERRUPT: u16 = 1;
const T_IN: u32 = 0;
const BLK_OK: u8 = 0;

/// Write one 16-byte split-ring descriptor at `at`: addr, len, flags, next.
/// SAFETY: `at` is inside our mapped, writable desc-ring page.
unsafe fn write_desc(at: u64, addr: u64, len: u32, flags: u16, next: u16) {
    write_volatile(at as *mut u64, addr);
    write_volatile((at + 8) as *mut u32, len);
    write_volatile((at + 12) as *mut u16, flags);
    write_volatile((at + 14) as *mut u16, next);
}

/// Submit a one-sector read of `sector` into `data_addr` (an IOVA) using chain
/// head 0, ring the doorbell, and wait for the used ring to advance. Returns the
/// status byte the device wrote. The whole submit is our own writes -- no syscall.
/// SAFETY: all VAs are pages bind_device mapped for us; `qsize` is the ring size.
unsafe fn submit_read(qsize: u16, buf_iova: u64, data_addr: u64, sector: u64) -> u8 {
    // Request header + status sentinel in our buffer page.
    write_volatile((BUF + HDR_OFF) as *mut u32, T_IN);
    write_volatile((BUF + HDR_OFF + 4) as *mut u32, 0);
    write_volatile((BUF + HDR_OFF + 8) as *mut u64, sector);
    write_volatile((BUF + STATUS_OFF) as *mut u8, 0xFF);

    // Descriptor chain at head 0: hdr (device-read) -> data (device-write) ->
    // status (device-write). Addresses are IOVAs the device's domain resolves.
    write_desc(DESC, buf_iova + HDR_OFF, 16, F_NEXT, 1);
    write_desc(DESC + 16, data_addr, 512, F_NEXT | F_WRITE, 2);
    write_desc(DESC + 32, buf_iova + STATUS_OFF, 1, F_WRITE, 0);

    // Read the used index before we ring, so we can wait for exactly one advance.
    let used_before = read_volatile((USED + 2) as *const u16);

    // Publish head 0 in the avail ring, ordered before the doorbell.
    write_volatile(AVAIL as *mut u16, AVAIL_NO_INTERRUPT);
    let idx = read_volatile((AVAIL + 2) as *const u16);
    let ring_slot = (idx % qsize) as u64;
    write_volatile((AVAIL + 4 + ring_slot * 2) as *mut u16, 0u16);
    fence(Ordering::SeqCst);
    write_volatile((AVAIL + 2) as *mut u16, idx.wrapping_add(1));
    fence(Ordering::SeqCst);

    // Ring the doorbell (queue index 0) with a plain MMIO store.
    write_volatile(NOTIFY as *mut u16, 0u16);

    // Drain: poll the used ring in our own mapping until the device advances it.
    let mut spins = 0u64;
    while read_volatile((USED + 2) as *const u16) == used_before {
        spins += 1;
        if spins >= 200_000_000 {
            return 0xFE; // timed out -- report a non-OK status
        }
        core::hint::spin_loop();
    }
    read_volatile((BUF + STATUS_OFF) as *const u8)
}

/// True if the 512-byte data buffer holds the sector-0 ramp (byte i == i % 256).
/// SAFETY: DATA is our mapped data page.
unsafe fn data_is_ramp() -> bool {
    let mut i = 0u64;
    while i < 512 {
        if read_volatile((DATA + i) as *const u8) != (i % 256) as u8 {
            return false;
        }
        i += 1;
    }
    true
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    sys_write(b"bind: ring 3\n");

    // Bind through the reference executor (D9): init_bound issues bind_device,
    // mapping the six-page window at BASE, and hands back [qsize, data_iova,
    // buf_iova] for the manual negative probe below to reuse the same mapping.
    let mut info = [0u64; 3]; // qsize, data_iova, buf_iova
    if !ring::init_bound(BIND_SLOT, BASE, &mut info) {
        fail(b"bind: init_bound failed\n");
    }
    let qsize = info[0] as u16;
    let buf_iova = info[2];
    if qsize == 0 {
        fail(b"bind: queue size zero\n");
    }

    // Positive: FOUR overlapping reads through the reference executor over the bound
    // device -- the D9 payoff (the same join/overlap machinery asyncblk uses, now
    // over a directly-bound device, with the kernel off the submit path). Each read
    // lands in its own reactor-owned sub-buffer; a wrong slot->buffer route shows up
    // as a sector's ramp in the wrong place.
    const N: usize = 4;
    let r0 = ring::read_bound(0);
    let r1 = ring::read_bound(1);
    let r2 = ring::read_bound(2);
    let r3 = ring::read_bound(3);
    // Capture each read's landing buffer before join takes ownership of the reads.
    let vas = [r0.data_va(), r1.data_va(), r2.data_va(), r3.data_va()];
    let status = ring::block_on(ring::join([r0, r1, r2, r3]));

    // Every read OK, and each buffer holds ITS sector's ramp: byte j of sector s is
    // (s + j) & 0xFF (the bind image's per-sector ramp), so no completion was routed
    // to the wrong slot.
    let mut ok = true;
    let mut s = 0usize;
    while s < N {
        if status[s] as u8 != BLK_OK {
            ok = false;
        }
        for &j in &[0u64, 1, 7, 511] {
            let expect = ((s as u64 + j) & 0xFF) as u8;
            // SAFETY: vas[s] is this read's mapped sub-buffer; the device DMA'd its
            // sector in.
            let got = unsafe { read_volatile((vas[s] + j) as *const u8) };
            if got != expect {
                ok = false;
            }
        }
        s += 1;
    }
    if !ok {
        fail(b"bind: executor overlapping reads did not each deliver their sector\n");
    }
    sys_write(b"bind: executor ran 4 overlapping reads over the bound device, each verified\n");

    // Negative: name an IOVA the device's domain does NOT map. The IOMMU must
    // confine the device, so the read cannot land in our buffer. The request may
    // still "complete" -- QEMU writes the mapped status byte even though the data
    // DMA faulted, so the used ring advances -- which is exactly why the userspace
    // signal is "the data never arrived" and the kernel confirms the hardware
    // fault from the fault-recording register after we exit.
    const POISON_IOVA: u64 = 0xFFFF_F000; // far above the domain's mapped window
    // SAFETY: DATA is our mapped page; the rings/buffer likewise.
    unsafe {
        // Zero the buffer so a stale ramp from the positive read cannot masquerade
        // as a delivered read.
        let mut i = 0u64;
        while i < 512 {
            write_volatile((DATA + i) as *mut u8, 0u8);
            i += 1;
        }
        let _ = submit_read(qsize, buf_iova, POISON_IOVA, 0);
        if data_is_ramp() {
            fail(b"bind: out-of-domain read was NOT confined (data leaked in)\n");
        }
    }
    sys_write(b"bind: out-of-domain read confined (libos named an unmapped iova)\n");

    // Release the binding explicitly (direct-binding slice 5, D7/I11): dropping the
    // BoundDevice capability IS its teardown -- the kernel resets the device, frees
    // its domain and buffers, and returns it to the unbound pool.
    if sys_cap_release(BIND_SLOT) != 0 {
        fail(b"bind: cap_release failed\n");
    }
    sys_write(b"bind: released the device (binding torn down)\n");

    sys_exit(0)
}

fn fail(msg: &[u8]) -> ! {
    sys_write(msg);
    sys_exit(1)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
