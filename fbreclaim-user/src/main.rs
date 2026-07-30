//! Framebuffer reclamation demo (Design/cap_reclaim.md D6) -- the lending half.
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
        // The pre-reclamation behaviour, stated loudly rather than exiting 0 and
        // looking like a pass: the child's death destroyed the screen and nothing
        // can draw for the rest of the boot.
        sys_write(b"fbreclaim: NOT reclaimed -- the screen died with the child\n");
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
    paint(&fb, 0x18, 0x10, 0x10);
    emit_hash(b"fbreclaim: reclaimed hash ", &fb);

    sys_exit(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
