//! Direct-binding D9 demo: the reference async executor drives a directly-bound
//! device end to end -- both the positive and the negative through the SAME executor.
//!
//! `bind_device` maps the notify page, the desc/avail/used rings, and the header/
//! status + data buffers into this process, and returns the IOVAs its IOMMU domain
//! resolves (the libOS names IOVAs, never physical addresses -- D1/I5). The reference
//! executor (`libos::ring`) then writes its own descriptors, rings the doorbell, and
//! drains the used ring, with the kernel off the submit path.
//!
//! - Positive (slice 4): four overlapping `read_bound`s through `join`, each verified
//!   against its own sector's ramp -- the D9 payoff, the same overlap machinery
//!   `asyncblk` uses, now over a directly-bound device.
//! - Negative (slice 5): one `read_bound_poison` through the same executor names an
//!   out-of-domain IOVA. The IOMMU confines the device, so the sector never lands in
//!   the reactor's buffer. This closes the milestone: the whole demo runs through the
//!   executor -- no hand-written descriptor chain remains -- and the IOMMU defense is
//!   proven on the executor's own read path, not assumed.

#![no_std]
#![no_main]

use core::ptr::{read_volatile, write_volatile};

use libos::ring;
use libplinth::{sys_cap_release, sys_exit, sys_write, BIND_SLOT, MAP_BASE};

/// The base of the 6-page window `bind_device` maps into us (notify, desc, avail,
/// used, buf, data). The executor owns the layout above this; we only name the base.
const BASE: u64 = MAP_BASE + 0x8000;

/// virtio-blk OK status, checked on the positive reads.
const BLK_OK: u8 = 0;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    sys_write(b"bind: ring 3\n");

    // Bind through the reference executor (D9): init_bound issues bind_device,
    // mapping the six-page window at BASE and recording the geometry the reactor
    // needs to write its own descriptors. `info` (qsize, data_iova, buf_iova) is the
    // same geometry the reactor keeps; we do not need it here now that both the
    // positive and the negative go through the executor.
    let mut info = [0u64; 3];
    if !ring::init_bound(BIND_SLOT, BASE, &mut info) {
        fail(b"bind: init_bound failed\n");
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

    // Negative, carried up through the executor (slice 5): a read driven by the SAME
    // reference executor, but naming an IOVA the device's domain does NOT map. The
    // IOMMU must confine the device, so the sector cannot land in the reactor's
    // buffer. The request may still "complete" -- QEMU writes the mapped status byte
    // even though the data DMA faulted, so the used ring advances and the executor's
    // wait returns -- which is exactly why the userspace signal is "the data never
    // arrived," and the kernel confirms the hardware fault from the fault-recording
    // register after we exit. This is slice 4's forced-fault proof inherited by the
    // executor's own read path rather than a hand-written descriptor chain.
    let poison = ring::read_bound_poison(0);
    let pva = poison.data_va();
    // Zero this read's landing sub-buffer first, so a stale ramp from the positive
    // pass (slot reuse) cannot masquerade as a delivered read.
    // SAFETY: pva is the reactor's mapped per-slot data sub-buffer for this read.
    unsafe {
        let mut i = 0u64;
        while i < 512 {
            write_volatile((pva + i) as *mut u8, 0u8);
            i += 1;
        }
    }
    let _ = ring::block_on(poison);
    // Confined: the sector must not have arrived. Sample bytes whose ramp value is
    // nonzero (byte 0 of sector 0 is 0 either way, so it cannot witness a leak).
    // SAFETY: pva is this read's mapped sub-buffer.
    let leaked = unsafe {
        let mut hit = false;
        for &j in &[1u64, 7, 255, 511] {
            if read_volatile((pva + j) as *const u8) != 0 {
                hit = true;
            }
        }
        hit
    };
    if leaked {
        fail(b"bind: out-of-domain executor read was NOT confined (data leaked in)\n");
    }
    sys_write(b"bind: out-of-domain executor read confined (named an unmapped iova)\n");

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
