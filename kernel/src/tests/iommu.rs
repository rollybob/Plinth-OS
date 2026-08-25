//! Unit tests for the IOMMU device DMA domain (second-level page tables).
//!
//! The `Domain` is a pure page-table structure over the real frame allocator:
//! create, map/unmap a 4-KiB page, translate, tear down. These drive it directly
//! -- no VT-d register, no translation enabled, no device -- the same "pure
//! structure over injected backing" discipline as the `WaitQueue` tests, but the
//! backing here is the live `FrameAlloc` the harness already hands us, so the
//! leak checks are real (frames actually allocated and actually returned).
//!
//! What the integration path cannot yet isolate (there is no device behind a
//! domain until slice 3) these pin: the walk arithmetic, that translate is the
//! inverse of map, that a double-map is refused, and -- the load-bearing one --
//! that teardown returns every table frame and not one data frame.

use super::TestCtx;
use crate::iommu::{Domain, DomainError, IOMMU_READ, IOMMU_WRITE};
use crate::test_assert;

/// QEMU's remapping unit is 48-bit -> a 4-level table. The tests use that width.
const AW: u8 = 48;

/// map then translate is the identity on the frame, carrying the page offset;
/// unmap makes it disappear.
pub fn map_translate_roundtrip(ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut d = Domain::new(ctx.frames, AW).map_err(|_| "domain new failed")?;

    let iova = 0x1000u64;
    let phys = 0x00AA_0000u64;
    d.map(ctx.frames, iova, phys, IOMMU_READ | IOMMU_WRITE)
        .map_err(|_| "map failed")?;

    // Exact frame, and the offset within the page is preserved.
    test_assert!(d.translate(iova) == Some(phys), "translate should resolve the mapped frame");
    test_assert!(
        d.translate(iova + 0x123) == Some(phys + 0x123),
        "translate should carry the page offset"
    );
    // A different page in the same table is not mapped.
    test_assert!(d.translate(iova + 0x1000).is_none(), "neighbour page must be unmapped");

    d.unmap(iova).map_err(|_| "unmap failed")?;
    test_assert!(d.translate(iova).is_none(), "translate after unmap must be None");

    d.teardown(ctx.frames);
    Ok(())
}

/// A fresh domain resolves nothing, and the walk terminates cleanly at every
/// level (no present entry anywhere).
pub fn empty_domain_translates_none(ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut d = Domain::new(ctx.frames, AW).map_err(|_| "domain new failed")?;
    test_assert!(d.translate(0x0).is_none(), "zero iova unmapped in a fresh domain");
    test_assert!(d.translate(0x1000).is_none(), "low iova unmapped");
    test_assert!(d.translate(1 << 30).is_none(), "mid iova unmapped");
    test_assert!(d.translate(1 << 39).is_none(), "high iova unmapped");
    d.teardown(ctx.frames);
    Ok(())
}

/// Misaligned addresses are rejected; a double map is caught; unmapping nothing
/// is an error. The negative space, so a bug cannot pass silently.
pub fn rejects_bad_requests(ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut d = Domain::new(ctx.frames, AW).map_err(|_| "domain new failed")?;

    test_assert!(
        d.map(ctx.frames, 0x1001, 0x2000, IOMMU_READ) == Err(DomainError::Misaligned),
        "unaligned iova must be rejected"
    );
    test_assert!(
        d.map(ctx.frames, 0x1000, 0x2001, IOMMU_READ) == Err(DomainError::Misaligned),
        "unaligned phys must be rejected"
    );
    test_assert!(
        d.unmap(0x1000) == Err(DomainError::NotMapped),
        "unmapping an absent page must be NotMapped"
    );

    d.map(ctx.frames, 0x1000, 0x2000, IOMMU_READ | IOMMU_WRITE)
        .map_err(|_| "first map failed")?;
    test_assert!(
        d.map(ctx.frames, 0x1000, 0x3000, IOMMU_READ) == Err(DomainError::AlreadyMapped),
        "double map must be AlreadyMapped, not a silent overwrite"
    );

    d.teardown(ctx.frames);
    Ok(())
}

/// The load-bearing property: teardown returns every table frame the domain
/// allocated -- root and all intermediate tables -- and nothing else. Mappings
/// spread across the address space so several distinct intermediate tables are
/// built, then freed. Non-vacuous by construction: it asserts frames were really
/// consumed before checking they all came back.
pub fn teardown_frees_every_table(ctx: &mut TestCtx) -> Result<(), &'static str> {
    let before = ctx.frames.free_frames();

    let mut d = Domain::new(ctx.frames, AW).map_err(|_| "domain new failed")?;
    // Three IOVAs chosen to diverge at the L4, L3, and L1 index respectively, so
    // the domain must allocate multiple separate intermediate tables (not just
    // reuse one path).
    d.map(ctx.frames, 0x1000, 0x10_0000, IOMMU_READ | IOMMU_WRITE)
        .map_err(|_| "map a failed")?;
    d.map(ctx.frames, 1 << 30, 0x20_0000, IOMMU_READ | IOMMU_WRITE)
        .map_err(|_| "map b failed")?;
    d.map(ctx.frames, 1 << 39, 0x30_0000, IOMMU_READ | IOMMU_WRITE)
        .map_err(|_| "map c failed")?;

    // The domain really took frames for its tables (root + intermediates): if it
    // had not, "all frames returned" would prove nothing.
    test_assert!(ctx.frames.free_frames() < before, "domain must consume table frames");

    d.teardown(ctx.frames);
    test_assert!(
        ctx.frames.free_frames() == before,
        "teardown must return every table frame (no leak, no over-free)"
    );
    Ok(())
}

/// A nonsensical address width (not a VT-d 3/4/5-level depth) is refused rather
/// than silently building a wrong-shaped table.
pub fn rejects_unsupported_width(ctx: &mut TestCtx) -> Result<(), &'static str> {
    test_assert!(
        Domain::new(ctx.frames, 40).err() == Some(DomainError::UnsupportedWidth),
        "40-bit is not a VT-d AGAW and must be rejected"
    );
    // 48-bit (the real unit) is accepted; clean up so the check leaves no frame held.
    let mut d = Domain::new(ctx.frames, AW).map_err(|_| "48-bit width should be accepted")?;
    d.teardown(ctx.frames);
    Ok(())
}
