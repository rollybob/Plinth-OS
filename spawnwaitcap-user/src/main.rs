//! `spawn_and_wait_cap` end-to-end demo (ABI v2.9, K-012).
//!
//! The 08-05 session added `spawn_and_wait_cap` and shipped it with no watched
//! failure of its own. Its delivery path was inherited-proven -- `fbreclaim-user`
//! already watches lend -> child dies -> lender collects go green every smoke run
//! -- but that demo drives the mechanism by hand (`sys_spawn`, then
//! `sys_recv_cap`), so the *helper that packages those calls* was the one part
//! with nothing exercising it. This is that watch, and it deliberately does not
//! replace fbreclaim: keeping one demo on the raw syscalls is what makes a
//! failure attributable, because if both went through the helper a kernel
//! regression and a library regression would be indistinguishable.
//!
//! It reuses `fbreclaimchild-user` as its child rather than shipping a near-copy
//! of it. The child's job -- map the transferred capability, draw through it to
//! prove it really holds it, then fault -- is identical for both parents, and a
//! second crate saying the same thing would be one more place to drift.
//!
//! The helper makes three claims and this demo now watches all three. They need
//! two different shapes, which is why there are two parts below.
//!
//! # Part 1 -- the landing slot (the framebuffer round)
//!
//! **This part lends a Framebuffer -- once a necessity, now a choice.** When this
//! demo was written, a Framebuffer was the only lendable-and-recoverable object:
//! `capability::release_action` scoped reclamation to `CapObject::Framebuffer`
//! (Design/cap_reclaim.md D2, narrowed at ruling time), so the 08-05 handoff's
//! proposal to lend "a `Ring`/`Frame`" did not work -- an ordinary Frame lent to a
//! dying child is simply freed with it and the death-wake carries `NO_CAP`. That
//! was measured, not assumed: the Frame version of this demo was built first and
//! reported no landing slot, its frame bracket flat because the frame went back to
//! the allocator rather than to the lender.
//!
//! Slice 4 (lender_owed.md D6, 2026-08-17) widened `is_reclaimable_kind` to
//! `Framebuffer | BlockRange | EventSource`, and `blkreclaim-user` now lends a
//! `BlockRange` a dying child returns -- so this part keeps the Framebuffer by
//! CHOICE, not because nothing else can be lent. The choice is still the right
//! one for *this* demo: it reuses `fbreclaimchild-user` as its child and the two
//! differing hashes below are its proof the screen genuinely came home. A Frame or
//! Ring remains unlendable (re-mintable / pooled, so freed rather than reclaimed),
//! which is why the widening stopped at the three kinds no syscall can re-mint.
//!
//! **What this asserts that `fbreclaim-user` does not:** that the helper returns
//! the landing slot at all. `spawn_and_wait` returns two values and drops it, and
//! that collapse IS K-012. Watched failing: swapping the call below for
//! `spawn_and_wait` takes the "no landing slot" exit and turns smoke red.
//!
//! The two hashes carry the same proof they carry in `fbreclaim-user`: the first
//! says the screen was genuinely ours before we lent it, the second says it is
//! ours again, and they differ because the second frame is painted a different
//! colour -- so a stale mapping cannot masquerade as a reclaimed one.
//!
//! # Part 2 -- the released spawn handle (the loop)
//!
//! Every `spawn` mints a RECV handle into the caller's table, and the helper
//! releases it after the join. Nothing proved that until this loop, and the
//! obvious candidate does not: **the endpoint bracket xtask puts around this
//! demo cannot see a leaked handle**, measured on 08-06 by deleting the
//! `sys_cap_release` from the helper and watching smoke stay entirely green
//! (210/176/386, endpoint baseline "8 free, no leak"). The bracket samples before
//! and after the *whole demo*, and by "after" the process has exited and teardown
//! has released every capability it held. A bracket around a process's whole life
//! cannot see a leak inside that life, only one that outlives it (assumption
//! A-12). It is kept for what it does prove -- the spawn endpoint is reclaimed
//! once the demo is over.
//!
//! Catching the leak needs `caprelease-user`'s shape instead: run the cycle more
//! times than the system can have in flight, so what is never given back runs
//! out. With the release, every round reuses what the previous round returned and
//! the loop is flat.
//!
//! **What runs out is the endpoint pool, not the capability table.** Measured on
//! 2026-08-10 by deleting the helper's `sys_cap_release` and reading the round
//! this loop dies on: **round 7**, with roughly six cap slots still free.
//! `MAX_ENDPOINTS` is 8 (`kernel/src/ipc.rs`) and every spawn creates one for its
//! result channel; the handle is what keeps it referenced, so leaking the handle
//! pins the endpoint. Seven rather than eight because part 1's handle leaks too
//! under that control, taking one endpoint with it -- the arithmetic is exact,
//! and `caprelease-user` under the same treatment dies at 8 with no part 1 ahead
//! of it. Both `caprelease-user` and the xtask message described this as a table
//! overrun until this session; see assumption A-13 in Design/known_bugs.md.
//!
//! This does not weaken the test -- the leak is caught either way, and `ROUNDS`
//! clears both limits. It does mean the loop never actually reaches a full table,
//! so do not cite it as evidence about table behaviour. `ROUNDS` is derived from
//! the minimums `ABI.md` publishes rather than copied from the kernel, so it
//! still clears both if either limit grows (`lender_owed.md` D9).
//!
//! **Part 2 deliberately transfers nothing and spawns the silent worker.** The
//! handle is minted by every spawn regardless of whether a capability rides
//! along, so the framebuffer is irrelevant to this claim -- and dropping it is
//! what keeps the cost down. Twenty rounds of `fbreclaimchild` would put sixty
//! lines into `expected_boot_log.txt` (its own line, the fault, the termination)
//! to say something none of them are about. `quietworker-user` exists for exactly
//! this reason and `caprelease-user` uses it the same way: the whole loop adds a
//! single summary line to the boot log.
//!
//! Both parts live in one process on purpose. The 16-slot table they contend for
//! is the same table, so part 2 runs with part 1's reclaimed framebuffer still
//! held -- which is the honest budget, not a clean-room one.

