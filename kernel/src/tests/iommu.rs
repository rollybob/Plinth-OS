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
use crate::iommu::{
    Domain, DomainError, IovaAllocator, PteFmt, TranslationTables, IOMMU_READ, IOMMU_WRITE,
};
use crate::test_assert;

/// QEMU's remapping unit is 48-bit -> a 4-level table. The tests use that width.
const AW: u8 = 48;

/// map then translate is the identity on the frame, carrying the page offset;
/// unmap makes it disappear.
pub fn map_translate_roundtrip(ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut d = Domain::new(ctx.frames, AW, PteFmt::Vtd).map_err(|_| "domain new failed")?;

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

/// The AMD-Vi entry encoding round-trips the same way the VT-d one does: build a
/// domain in the AMD-Vi format, map a page, translate it back (proving the present
/// bit / Next Level / IR-IW encoding decodes to the right address), refuse a double
/// map, unmap, and tear down with no leak. The radix walk is shared with VT-d; this
/// pins the AMD-Vi per-entry encoding that forks under it.
pub fn amdvi_map_translate_roundtrip(ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut d = Domain::new(ctx.frames, AW, PteFmt::AmdVi).map_err(|_| "amd domain new failed")?;

    let iova = 0x2000u64;
    let phys = 0x00BB_0000u64;
    d.map(ctx.frames, iova, phys, IOMMU_READ | IOMMU_WRITE)
        .map_err(|_| "amd map failed")?;

    test_assert!(d.translate(iova) == Some(phys), "amd translate should resolve the mapped frame");
    test_assert!(
        d.translate(iova + 0x321) == Some(phys + 0x321),
        "amd translate should carry the page offset"
    );
    test_assert!(d.translate(iova + 0x1000).is_none(), "amd neighbour page must be unmapped");
    test_assert!(
        d.map(ctx.frames, iova, phys, IOMMU_READ) == Err(DomainError::AlreadyMapped),
        "amd double-map must be refused"
    );

    d.unmap(iova).map_err(|_| "amd unmap failed")?;
    test_assert!(d.translate(iova).is_none(), "amd translate after unmap must be None");

    d.teardown(ctx.frames);
    Ok(())
}

/// A fresh domain resolves nothing, and the walk terminates cleanly at every
/// level (no present entry anywhere).
pub fn empty_domain_translates_none(ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut d = Domain::new(ctx.frames, AW, PteFmt::Vtd).map_err(|_| "domain new failed")?;
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
    let mut d = Domain::new(ctx.frames, AW, PteFmt::Vtd).map_err(|_| "domain new failed")?;

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

    let mut d = Domain::new(ctx.frames, AW, PteFmt::Vtd).map_err(|_| "domain new failed")?;
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
        Domain::new(ctx.frames, 40, PteFmt::Vtd).err() == Some(DomainError::UnsupportedWidth),
        "40-bit is not a VT-d AGAW and must be rejected"
    );
    // 48-bit (the real unit) is accepted; clean up so the check leaves no frame held.
    let mut d = Domain::new(ctx.frames, AW, PteFmt::Vtd).map_err(|_| "48-bit width should be accepted")?;
    d.teardown(ctx.frames);
    Ok(())
}

// --- Root / context translation tables (slice 3a) ---

// Bit layout of the VT-d root/context entries under test (Intel VT-d spec). Kept
// as local literals so the test states the format independently of the module.
const PRESENT: u64 = 1 << 0;
const TT_MASK: u64 = 0b11 << 2; // context translation-type field (00 = second-level)
const AW_MASK: u64 = 0x7; // context high-word address-width field
const PTR_MASK: u64 = 0x000f_ffff_ffff_f000; // [51:12] next-table / SLPTPTR pointer

