//! GDT, TSS, and the selector layout for the ring 0 <-> ring 3 boundary.
//!
//! The selector order is load-bearing twice over:
//!   - syscall/sysret derive the target selectors arithmetically from the
//!     STAR MSR: sysretq loads CS = STAR[63:48]+16 and SS = STAR[63:48]+8,
//!     so user data must sit immediately below user code.
//!   - enter_user_asm (usermode.rs) hardcodes 0x1b/0x23 when building its
//!     iretq frame. init() asserts the layout so a reordering here fails
//!     loudly at boot instead of #GP-ing in the transition.
//!
//! Stage B2.2 (D6): each core gets its own GDT+TSS+stacks, built by the same
//! `init()` every time -- so the selector *values* are identical across
//! cores by construction (the asserts below hold per-core) -- and loads its
//! own with `lgdt`/`ltr`. Only the TSS descriptor's *target* (this core's
//! own TSS) differs core to core; nothing above this module needs to know.

use core::ptr::{addr_of, addr_of_mut};

use x86_64::instructions::segmentation::{Segment, CS, DS, ES, SS};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

use crate::percpu;

/// IST slot used by the double-fault handler (interrupts.rs).
pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

const STACK_SIZE: usize = 8 * 4096;

// The field is storage only -- referenced by address, never read as data.
#[repr(align(16))]
struct Stack(#[allow(dead_code)] [u8; STACK_SIZE]);

/// One core's GDT, TSS, and the two stacks the TSS points at. Grouped so
/// `PER_CORE` below is a single fixed array, one slot per possible core.
struct CoreGdt {
    rsp0_stack: Stack,
    df_stack: Stack,
    tss: TaskStateSegment,
    gdt: GlobalDescriptorTable,
}

impl CoreGdt {
    const fn new() -> CoreGdt {
        CoreGdt {
            rsp0_stack: Stack([0; STACK_SIZE]),
            df_stack: Stack([0; STACK_SIZE]),
            tss: TaskStateSegment::new(),
            gdt: GlobalDescriptorTable::new(),
        }
    }
}

/// Reserved statically, one slot per possible core (no heap, like the rest
/// of Plinth) -- mirrors `percpu.rs`'s `PERCPU` array.
static mut PER_CORE: [CoreGdt; percpu::MAX_CORES] = [const { CoreGdt::new() }; percpu::MAX_CORES];

pub struct Selectors {
    pub kcode: SegmentSelector,
    pub kdata: SegmentSelector,
    pub ucode: SegmentSelector,
    pub udata: SegmentSelector,
}

/// Build, load, and activate `core_id`'s own GDT + TSS. Call once per core:
/// the BSP at boot, each AP at bring-up (Stage B2.2).
pub fn init(core_id: usize) -> Selectors {
    // SAFETY: each core writes and loads only its own PER_CORE[core_id]
    // slot, exactly once, before that core relies on it -- no two cores ever
    // touch the same slot, and a slot is never rebuilt afterward. The
    // `&'static` references `tss_segment`/`load` require are sound because
    // PER_CORE itself is `'static` storage.
    unsafe {
        (*addr_of_mut!(PER_CORE))[core_id].tss.privilege_stack_table[0] = VirtAddr::new(
            addr_of!((*addr_of!(PER_CORE))[core_id].rsp0_stack) as u64 + STACK_SIZE as u64,
        );
        (*addr_of_mut!(PER_CORE))[core_id].tss.interrupt_stack_table
            [DOUBLE_FAULT_IST_INDEX as usize] = VirtAddr::new(
            addr_of!((*addr_of!(PER_CORE))[core_id].df_stack) as u64 + STACK_SIZE as u64,
        );

        let gdt = &mut (*addr_of_mut!(PER_CORE))[core_id].gdt;
        let kcode = gdt.add_entry(Descriptor::kernel_code_segment());
        let kdata = gdt.add_entry(Descriptor::kernel_data_segment());
        let udata = gdt.add_entry(Descriptor::user_data_segment());
        let ucode = gdt.add_entry(Descriptor::user_code_segment());
        let tss_sel = gdt.add_entry(Descriptor::tss_segment(&*addr_of!(
            (*addr_of!(PER_CORE))[core_id].tss
        )));

        (*addr_of!((*addr_of!(PER_CORE))[core_id].gdt)).load();
        CS::set_reg(kcode);
        SS::set_reg(kdata);
        DS::set_reg(kdata);
        ES::set_reg(kdata);
        load_tss(tss_sel);

        // Keep in sync with enter_user_asm and the STAR layout (see module
        // doc). Holds on every core by construction (same build sequence
        // every time), but asserted per-core anyway -- fails loudly at boot
        // rather than #GP-ing in the ring transition.
        assert!(
            kcode.0 == 0x08 && kdata.0 == 0x10,
            "kernel selectors moved -- update usermode.rs and syscall.rs"
        );
        assert!(
            udata.0 == 0x1b && ucode.0 == 0x23,
            "user selectors moved -- update usermode.rs and syscall.rs"
        );

        Selectors { kcode, kdata, ucode, udata }
    }
}

/// Repoint this core's kernel stack the CPU switches to on a ring-3 ->
/// ring-0 interrupt (TSS RSP0). The scheduler calls this on every context
/// switch so the next process's interrupt frame lands on that process's own
/// kernel stack -- a shared RSP0 stack would clobber a suspended process's
/// saved frame. `top` must be the highest address of a live, 16-byte-aligned
/// stack.
pub fn set_kernel_stack(top: u64) {
    // SAFETY: resolves to this core's own TSS (percpu::core_id, set up by
    // this core's own init() before anything calls set_kernel_stack); the
    // TSS is mutated only between ring-3 runs (interrupts disabled) and by
    // this core's own init() at boot/bring-up -- never two cores touching
    // the same slot.
    unsafe {
        let core_id = percpu::core_id();
        (*addr_of_mut!(PER_CORE))[core_id].tss.privilege_stack_table[0] = VirtAddr::new(top);
    }
}
