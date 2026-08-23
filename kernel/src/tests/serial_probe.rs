//! UART presence probe (D11 step 2).
//!
//! Under QEMU a 16550 always decodes COM1, so the positive test only proves
//! the probe returns true where a UART exists. On its own that is the green
//! negative the D11 ruling warns about -- a probe hard-wired to `true` would
//! pass it. The absent-port test forces the other branch: it aims the probe at
//! COM4 (0x2E8), a base this harness's `-serial stdio` invocation never wires,
//! whose scratch register reads back open-bus 0xFF, and requires `false`. The
//! two together show the probe distinguishes present from absent rather than
//! returning a constant. Both run only in the `--features tests` QEMU build.

use super::TestCtx;
use crate::serial;
use crate::test_assert;

/// A base the test harness's QEMU invocation does not wire. Kept here, next to
/// the assumption it depends on, so a future change that adds a second serial
/// port has a comment to trip over.
const UNWIRED_COM: u16 = 0x2E8; // COM4

pub fn probe_detects_com1(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    test_assert!(serial::probe(), "probe() found no UART at COM1 under QEMU");
    Ok(())
}

pub fn probe_rejects_absent_port(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    test_assert!(
        !serial::probe_at(UNWIRED_COM),
        "probe matched an unwired port -- it is not actually testing presence"
    );
    Ok(())
}