/// `set_device` encodes a context entry the way the VT-d spec lays it out: a
/// present, second-level entry whose SLPTPTR is the domain root, with the right
/// address width and domain id -- and it points the root entry at the context
/// table. Also ties the two structures together: the SLPTPTR is a real
/// `Domain::root()`.
pub fn context_entry_encoding(ctx: &mut TestCtx) -> Result<(), &'static str> {
    // A real domain to point at, so the SLPTPTR under test is a live root.
    let mut dom = Domain::new(ctx.frames, AW, PteFmt::Vtd).map_err(|_| "domain new failed")?;
    let slptptr = dom.root();

    let mut tt = TranslationTables::new(ctx.frames).map_err(|_| "tables new failed")?;
    tt.set_device(3, 0, slptptr, 4, 7);

    // Root entry for bus 0: present, and its pointer is 4-KiB aligned and nonzero
    // (it names the context table).
    let (root_lo, _root_hi) = tt.root_entry(0);
    test_assert!(root_lo & PRESENT != 0, "root[0] must be present");
    test_assert!(root_lo & PTR_MASK != 0, "root[0] must name a context table");

    // Context entry for 3:0.
    let (lo, hi) = tt.context_entry(3, 0);
    test_assert!(lo & PRESENT != 0, "context 3:0 must be present");
    test_assert!(lo & TT_MASK == 0, "translation type must be second-level (00)");
    test_assert!(lo & PTR_MASK == slptptr, "SLPTPTR must be the domain root");
    test_assert!(hi & AW_MASK == 2, "4-level table encodes AW = 2 (48-bit)");
    test_assert!((hi >> 8) & 0xffff == 7, "domain id must be recorded");

    // An untouched source-id stays absent -- set_device wrote only 3:0.
    let (other_lo, _) = tt.context_entry(4, 0);
    test_assert!(other_lo & PRESENT == 0, "an unset device must not be present");

    tt.teardown(ctx.frames);
    dom.teardown(ctx.frames);
    Ok(())
}

/// Tearing down the tables returns both frames (root + context); non-vacuous
/// (asserts they were taken first).
pub fn translation_tables_teardown_frees(ctx: &mut TestCtx) -> Result<(), &'static str> {
    let before = ctx.frames.free_frames();
    let mut tt = TranslationTables::new(ctx.frames).map_err(|_| "tables new failed")?;
    tt.set_device(3, 0, 0x1000, 4, 1);
    test_assert!(ctx.frames.free_frames() < before, "tables must consume frames");
    tt.teardown(ctx.frames);
    test_assert!(
        ctx.frames.free_frames() == before,
        "teardown must return the root and context frames"
    );
    Ok(())
}

// --- Opaque IOVA allocator + non-identity buffer mapping (direct-binding slice 1) ---

/// The IOVA window the buffer tests allocate from. Nonzero (0 is the invalid
/// sentinel) and well clear of the low identity IOVAs the slice-2/3 tests use, so
/// the returned IOVA cannot accidentally equal a physical address under test.
const IOVA_BASE: u64 = 0x4000_0000; // 1 GiB

/// `map_buffer` assigns an opaque, NON-identity IOVA and maps the frame at it;
/// `translate` resolves that IOVA to the physical frame (carrying the offset);
/// `unmap_buffer` clears it. The slice-1 headline: the libOS-facing IOVA is not
/// the physical address, unlike the slice-3 identity block map.
pub fn iova_map_translate_roundtrip(ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut d = Domain::new(ctx.frames, AW, PteFmt::Vtd).map_err(|_| "domain new failed")?;
    let mut alloc = IovaAllocator::new(IOVA_BASE, 16);

    let phys = 0x00AA_0000u64;
    let iova = d
        .map_buffer(ctx.frames, &mut alloc, phys, IOMMU_READ | IOMMU_WRITE)
        .map_err(|_| "map_buffer failed")?;

    // The assigned IOVA is opaque: the window base, NOT the physical address --
    // the property that makes it safe to hand to a library OS.
    test_assert!(iova == IOVA_BASE, "first IOVA must come from the window base");
    test_assert!(iova != phys, "the IOVA must not be the physical address (non-identity)");

    test_assert!(d.translate(iova) == Some(phys), "translate must resolve the mapped frame");
    test_assert!(
        d.translate(iova + 0x40) == Some(phys + 0x40),
        "translate must carry the page offset"
    );

    d.unmap_buffer(&mut alloc, iova).map_err(|_| "unmap_buffer failed")?;
    test_assert!(d.translate(iova).is_none(), "translate after unmap_buffer must be None");

    d.teardown(ctx.frames);
    Ok(())
}

