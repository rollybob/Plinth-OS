//! The shared workload: one application, parameterised only by which
//! library OS manages its memory.
//!
//! Sizes are tuned so the two policies must diverge. Three 1536-byte
//! blocks: a and b fill 3072 of the first 4096-byte frame. After
//! free(a), the third allocation either reuses a's slot (free list --
//! same address, still one kernel frame) or bumps past the remaining
//! 1024 bytes into a second kernel frame (bump -- new address, two
//! frames). Identical code, identical kernel, different OS.

#![no_std]

use libos::MemPolicy;
use libplinth::{sys_exit, sys_write, write_dec, write_hex};

const BLOCK: usize = 1536;

pub fn run<P: MemPolicy>(policy: &mut P) {
    sys_write(b"demo: policy = ");
    sys_write(policy.name().as_bytes());
    sys_write(b"\n");

    let a = checked_alloc(policy, BLOCK, 0xAA);
    print_block(b"demo: a = ", a);
    let b = checked_alloc(policy, BLOCK, 0xBB);
    print_block(b"demo: b = ", b);

    policy.free(a, BLOCK);
    sys_write(b"demo: freed a\n");

    let c = checked_alloc(policy, BLOCK, 0xCC);
    print_block(b"demo: c = ", c);

    if c == a {
        sys_write(b"demo: c reused a freed block\n");
    } else {
        sys_write(b"demo: c got a new address\n");
    }

    // b must have survived everything around it untouched.
    if !verify(b, BLOCK, 0xBB) {
        fail(b"demo: b was corrupted\n");
    }

    sys_write(b"demo: kernel frames used: ");
    write_dec(policy.kernel_frames() as u64);
    sys_write(b"\n");
}

/// Allocate, fill with `val`, verify the fill round-trips.
fn checked_alloc<P: MemPolicy>(policy: &mut P, size: usize, val: u8) -> *mut u8 {
    let ptr = policy.alloc(size);
    if ptr.is_null() {
        fail(b"demo: allocation failed\n");
    }
    // SAFETY: the policy returned `size` bytes of mapped, writable user
    // memory (or null, handled above). Volatile so the accesses hit the
    // actual frames.
    unsafe {
        let mut i = 0;
        while i < size {
            ptr.add(i).write_volatile(val);
            i += 1;
        }
    }
    if !verify(ptr, size, val) {
        fail(b"demo: readback mismatch\n");
    }
    ptr
}

fn verify(ptr: *mut u8, size: usize, val: u8) -> bool {
    // SAFETY: as in checked_alloc.
    unsafe {
        let mut i = 0;
        while i < size {
            if ptr.add(i).read_volatile() != val {
                return false;
            }
            i += 1;
        }
    }
    true
}

fn print_block(label: &[u8], ptr: *mut u8) {
    sys_write(label);
    write_hex(ptr as u64);
    sys_write(b"\n");
}

fn fail(msg: &[u8]) -> ! {
    sys_write(msg);
    sys_exit(1)
}
