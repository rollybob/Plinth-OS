//! Framebuffer reclamation demo (D6) -- the lending half.
//!
//! The positive statement of a negative property: **a lent capability survives
//! the borrower's death.** This process is granted the whole screen, draws and
//! hashes a frame, transfers the framebuffer capability to a child that FAULTS
//! while holding it, and then draws and hashes a second frame through the
//! capability that came back.
//!
//! Both frames are the whole demo. The first proves the screen was genuinely
//! ours before we lent it; the second proves it is ours again afterwards. On the
//! pre-reclamation kernel the second one is not merely wrong, it is
//! unreachable -- the only framebuffer capability in existence died with the
//! child, no syscall mints another, and the map below fails. That is what makes
//! this a regression test rather than a demonstration.
//!
//! It also exercises the ABI v2.9 half of the mechanism. Reclamation without
//! notification would be useless here: there is no operation to enumerate a
//! capability table, so if the death-wake did not report where the capability
//! landed, this process could not find it. The wake arrives as `IPC_PEER_DIED`
//! -- the child died, which is expected -- carrying a real slot in place of the
//! `NO_CAP` a v2.8 kernel always returned.
//!
//! **This demo used to have a race, and it was the kernel's, not the demo's.**
//! Until 2026-07-30 the landing slot reached only a lender already BLOCKED on the
//! dying process, because that is the only peer `reap_dying` wakes. If the child
//! faulted before this process reached its `recv` below, the capability was
//! reclaimed into our table anyway -- measured, at slot 2, drawable, hashing
//! identically to the success case -- but `recv` took a different path in the
//! kernel and returned `NO_CAP`, so we could not find what we had been given.
//! That was K-023, and it made this demo pass most of the time rather than
//! always.
//!
//! D7 closed it: the landing slot is recorded on the
//! lender's scheduler slot and BOTH delivery paths read it from there, so the
//! result no longer depends on which of us reached the kernel first. The demo is
//! therefore ordering-independent by construction rather than by luck -- if it
//! ever goes intermittent again, that is a real regression and not the weather.
//!
//! Compare the other two framebuffer negatives. `gfxbound-user` reaches outside
//! its grant in space; `gfxrevoke-user` reaches outside its own grant in time,
//! after releasing it. This one is the case where the process that loses the
//! capability is not the one that gets it back.

#![no_std]
#![no_main]

use libgfx::Framebuffer;
use libplinth::{
    sys_exit, sys_recv_cap, sys_spawn, sys_write, write_hex, FB_SLOT, IPC_PEER_DIED, MAP_BASE,
    NO_CAP, SYS_ERR,
};

/// `fbreclaimchild-user`'s id in the kernel's SPAWNABLE table.
const FBRECLAIMCHILD_ID: u64 = 5;

/// Side of the hashed square. 128 to match every other framebuffer demo (gfx,
/// gfxtext, shell, shellapp), so the numbers are comparable by eye in the boot
/// log. This said 64 while its comment claimed to match them, which it did not.
const HASH_SIDE: u32 = 128;

fn emit_hash(tag: &[u8], fb: &Framebuffer) {
    sys_write(tag);
    write_hex(fb.hash_origin_square(HASH_SIDE));
    sys_write(b"\n");
}

/// Paint a deterministic frame: a flat field plus a marker pixel at the origin,
/// so the hash is fixed and a lost mapping cannot masquerade as a matching one.
fn paint(fb: &Framebuffer, r: u8, g: u8, b: u8) {
    let info = fb.info();
    fb.fill_rect(0, 0, info.width, info.height, r, g, b);
    fb.put_pixel(0, 0, 0xFF, 0xFF, 0xFF);
}

#[no_mangle]
pub extern "C" fn _start(_id: u64) -> ! {
    // 1) The screen is ours: map it, draw, hash.
    let fb = match Framebuffer::map(FB_SLOT, MAP_BASE) {
        Some(fb) => fb,
        None => {
            sys_write(b"fbreclaim: initial map failed\n");
            sys_exit(1);
        }
    };
    paint(&fb, 0x10, 0x10, 0x18);
    emit_hash(b"fbreclaim: lent hash ", &fb);

    // 2) Lend it to a child that will die holding it. The spawn transfer revokes
    //    the capability here and unmaps it with the authority (fb_mapping D1), so
    //    from this point until the child dies this process cannot draw at all.
    let handle = sys_spawn(FBRECLAIMCHILD_ID, FB_SLOT);
    if handle == SYS_ERR {
        sys_write(b"fbreclaim: spawn failed\n");
        sys_exit(2);
    }

    // 3) Wait. The child faults, so the wake is IPC_PEER_DIED -- and since v2.9
    //    it carries the slot the reclaimed capability landed in.
    let (status, _msg, cap_slot) = sys_recv_cap(handle);
    if status != IPC_PEER_DIED {
        sys_write(b"fbreclaim: expected a dead child\n");
        sys_exit(3);
    }
    if cap_slot == NO_CAP {
        // Stated loudly rather than exiting 0 and looking like a pass. Say only
        // what is actually known here, which is less than it looks: the wake
        // carried no slot. Whether the capability still EXISTS is not
        // observable from this process, because there is no op to enumerate a
        // capability table -- and the two kernels that reach this line disagree
        // about the answer. On a pre-reclamation kernel it really is gone. On
        // this one it is almost certainly sitting in our table unfound, because
        // we lost the race in K-023. A message that picked either story would be
        // false half the time; this one is true in both.
        sys_write(b"fbreclaim: no landing slot -- the death-wake reported nothing\n");
        sys_exit(4);
    }
    sys_write(b"fbreclaim: child died, screen came back\n");

    // 4) The screen is ours again. Map it at the slot the kernel reported and
    //    draw a DIFFERENT frame, so the second hash cannot coincide with the
    //    first and a stale mapping cannot be mistaken for a live one.
    let fb = match Framebuffer::map(cap_slot, MAP_BASE) {
        Some(fb) => fb,
        None => {
            sys_write(b"fbreclaim: remap after reclaim failed\n");
            sys_exit(5);
        }
    };
    // Deliberately not a permutation of the first colour, and a different
    // channel AVERAGE. `libgfx` collapses a `FB_FMT_U8` framebuffer to
    // `(r + g + b) / 3`, so any reordering of the same three values writes an
    // identical byte -- the two hashes would match and this demo's whole proof
    // would pass vacuously on such a display. The smoke config is BGR, so the
    // original permutation happened to work; it was luck, not a choice.
    paint(&fb, 0x60, 0x48, 0x30);
    emit_hash(b"fbreclaim: reclaimed hash ", &fb);

    sys_exit(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
