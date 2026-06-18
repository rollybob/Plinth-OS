//! The filesystem library-OS demo (loader half of the load-from-disk demo).
//!
//! The kernel launches this process with one capability: a BlockRange over the
//! boot-archive device, at BLOCK_SLOT. Everything else is policy in unprivileged
//! code -- the on-disk archive format and the lookup-then-load flow live in
//! libfs, not the kernel. This program asks libfs to load a program named
//! "diskhello" off the archive and launch it, then collects the loaded
//! program's result over the channel spawn set up. The kernel never parses the
//! archive: it only multiplexes the disk (the BlockRange) and validates the ELF
//! libfs hands back to spawn_from_buffer.

#![no_std]
#![no_main]

use libfs::load::spawn_from_archive;
use libplinth::{sys_exit, sys_recv, sys_write, write_dec, BLOCK_SLOT, IPC_OK, NO_CAP};

#[no_mangle]
pub extern "C" fn _start(_id: u64) -> ! {
    sys_write(b"fsdemo: loading 'diskhello' from the boot archive\n");

    match spawn_from_archive(BLOCK_SLOT, b"diskhello", NO_CAP) {
        Ok(handle) => {
            // recv on the spawn handle is the wait: diskhello sends its result
            // and exits, and this collects it.
            let (status, result) = sys_recv(handle);
            if status == IPC_OK {
                sys_write(b"fsdemo: diskhello returned ");
                write_dec(result);
                sys_write(b"\n");
            } else {
                sys_write(b"fsdemo: diskhello did not report a result\n");
            }
        }
        Err(e) => {
            sys_write(b"fsdemo: load failed: ");
            sys_write(e.as_bytes());
            sys_write(b"\n");
        }
    }

    sys_exit(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
