//! GDT, TSS, and the selector layout for the ring 0 <-> ring 3 boundary.
//!
//! The selector order is load-bearing twice over:
//!   - syscall/sysret derive the target selectors arithmetically from the
//!     STAR MSR: sysretq loads CS = STAR[63:48]+16 and SS = STAR[63:48]+8,
//!     so user data must sit immediately below user code.
//!   - enter_user_asm (usermode.rs) hardcodes 0x1b/0x23 when building its
//!     iretq frame. init() asserts the layout so a reordering here fails
//!     loudly at boot instead of #GP-ing in the transition.

use core::ptr::{addr_of, addr_of_mut};

use x86_64::instructions::segmentation::{Segment, CS, DS, ES, SS};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

/// IST slot used by the double-fault handler (interrupts.rs).
pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

const STACK_SIZE: usize = 8 * 4096;

// The field is storage only -- referenced by address, never read as data.
#[repr(align(16))]
struct Stack(#[allow(dead_code)] [u8; STACK_SIZE]);

/// Stack the CPU switches to on a ring-3 -> ring-0 interrupt (TSS RSP0).
static mut RSP0_STACK: Stack = Stack([0; STACK_SIZE]);
/// Dedicated stack for double faults, so a kernel stack overflow still
/// reaches a working handler.
static mut DF_STACK: Stack = Stack([0; STACK_SIZE]);

static mut TSS_STORAGE: TaskStateSegment = TaskStateSegment::new();
static mut GDT_STORAGE: GlobalDescriptorTable = GlobalDescriptorTable::new();

pub struct Selectors {
    pub kcode: SegmentSelector,
    pub kdata: SegmentSelector,
    pub ucode: SegmentSelector,
    pub udata: SegmentSelector,
}

pub fn init() -> Selectors {
    // SAFETY: single-threaded early boot; nothing else touches these
    // statics, and the GDT/TSS live for the kernel lifetime.
    unsafe {
        let tss = &mut *addr_of_mut!(TSS_STORAGE);
        tss.privilege_stack_table[0] =
            VirtAddr::new(addr_of!(RSP0_STACK) as u64 + STACK_SIZE as u64);
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] =
            VirtAddr::new(addr_of!(DF_STACK) as u64 + STACK_SIZE as u64);

        let gdt = &mut *addr_of_mut!(GDT_STORAGE);
        let kcode = gdt.add_entry(Descriptor::kernel_code_segment());
        let kdata = gdt.add_entry(Descriptor::kernel_data_segment());
        let udata = gdt.add_entry(Descriptor::user_data_segment());
        let ucode = gdt.add_entry(Descriptor::user_code_segment());
        let tss_sel = gdt.add_entry(Descriptor::tss_segment(&*addr_of!(TSS_STORAGE)));

        (*addr_of!(GDT_STORAGE)).load();
        CS::set_reg(kcode);
        SS::set_reg(kdata);
        DS::set_reg(kdata);
        ES::set_reg(kdata);
        load_tss(tss_sel);

        // Keep in sync with enter_user_asm and the STAR layout (see module doc).
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
