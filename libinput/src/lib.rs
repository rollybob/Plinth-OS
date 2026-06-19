//! libinput -- a console-input library OS.
//!
//! Input is policy, the same way the filesystem is: the kernel multiplexes the
//! keyboard into raw scancode events through an `EventSource` capability and
//! refuses to say what a key *means*, so the keymap (Set-1 scancode ->
//! character, with shift) and the line discipline (assemble a line, handle
//! backspace) live here, in unprivileged code over `event_recv`. This is the
//! input analogue of `libfs` over `block_read`.
//!
//! `keymap` is pure -- no syscalls -- so it is host-unit-tested with `cargo
//! test` (like libfs's archive parser). The line reader issues syscalls and is
//! built only on the bare target (its libplinth dependency is target-gated, so
//! the host test build compiles just the keymap).

#![cfg_attr(not(test), no_std)]

pub mod keymap;

#[cfg(target_os = "none")]
mod reader;
#[cfg(target_os = "none")]
pub use reader::read_line;