#![no_std]
#![no_main]

use libgfx::Framebuffer;
use libplinth::{
    spawn_and_wait_cap, sys_exit, sys_write, write_hex, FB_SLOT, IPC_ERR, IPC_OK, IPC_PEER_DIED,
    MAP_BASE, NO_CAP,
};

/// `fbreclaimchild-user`'s id in the kernel's SPAWNABLE table. Mirrored from
/// `fbreclaim-user`, which names the same child; ids are positional, so if one
/// of these goes stale they both do.
const FBRECLAIMCHILD_ID: u64 = 5;

/// `quietworker-user`'s id in the same table, mirrored from `caprelease-user`.
const QUIETWORKER_ID: u64 = 4;

/// Rounds in part 2. A leaking build dies at round 7 (measured -- the eight
/// endpoints, one of them already spent by part 1; see the module docs), and
/// would die in the low teens on the capability table even if the pool were
/// bottomless, since this process enters the loop holding its CPU budget and the
/// reclaimed framebuffer. The derived value clears both -- the same number as
/// `caprelease-user`, now for the structural reason rather than by both crates
/// happening to pick it: `ABI.md` publishes the limits as guaranteed minimums
/// and `libplinth` derives the count from them (`lender_owed.md` D9).
const ROUNDS: u64 = libplinth::REUSE_ROUNDS;

/// Side of the hashed square. 128 to match every other framebuffer demo, so the
/// numbers stay comparable by eye in the boot log.
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

fn fail(msg: &[u8], code: u64) -> ! {
    sys_write(msg);
    sys_exit(code)
}

/// Fail with a round number attached. Which round a part-2 failure lands on is
/// the diagnostic, and it is the reason these exits carry a number at all: round
/// 0 means the helper never worked, while round 7 means it worked until the
/// endpoint pool ran out -- the leaked-handle signature.
fn fail_at(prefix: &[u8], round: u64, code: u64) -> ! {
    emit_line(prefix, round, b"\n");
    sys_exit(code)
}