/// Distinct allocations get distinct, page-aligned IOVAs, and freeing one returns
/// it for reuse (LIFO) so paired map/unmap traffic stays within the window rather
/// than walking the cursor off the end.
pub fn iova_allocator_reuse(_ctx: &mut TestCtx) -> Result<(), &'static str> {
    let mut alloc = IovaAllocator::new(IOVA_BASE, 4);
    let a = alloc.alloc().ok_or("first alloc must succeed")?;
    let b = alloc.alloc().ok_or("second alloc must succeed")?;
    test_assert!(a != b, "distinct allocations must be distinct");
    test_assert!(a % 0x1000 == 0 && b % 0x1000 == 0, "IOVAs must be page-aligned");

    alloc.free(b);
    let c = alloc.alloc().ok_or("alloc after free must succeed")?;
    test_assert!(c == b, "a freed IOVA must be reused before the cursor advances");
    Ok(())
}

/// A spent window yields `IovaExhausted` -- both directly from `alloc()` and
/// surfaced through `map_buffer` -- and the mappings made before exhaustion still
/// tear down with no leak.
pub fn iova_exhaustion(ctx: &mut TestCtx) -> Result<(), &'static str> {
    let before = ctx.frames.free_frames();
    let mut d = Domain::new(ctx.frames, AW, PteFmt::Vtd).map_err(|_| "domain new failed")?;
    let mut alloc = IovaAllocator::new(IOVA_BASE, 2); // exactly two IOVAs

    let i0 = d
        .map_buffer(ctx.frames, &mut alloc, 0x10_0000, IOMMU_READ)
        .map_err(|_| "first map_buffer failed")?;
    let i1 = d
        .map_buffer(ctx.frames, &mut alloc, 0x20_0000, IOMMU_READ)
        .map_err(|_| "second map_buffer failed")?;
    test_assert!(i0 != i1, "the two window IOVAs must be distinct");
    test_assert!(
        d.map_buffer(ctx.frames, &mut alloc, 0x30_0000, IOMMU_READ)
            == Err(DomainError::IovaExhausted),
        "a third map must exhaust the two-page window"
    );
    test_assert!(alloc.alloc().is_none(), "alloc must also report the window spent");

    d.teardown(ctx.frames);
    test_assert!(
        ctx.frames.free_frames() == before,
        "teardown must return every frame after exhaustion"
    );
    Ok(())
}

/// The load-bearing no-leak property through the allocator path: several buffers
/// mapped at opaque IOVAs, all table frames returned on teardown and no mapped
/// data frame freed. Non-vacuous: asserts table frames were really consumed first.
pub fn map_buffer_teardown_no_leak(ctx: &mut TestCtx) -> Result<(), &'static str> {
    let before = ctx.frames.free_frames();
    let mut d = Domain::new(ctx.frames, AW, PteFmt::Vtd).map_err(|_| "domain new failed")?;
    let mut alloc = IovaAllocator::new(IOVA_BASE, 8);

    for k in 0..4u64 {
        d.map_buffer(ctx.frames, &mut alloc, 0x10_0000 + k * 0x1000, IOMMU_READ | IOMMU_WRITE)
            .map_err(|_| "map_buffer failed")?;
    }
    test_assert!(ctx.frames.free_frames() < before, "mapping must consume table frames");

    d.teardown(ctx.frames);
    test_assert!(
        ctx.frames.free_frames() == before,
        "teardown must return every table frame (no leak, no over-free)"
    );
    Ok(())
}
