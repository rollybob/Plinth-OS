//! Unified-loop demo (Stage 4): one ring multiplexes block I/O and input.
//!
//! The payoff of carrying both on one ring (Design/event_rings.md s10): this
//! libOS registers a SINGLE ring, issues a block read AND opens a keyboard event
//! subscription on it, and drives both to completion in ONE `block_on` /
//! `ring_wait` loop. A real OS's event loop is exactly this shape -- wait on disk
//! and input together -- and here it is one `ring_wait`, the kernel demuxing each
//! completion back to its future (the read) or stream (the events) by `user_data`.
//! `join2` polls both each wake, so the read and the keystrokes interleave in
//! whatever order the device and the synthetic scaffold produce them.
//!
//! The kernel grants two capabilities: a BlockRange over device 0 (slot 1) and a
//! keyboard EventSource (slot 2). Correctness is asserted, never transcript-
//! matched: the read must land sector 0's ramp bytes, and each event must arrive
//! in order with its scancode. A scripted scancode sequence drives the input side
//! in headless smoke; a real keyboard would otherwise.

#![no_std]
#![no_main]

use libos::ring;
use libplinth::{
    event_code, event_kind, sys_exit, sys_frame_alloc, sys_frame_map, sys_write, write_dec,
    BLK_OK, BLOCK_SLOT, EVENT_KEY, MAP_BASE, SYS_ERR,
};

/// Events to collect from the keyboard stream. Must match the synthetic sequence
/// main.rs arms via `input::arm_synthetic`.
const N_EVENTS: usize = 3;

/// The scripted scancodes, in order: Set-1 make codes for 'x','y','z'. Must match
/// main.rs.
const SEQUENCE: [u16; N_EVENTS] = [0x2D, 0x15, 0x2C];

/// The EventSource is the SECOND grant, so it lands one slot after the BlockRange:
/// the kernel mints grants in order after the CPU budget (BlockRange at slot 1 =
/// BLOCK_SLOT, EventSource at slot 2).
const EVENT_SLOT: u64 = BLOCK_SLOT + 1;

/// Offsets within the read sector to spot-check against the ramp.
const PROBES: [usize; 4] = [0, 1, 7, 511];

#[no_mangle]
pub extern "C" fn _start(_id: u64) -> ! {
    sys_write(b"unified: start\n");

    if !ring::init() {
        sys_write(b"unified: ring init failed\n");
        sys_exit(1);
    }

    // One I/O frame for the block read, mapped at MAP_BASE (clear of the ring
    // frames, which sit at the top of the map window).
    let frame = sys_frame_alloc();
    if frame == SYS_ERR || sys_frame_map(frame, MAP_BASE) == SYS_ERR {
        sys_exit(2);
    }

    // Open the keyboard subscription, then join a block read with collecting
    // N events. Both ride the one ring; block_on drives them through a single
    // ring_wait loop, reaping disk completions and key events alike.
    let mut stream = ring::subscribe(EVENT_SLOT);
    let (read_status, events) = ring::block_on(ring::join2(
        ring::read(BLOCK_SLOT, frame, 0, 1),
        stream.collect::<N_EVENTS>(),
    ));

    // Assert: the read succeeded and landed sector 0's ramp bytes (ramp byte j of
    // relative sector i is (i + j) & 0xFF; here i = 0), and each event arrived in
    // order with its scancode.
    let mut ok = read_status == BLK_OK;
    let buf = MAP_BASE as *const u8;
    for &j in PROBES.iter() {
        let expect = (j & 0xFF) as u8;
        // SAFETY: frame is mapped at MAP_BASE and the device DMA'd a sector in.
        let got = unsafe { buf.add(j).read_volatile() };
        if got != expect {
            ok = false;
        }
    }
    for i in 0..N_EVENTS {
        if event_kind(events[i]) != EVENT_KEY || event_code(events[i]) != SEQUENCE[i] {
            ok = false;
        }
    }

    stream.cancel();

    if ok {
        sys_write(b"unified: ok (1 read + ");
        write_dec(N_EVENTS as u64);
        sys_write(b" events on one ring)\n");
        sys_exit(0);
    } else {
        sys_write(b"unified: FAIL\n");
        sys_exit(4);
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