#[no_mangle]
pub extern "C" fn _start(_id: u64) -> ! {
    // ---------------------------------------------------------------
    // Part 1: the landing slot.
    // ---------------------------------------------------------------

    // 1) The screen is ours: map it, draw, hash.
    let fb = match Framebuffer::map(FB_SLOT, MAP_BASE) {
        Some(fb) => fb,
        None => fail(b"spawnwaitcap: initial map failed\n", 1),
    };
    paint(&fb, 0x18, 0x10, 0x10);
    emit_hash(b"spawnwaitcap: lent hash ", &fb);

    // 2) Lend the screen to a child that dies holding it, and wait -- one call
    //    doing what fbreclaim-user spells out as three. The transfer revokes the
    //    capability here and unmaps it with the authority (fb_mapping D1), so
    //    from here until the child dies this process cannot draw at all.
    let (status, _msg, cap_slot) = spawn_and_wait_cap(FBRECLAIMCHILD_ID, FB_SLOT);
    if status != IPC_PEER_DIED {
        // Covers IPC_ERR (the spawn failed) and IPC_OK (the child sent a result
        // instead of dying). Either breaks the premise, so the map below would
        // prove nothing.
        fail(b"spawnwaitcap: expected a dead child\n", 2);
    }
    if cap_slot == NO_CAP {
        // The value this helper exists to return. If this fires, either it has
        // regressed to `spawn_and_wait`'s two-value collapse -- which is K-012,
        // reopened -- or the kernel stopped recording the landing slot.
        fail(b"spawnwaitcap: no landing slot -- the helper reported nothing\n", 3);
    }
    // The homecoming guarantee, asserted directly rather than inferred from a
    // green run (`lender_owed.md` D2(D)): a lent capability returns to the slot
    // it left from, because that slot was reserved the moment the loan started.
    //
    // Watched failing before it was believed. With the reservation disabled in
    // `process::revoke_and_unmap_for_lend`, this same demo reports **slot 2** --
    // the first free slot, which is what the kernel chose for every loan before
    // this milestone. With it, slot 1, which is FB_SLOT. That is the whole
    // difference the ruling buys, and it is one number.
    if cap_slot != FB_SLOT {
        fail(b"spawnwaitcap: the screen did not come home to the slot it left\n", 8);
    }
    emit_line(b"spawnwaitcap: child died, screen came back at slot ", cap_slot, b"\n");

    // 3) The screen is ours again. Map it at the slot the helper reported and
    //    paint a different colour, so the second hash cannot coincide with the
    //    first. Not a permutation of the first: libgfx collapses a FB_FMT_U8
    //    framebuffer to (r + g + b) / 3, so a reordering of the same three
    //    values would write an identical byte and both hashes would match
    //    vacuously on such a display.
    let fb = match Framebuffer::map(cap_slot, MAP_BASE) {
        Some(fb) => fb,
        None => fail(b"spawnwaitcap: remap after reclaim failed\n", 4),
    };
    paint(&fb, 0x30, 0x60, 0x48);
    emit_hash(b"spawnwaitcap: reclaimed hash ", &fb);

    // ---------------------------------------------------------------
    // Part 2: the released spawn handle.
    // ---------------------------------------------------------------
    // Still holding the reclaimed framebuffer, deliberately -- see the module
    // docs. Nothing is transferred here: `NO_CAP` as the transfer slot is the
    // kernel's "no capability rides along" sentinel (`spawn_scheduled` compares
    // it against `ERR`, and the two constants are both `u64::MAX`).
    let mut round = 0u64;
    while round < ROUNDS {
        let (status, _result, cap_slot) = spawn_and_wait_cap(QUIETWORKER_ID, NO_CAP);
        if status == IPC_ERR {
            // The leak's face: `spawn_scheduled` could not create a result
            // endpoint, because the leaked handles are still holding all eight.
            fail_at(b"spawnwaitcap: spawn failed at round ", round, 5);
        }
        if status != IPC_OK {
            // quietworker sends a result and exits normally, so anything else --
            // IPC_PEER_DIED especially -- means the child died instead of
            // reporting, and the join proved nothing.
            fail_at(b"spawnwaitcap: join failed at round ", round, 6);
        }
        if cap_slot != NO_CAP {
            // Nothing was lent, so nothing may come home. A slot arriving here
            // would mean the reclaim path fired for a capability this process
            // never gave away.
            fail_at(b"spawnwaitcap: unexpected landing slot at round ", round, 7);
        }
        round += 1;
    }

    emit_line(b"spawnwaitcap: ", ROUNDS, b" handle round-trips, slots reused\n");

    sys_exit(0)
}

/// Write `<prefix><v><suffix>` as one atomic sys_write. Same shape as
/// caprelease-user's: one write per line, so concurrent processes cannot
/// interleave mid-line in the serial log.
fn emit_line(prefix: &[u8], v: u64, suffix: &[u8]) {
    let mut buf = [0u8; 96];
    let mut len = 0;
    len += put(&mut buf[len..], prefix);
    len += put_dec(&mut buf[len..], v);
    len += put(&mut buf[len..], suffix);
    sys_write(&buf[..len]);
}

fn put(dst: &mut [u8], src: &[u8]) -> usize {
    let mut i = 0;
    while i < src.len() {
        dst[i] = src[i];
        i += 1;
    }
    src.len()
}

fn put_dec(dst: &mut [u8], mut v: u64) -> usize {
    if v == 0 {
        dst[0] = b'0';
        return 1;
    }
    let mut tmp = [0u8; 20];
    let mut i = 0;
    while v > 0 {
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
        i += 1;
    }
    let mut j = 0;
    while j < i {
        dst[j] = tmp[i - 1 - j];
        j += 1;
    }
    i
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
