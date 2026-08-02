//! The shell -- the visual userspace skin (D8, Design/display_skin.md): a splash,
//! a home screen of app icons, keyboard navigation, a mouse cursor, and
//! launching by click (Design/clickable_apps.md slices 1 and 2).
//!
//! It holds the whole-screen `Framebuffer` (FB_SLOT), the keyboard `EventSource`
//! (KBD_SLOT) and the mouse `EventSource` (MOUSE_SLOT). Two icons are shell-drawn
//! "views"; the other two are real apps the shell `spawn`s, handing each the
//! framebuffer -- the display capability as transferable focus. Scripted scancode
//! and mouse-packet sequences (armed kernel-side) drive it deterministically; each
//! fixed frame's top-left square is hashed to serial.
//!
//! **The two apps differ in how they give the screen back, and that is the point.**
//! APP (`shellapp`) transfers the capability back over the spawn result channel and
//! exits cleanly (D6c). CRASH (`fbreclaimchild`) faults while still holding it and
//! hands back nothing; the kernel reclaims the capability on teardown and reports
//! where it landed on the death wake (ABI v2.9). The shell's recovery is identical
//! either way, down to the frame hash -- which is what makes a crashing app
//! survivable rather than terminal for the display (C6).
//!
//! All of it -- splash, layout, icons, the pointer, hit-testing, the launch
//! handoff -- is unprivileged policy over the framebuffer/keyboard/mouse
//! capabilities + spawn + IPC. The kernel draws nothing and knows nothing of
//! icons, cursors, or "apps". It does not even own a pointer position: it ships
//! raw `(dx, dy, buttons)` packets and this file decides what they mean (I1, C8).
//!
//! **Both input sources ride ONE ring.** The `sys_event_recv` shim libinput's
//! `read_key` uses keeps a *single* subscription and cancels it when the source
//! changes, so it cannot serve two devices; the libos ring can, and the kernel's
//! subscription table has always allowed two sources on one ring. So this is the
//! first userspace unified input loop: two `subscribe`s, one `ring_wait`,
//! `select2` taking whichever packet lands first.

#![no_std]
#![no_main]

use libgfx::{text_width, Framebuffer};
use libinput::{Key, Keymap};
use libos::ring::{self, Either};
use libplinth::{
    event_code, event_kind, event_value, mouse_buttons, mouse_dx, mouse_dy, sys_cap_release,
    sys_exit, sys_recv_cap, sys_spawn, sys_write, write_dec, write_hex, ABI_VERSION, EVENT_KEY,
    EVENT_MOUSE_MOVE, FB_SLOT, IPC_OK, IPC_PEER_DIED, MAP_BASE, NO_CAP, SYS_ERR,
};

/// The keyboard EventSource lands in the slot after the framebuffer, and the
/// mouse in the slot after that (a single-process run mints grants in order:
/// Framebuffer at FB_SLOT = 1, then 2, then 3). These three constants and the
/// kernel's grant array in main.rs are one decision written in two places --
/// the K-009 shape -- so both ends carry a comment naming the other.
const KBD_SLOT: u64 = 2;
const MOUSE_SLOT: u64 = 3;
const HASH_SIDE: u32 = 128;

/// A 2x2 icon grid; index = row*2 + col. Two shell-drawn views + two real apps.
const ICON_LABELS: [&[u8]; 4] = [b"INFO", b"BARS", b"CRASH", b"APP"];

/// Which icons launch a real process, and which `SPAWNABLE` id each one is
/// (Design/clickable_apps.md C3). `None` is a shell-drawn view.
///
/// This table is library-OS policy and deliberately NOT a kernel registry: the
/// kernel exposes `SPAWNABLE` ids and `sys_spawn`, and which of them are offered
/// to a user, under what name and in what order, is presentation. A kernel-side
/// app registry would be the kernel deciding what a user may launch, which is N3
/// arbitration.
///
/// The cost, disclosed rather than hidden (N7): these ids mirror the ORDER of the
/// kernel's `SPAWNABLE` array in main.rs, and nothing checks that they agree. That
/// array carries an "append only" warning naming this hazard, and K-009 is what it
/// looks like when it bites. Two entries is still comfortably a comment's worth of
/// coupling; a generated table is the answer if this grows.
const ICON_APPS: [Option<u64>; 4] = [
    None,                       // INFO  -- shell-drawn view
    None,                       // BARS  -- shell-drawn view
    Some(FBRECLAIMCHILD_ID),    // CRASH -- faults while holding the screen
    Some(SHELLAPP_ID),          // APP   -- draws and hands the screen back
];

/// shellapp's index in the kernel's SPAWNABLE table (see main.rs). It draws and
/// transfers the framebuffer back, then exits cleanly.
const SHELLAPP_ID: u64 = 3;
/// fbreclaimchild's index in the same table. It maps the screen, draws to prove
/// it really holds it, and then FAULTS while still holding it -- the case the
/// whole reclamation milestone exists for (Design/cap_reclaim.md D6, C6 here).
const FBRECLAIMCHILD_ID: u64 = 5;

/// Icon cell geometry. The *spacing* (`ICON_W + ICON_GAP`, `ICON_H + ICON_GAP`)
/// is resolution-independent even though the grid ORIGIN is centred, which is
/// what lets a scripted pointer journey move between icons in fixed deltas on
/// any resolution (Design/clickable_apps.md C7).
const ICON_W: u32 = 200;
const ICON_H: u32 = 120;
const ICON_GAP: u32 = 48;

/// Side of the square pointer, in pixels.
const CURSOR_SIDE: u32 = 8;

const BG: (u8, u8, u8) = (0x10, 0x10, 0x20);
const FG: (u8, u8, u8) = (0xE0, 0xE0, 0xF0);
const BAR: (u8, u8, u8) = (0x28, 0x28, 0x40);
const ICON_BORDER: (u8, u8, u8) = (0x60, 0x60, 0x80);
const SEL_BORDER: (u8, u8, u8) = (0xF0, 0xC0, 0x40);
const CURSOR_FG: (u8, u8, u8) = (0xFF, 0xFF, 0xFF);

/// The pointer: a position, a visibility flag, and the pixels it is covering.
///
/// Save-under (Design/clickable_apps.md C4): the block beneath the pointer is
/// captured before it is drawn and written back before it moves, so a pointer
/// move repaints `2 * CURSOR_SIDE^2` pixels instead of the screen. A full repaint
/// per packet would undo the flicker fix in `cc5c6f7`, which is visible because
/// there is no back buffer.
///
/// The saved pixels are RAW native words, not (r, g, b): a colour round-trip is
/// lossy on `FB_FMT_U8` and would discolour whatever the pointer passed over.
struct Cursor {
    x: u32,
    y: u32,
    shown: bool,
    save: [u32; (CURSOR_SIDE * CURSOR_SIDE) as usize],
    /// Which icon the pointer is currently over, as last reported to serial.
    /// `None` = over no icon. Held so motion emits a line only when the answer
    /// CHANGES -- six packets crossing one boundary is one line, not six.
    over: Option<usize>,
}

impl Cursor {
    const fn new() -> Cursor {
        Cursor {
            x: 0,
            y: 0,
            shown: false,
            save: [0; (CURSOR_SIDE * CURSOR_SIDE) as usize],
            over: None,
        }
    }

    /// Capture the block under the pointer and draw it. No-op if already shown.
    fn show(&mut self, fb: &Framebuffer) {
        if self.shown {
            return;
        }
        let mut dy = 0u32;
        while dy < CURSOR_SIDE {
            let mut dx = 0u32;
            while dx < CURSOR_SIDE {
                let i = (dy * CURSOR_SIDE + dx) as usize;
                self.save[i] = fb.read_pixel_raw(self.x + dx, self.y + dy);
                fb.put_pixel(self.x + dx, self.y + dy, CURSOR_FG.0, CURSOR_FG.1, CURSOR_FG.2);
                dx += 1;
            }
            dy += 1;
        }
        self.shown = true;
    }

    /// Write the saved block back. No-op if already hidden.
    ///
    /// Every full redraw (`draw_home`, `draw_view`) and every hash MUST hide the
    /// pointer first: the saved block goes stale the moment anything else paints
    /// over it, and restoring a stale block would smear the old pixels back onto
    /// the new screen.
    fn hide(&mut self, fb: &Framebuffer) {
        if !self.shown {
            return;
        }
        let mut dy = 0u32;
        while dy < CURSOR_SIDE {
            let mut dx = 0u32;
            while dx < CURSOR_SIDE {
                let i = (dy * CURSOR_SIDE + dx) as usize;
                fb.write_pixel_raw(self.x + dx, self.y + dy, self.save[i]);
                dx += 1;
            }
            dy += 1;
        }
        self.shown = false;
    }

    /// Forget the saved block without painting it back.
    ///
    /// For the one case where restoring would be wrong: the framebuffer was
    /// handed to another process and came back with a different picture on it, so
    /// the pixels this pointer saved no longer exist anywhere.
    fn invalidate(&mut self) {
        self.shown = false;
    }
}

fn draw_splash(fb: &Framebuffer) {
    let info = fb.info();
    fb.fill_rect(0, 0, info.width, info.height, BG.0, BG.1, BG.2);
    fb.draw_text_centered(info.width / 2, info.height / 2 - 30, b"PLINTH", FG, BG, 6);
    // "VERSION " + libplinth's ABI_VERSION, assembled on the stack -- no_std has
    // no format!. Read from libplinth rather than written out here so the splash
    // cannot fall behind the ABI again, as it did through the v2.8 bump.
    const PREFIX: &[u8] = b"VERSION ";
    let mut banner = [0u8; 24];
    let n = PREFIX.len() + ABI_VERSION.len();
    banner[..PREFIX.len()].copy_from_slice(PREFIX);
    banner[PREFIX.len()..n].copy_from_slice(ABI_VERSION);
    // Well below the hashed top-left square, so the splash hash is unaffected.
    fb.draw_text_centered(info.width / 2, info.height / 2 + 36, &banner[..n], FG, BG, 2);
    // ...which is exactly why the drawn banner needs a second, ASSERTED copy.
    // Nothing in the suite can see a framebuffer region no hash covers, so for
    // the whole of v2.8 the splash could have kept saying 2.7 (it did) with every
    // test green. Emitting the SAME buffer to serial, from the same expression,
    // means `expected_boot_log.txt` pins the version the binaries were actually
    // built against and the two cannot drift apart again.
    //
    // The pointer is the same shape of problem and gets the same answer: it lives
    // outside the hashed square too, so `report_over` and `report_click` below
    // emit its state to serial rather than trusting a frame hash to catch it
    // (Design/clickable_apps.md C4).
    sys_write(b"shell: ");
    sys_write(&banner[..n]);
    sys_write(b"\n");
}

/// Rectangle (x, y, w, h) of icon `idx` in a 2x2 grid centered on screen.
fn icon_rect(w: u32, h: u32, idx: usize) -> (u32, u32, u32, u32) {
    let grid_w = ICON_W * 2 + ICON_GAP;
    let grid_h = ICON_H * 2 + ICON_GAP;
    let ox = (w - grid_w) / 2;
    let oy = (h - grid_h) / 2 + 30;
    let col = (idx % 2) as u32;
    let row = (idx / 2) as u32;
    (ox + col * (ICON_W + ICON_GAP), oy + row * (ICON_H + ICON_GAP), ICON_W, ICON_H)
}

/// Which icon covers point (x, y), if any. The hit-test half of clickable apps;
/// it is pure geometry over `icon_rect`, so it is the same function whether the
/// point came from a real mouse or the scripted sequence.
fn hit_test(w: u32, h: u32, x: u32, y: u32) -> Option<usize> {
    let mut idx = 0usize;
    while idx < 4 {
        let (ix, iy, iw, ih) = icon_rect(w, h, idx);
        if x >= ix && x < ix + iw && y >= iy && y < iy + ih {
            return Some(idx);
        }
        idx += 1;
    }
    None
}

/// Emit an icon index, or `none`.
fn write_icon(idx: Option<usize>) {
    match idx {
        Some(i) => {
            sys_write(b"icon ");
            write_dec(i as u64);
        }
        None => {
            sys_write(b"none");
        }
    }
}

/// Paint one icon cell: the box, its border, and its label.
///
/// The cell is refilled before the border is drawn, and `draw_border` paints
/// just *inside* the rectangle, so a previously-thicker selection border is
/// fully erased. That makes this pixel-identical to what `draw_home` draws for
/// the same icon -- which is what lets a selection move repaint two cells
/// instead of the whole screen.
fn draw_icon(fb: &Framebuffer, sw: u32, sh: u32, idx: usize, selected: bool) {
    let (x, y, w, h) = icon_rect(sw, sh, idx);
    fb.fill_rect(x, y, w, h, BAR.0, BAR.1, BAR.2);
    let (border, t) = if selected { (SEL_BORDER, 4) } else { (ICON_BORDER, 2) };
    fb.draw_border(x, y, w, h, t, border.0, border.1, border.2);
    let label = ICON_LABELS[idx];
    let lw = text_width(label, 3);
    fb.draw_text(x + (w - lw) / 2, y + h / 2 - 12, label, FG, BAR, 3);
}

/// Move the selection highlight by repainting only the two cells whose borders
/// change.
///
/// A full `draw_home` here would clear the entire screen and repaint it on every
/// arrow press. There is no back buffer -- drawing goes straight to the scanned-
/// out framebuffer -- so that full-screen clear is visible as a flash. The icons
/// are disjoint from each other, from the title bar, and from the bottom hint,
/// so repainting just these two cells leaves the screen in exactly the state a
/// full `draw_home` would have produced.
fn move_selection(fb: &Framebuffer, cur: &mut Cursor, prev: usize, sel: usize) {
    if prev == sel {
        return;
    }
    let info = fb.info();
    // The repainted cells may lie under the pointer, which would strand a stale
    // save-under block. Hide first, repaint, then re-capture.
    cur.hide(fb);
    draw_icon(fb, info.width, info.height, prev, false);
    draw_icon(fb, info.width, info.height, sel, true);
    cur.show(fb);
}

fn draw_home(fb: &Framebuffer, sel: usize) {
    let info = fb.info();
    fb.fill_rect(0, 0, info.width, info.height, BG.0, BG.1, BG.2);
    // Title bar (lands in the hashed top-left square).
    fb.fill_rect(0, 0, info.width, 40, BAR.0, BAR.1, BAR.2);
    fb.draw_text(8, 8, b"PLINTH HOME", FG, BAR, 3);
    let mut i = 0;
    while i < 4 {
        draw_icon(fb, info.width, info.height, i, i == sel);
        i += 1;
    }
    // Controls hint along the bottom. It sits well below the hashed top-left
    // square, so it does not affect the determinism hash.
    fb.draw_text_centered(
        info.width / 2,
        info.height - 48,
        b"ARROWS MOVE   ENTER OPEN   CLICK   Q QUIT",
        FG,
        BG,
        2,
    );
}

fn draw_view(fb: &Framebuffer, label: &[u8]) {
    let info = fb.info();
    let bg = (0x18u8, 0x10u8, 0x28u8);
    fb.fill_rect(0, 0, info.width, info.height, bg.0, bg.1, bg.2);
    fb.draw_text_centered(info.width / 2, info.height / 2 - 24, label, FG, bg, 4);
    fb.draw_text_centered(info.width / 2, info.height / 2 + 28, b"BACKSPACE TO RETURN", FG, bg, 2);
}

/// Hash the fixed top-left square and emit it, with the pointer hidden.
///
/// Hiding here is hash HYGIENE, not the pointer's assertion strategy -- those are
/// two different things and C4 ruled on the second. The pointer is asserted by
/// `report_over` / `report_click`; hiding it for the hash exists so the pointer
/// can never perturb an existing expectation by wandering into the top-left
/// square, which would surface as an inscrutable hash mismatch far from its
/// cause. Existing hashes therefore keep their exact pre-pointer values.
fn emit_hash(tag: &[u8], fb: &Framebuffer, cur: &mut Cursor) {
    let was_shown = cur.shown;
    cur.hide(fb);
    sys_write(tag);
    write_hex(fb.hash_origin_square(HASH_SIDE));
    sys_write(b"\n");
    if was_shown {
        cur.show(fb);
    }
}

/// Launch the app: hand it the framebuffer, wait for it back, remap and redraw.
///
/// Returns the framebuffer and the slot the capability came home in. Both the
/// Enter key and a pointer click reach this, so the launch sequence is written
/// ONCE (I7): the wait-handle release, the slot migration and the remap are
/// exactly the kind of decision that drifts when a second call site copies it.
fn launch_app(
    stale_fb: Framebuffer,
    fb_slot: u64,
    app_id: u64,
    sel: usize,
    cur: &mut Cursor,
) -> (Framebuffer, u64) {
    // D6c: spawn the app and hand it the framebuffer; the spawn transfer revokes
    // + unmaps our framebuffer here. The pointer goes with the screen -- and the
    // block it saved describes a picture that will not exist when we get the
    // screen back, so drop it rather than restore it.
    cur.invalidate();
    // Taken BY VALUE and dropped before the transfer, deliberately: the mapping
    // this handle describes is torn down by `sys_spawn` below, so touching it
    // afterwards would fault. Consuming it here makes that unrepresentable rather
    // than merely documented -- the caller cannot hold a framebuffer across the
    // handoff, and must use the one returned at the end.
    drop(stale_fb);
    sys_write(b"shell: launching app\n");
    let handle = sys_spawn(app_id, fb_slot);
    if handle == SYS_ERR {
        sys_write(b"shell: spawn failed\n");
        sys_exit(2);
    }
    // Join. There are TWO ways the screen comes back, and the shell must survive
    // both -- this is C6, and it is the reason the reclamation milestone exists.
    //
    //   IPC_OK         the app drew, transferred the capability back over this
    //                  channel, and exited cleanly. The cooperative path.
    //   IPC_PEER_DIED  the app FAULTED while still holding the screen. Nobody
    //                  handed anything back; the kernel reclaimed the capability
    //                  on teardown and, since ABI v2.9, reports the slot it landed
    //                  in on this very wake.
    //
    // What matters is that the shell does the SAME thing afterwards either way:
    // remap at the reported slot and redraw. Before reclamation the second case
    // was unrecoverable -- no syscall mints a framebuffer, so nothing in userspace
    // could draw again for the rest of the boot, and a shell that launched a
    // crashing app would have taken the display down with it.
    let (status, _msg, cap_slot) = sys_recv_cap(handle);
    match status {
        IPC_OK => {
            sys_write(b"shell: app returned the screen\n");
        }
        IPC_PEER_DIED => {
            sys_write(b"shell: app died holding the screen\n");
        }
        _ => {
            sys_write(b"shell: unexpected wake from the app\n");
            sys_exit(3);
        }
    }
    if cap_slot == NO_CAP {
        // Said plainly rather than exiting quietly. On the death path this is the
        // K-023 shape: the capability may well be sitting in this table unfound,
        // but nothing can enumerate a capability table, so the honest claim is
        // only that the wake carried no slot.
        sys_write(b"shell: no landing slot -- the screen did not come back\n");
        sys_exit(3);
    }
    // The framebuffer came back at cap_slot, not FB_SLOT; track it so the next
    // launch transfers the right capability.
    let new_slot = cap_slot;
    // The join is over, so the wait handle names an endpoint whose child is gone
    // -- dead weight in a 16-slot table. Release it, or every launch costs a slot
    // permanently and the ninth or so spawn fails with the table full (the
    // 2026-06-27 crash; there was no way to say this before ABI v2.8's
    // cap_release). Release AFTER the recv, not before: the handle is what we
    // received on, and the returned framebuffer has already landed at cap_slot,
    // so freeing this slot cannot disturb it.
    if sys_cap_release(handle) != 0 {
        sys_write(b"shell: releasing the spent wait handle failed\n");
        sys_exit(4);
    }
    // Re-map before drawing. The spawn transfer took the framebuffer mapping down
    // with the capability, so the old mapping is gone and touching it would fault
    // -- which is the point: this shell can draw because it holds the capability
    // and mapped it, not because a page-table entry happened to survive the
    // handoff (Design/fb_mapping.md D3).
    let fb = match Framebuffer::map(new_slot, MAP_BASE) {
        Some(fb) => fb,
        None => {
            sys_write(b"shell: remap after launch failed\n");
            sys_exit(5);
        }
    };
    draw_home(&fb, sel);
    emit_hash(b"shell: back home hash ", &fb, cur);
    cur.show(&fb);
    (fb, new_slot)
}

/// Open icon `sel`: launch it if it is the app, otherwise draw its view and wait
/// for Backspace. The shared tail of "Enter was pressed" and "an icon was
/// clicked" -- one decision, one place (I7).
///
/// Returns the (possibly new) framebuffer and slot, since launching moves the
/// capability.
fn open_icon(
    fb: Framebuffer,
    fb_slot: u64,
    sel: usize,
    cur: &mut Cursor,
    kbd: &mut ring::EventStream,
    keymap: &mut Keymap,
) -> (Framebuffer, u64) {
    if let Some(app_id) = ICON_APPS[sel] {
        return launch_app(fb, fb_slot, app_id, sel, cur);
    }
    // A shell-drawn view; any key but the kernel-scripted Backspace would also
    // work -- the demo scripts Backspace.
    cur.hide(&fb);
    draw_view(&fb, ICON_LABELS[sel]);
    emit_hash(b"shell: view hash ", &fb, cur);
    // Keyboard-only wait: a pointer has nothing to click on in a view, and any
    // mouse packets that arrive meanwhile stay queued on their own cookie in the
    // reactor rather than being dropped, so none are lost.
    loop {
        let ev = ring::block_on(kbd.next());
        if event_kind(ev) != EVENT_KEY {
            continue;
        }
        match keymap.feed(event_kind(ev), event_code(ev), event_value(ev)) {
            Key::Backspace => {
                draw_home(&fb, sel);
                cur.show(&fb);
                break;
            }
            // Quit works from a view, not just from home.
            //
            // This was found by the pointer, and it is a real defect rather than a
            // test inconvenience: before clicking existed the tour reached a view
            // only via a scripted Enter and always left via the scripted
            // Backspace, so "the only exit from a view is Backspace" was never
            // exercised against anything else. A click changes the selection, so
            // the key that follows is no longer the key the script assumed, and
            // the shell sat in a view waiting for a Backspace that was never
            // coming -- a hang, on a screen whose own hint line says Q QUIT.
            //
            // A UI where the quit key works on some screens and silently does
            // nothing on others is wrong however it is driven, so the fix belongs
            // here rather than in the scripted sequence.
            Key::Char(b'q') | Key::Char(b'Q') => {
                sys_write(b"shell: quit\n");
                sys_exit(0);
            }
            _ => {}
        }
    }
    (fb, fb_slot)
}

#[no_mangle]
pub extern "C" fn _start(_idx: u64) -> ! {
    sys_write(b"shell: start\n");

    // Rebindable: the mapping is torn down whenever the capability leaves this
    // table, so every launch round-trip ends with a fresh `map` (ABI v2.8 fix,
    // Design/fb_mapping.md D1/D3).
    let mut fb = match Framebuffer::map(FB_SLOT, MAP_BASE) {
        Some(fb) => fb,
        None => {
            sys_write(b"shell: map failed\n");
            sys_exit(1);
        }
    };

    // One ring, two subscriptions. This is what the shim `read_key` uses cannot
    // do: it keeps a single subscription and cancels it on a source change.
    if !ring::init() {
        sys_write(b"shell: ring init failed\n");
        sys_exit(6);
    }
    let mut kbd = ring::subscribe(KBD_SLOT);
    let mut mouse = ring::subscribe(MOUSE_SLOT);

    let mut cur = Cursor::new();

    draw_splash(&fb);
    emit_hash(b"shell: splash hash ", &fb, &mut cur);

    let mut sel = 0usize;
    draw_home(&fb, sel);

    // Park the pointer on the centre of the selected icon rather than at a screen
    // corner. This is real behaviour (the pointer starts where the attention is),
    // and it is also what makes the scripted journey resolution-independent: from
    // an icon centre, every other icon is a FIXED delta away -- ICON_W + ICON_GAP
    // across, ICON_H + ICON_GAP down -- on any screen size, even though the grid
    // origin itself is not (Design/clickable_apps.md C7).
    {
        let info = fb.info();
        let (ix, iy, iw, ih) = icon_rect(info.width, info.height, sel);
        cur.x = ix + iw / 2;
        cur.y = iy + ih / 2;
        cur.over = hit_test(info.width, info.height, cur.x, cur.y);
    }
    emit_hash(b"shell: home hash ", &fb, &mut cur);
    cur.show(&fb);
    sys_write(b"shell: cursor over ");
    write_icon(cur.over);
    sys_write(b"\n");

    // The framebuffer capability's CURRENT slot. It starts at FB_SLOT, but each
    // launch round-trip MOVES it: `sys_spawn` transfers it out (freeing FB_SLOT,
    // into which the wait handle is then minted), and the app hands it back via
    // `recv_cap`, which mints it into the next free slot -- NOT back at FB_SLOT.
    // So we must remember where it landed and transfer THAT slot next time, or a
    // second launch would hand the app the stale wait-handle instead.
    //
    // The observed values are 1 -> 7 on the first launch and 7 -> 7 on every one
    // after, and that stability is MISLEADING. It invites the conclusion that only
    // the first launch migrates anything, so the second launch adds no coverage
    // and the D7 relaunch guard is weaker than it claims. That conclusion is
    // wrong, and it was reached and retracted on 2026-08-01 -- recorded here
    // because the numbers will look just as suspicious to the next person who
    // prints them.
    //
    // The guard is not about the migration RECURRING. It is about whether this
    // shell USES the migrated value on the next transfer, and that is observable
    // only on a relaunch. Verified rather than argued: reporting a stale slot here
    // while leaving the remap correct lets the first launch complete normally and
    // makes the SECOND fail -- the app receives the wrong capability, never maps
    // the framebuffer and never draws. Breaking the remap instead is too blunt to
    // show this; it fails at the first launch and proves only that some slot
    // matters.
    let mut fb_slot = FB_SLOT;

    let mut keymap = Keymap::new();
    let mut buttons_prev = 0u8;
    loop {
        // One ring_wait serves both devices; whichever posts first wins. Biased to
        // the keyboard, so a saturated keyboard would starve the pointer -- see
        // `select2`. Neither scripted sequence can saturate anything.
        match ring::block_on(ring::select2(kbd.next(), mouse.next())) {
            Either::A(ev) => {
                if event_kind(ev) != EVENT_KEY {
                    continue;
                }
                match keymap.feed(event_kind(ev), event_code(ev), event_value(ev)) {
                    // 2x2 grid: up/down toggle the row, left/right toggle the column.
                    Key::Up | Key::Down => {
                        let prev = sel;
                        sel ^= 2;
                        move_selection(&fb, &mut cur, prev, sel);
                    }
                    Key::Left | Key::Right => {
                        let prev = sel;
                        sel ^= 1;
                        move_selection(&fb, &mut cur, prev, sel);
                    }
                    Key::Enter => {
                        let (nfb, nslot) =
                            open_icon(fb, fb_slot, sel, &mut cur, &mut kbd, &mut keymap);
                        fb = nfb;
                        fb_slot = nslot;
                    }
                    Key::Char(b'q') | Key::Char(b'Q') => {
                        // No farewell frame: the kernel terminates QEMU shortly
                        // after this exit (isa-debug-exit is attached on every
                        // path since 2026-07-25), so anything drawn here would be
                        // on screen for microseconds.
                        sys_write(b"shell: quit\n");
                        sys_exit(0);
                    }
                    _ => {}
                }
            }
            Either::B(ev) => {
                if event_kind(ev) != EVENT_MOUSE_MOVE {
                    continue;
                }
                let info = fb.info();
                // Accumulate and clamp: the kernel ships a relative sample and owns
                // no pointer, so the position, the screen bounds and the sign
                // convention are all decided here (I1, C8). PS/2 reports +Y as UP,
                // and screen Y grows DOWN, so dy is subtracted -- the kernel has no
                // opinion either way.
                let w = info.width as i64;
                let h = info.height as i64;
                let nx = (cur.x as i64 + mouse_dx(ev) as i64).clamp(0, w - CURSOR_SIDE as i64);
                let ny = (cur.y as i64 - mouse_dy(ev) as i64).clamp(0, h - CURSOR_SIDE as i64);
                if nx as u32 != cur.x || ny as u32 != cur.y {
                    cur.hide(&fb);
                    cur.x = nx as u32;
                    cur.y = ny as u32;
                    cur.show(&fb);
                }
                // Report only when the ANSWER changes, not per packet: the pointer
                // lives outside the hashed square, so this line is the only thing
                // that can catch it being wrong (C4), and a fixed-delta journey
                // that lands somewhere else at a different resolution shows up
                // here as a loud mismatch rather than a silent miss (C7).
                let over = hit_test(info.width, info.height, cur.x, cur.y);
                if over != cur.over {
                    cur.over = over;
                    sys_write(b"shell: cursor over ");
                    write_icon(over);
                    sys_write(b"\n");
                }
                // A click is the press EDGE, not the button being held: one packet
                // per PS/2 sample means a held button would otherwise re-fire on
                // every sample.
                let buttons = mouse_buttons(ev);
                let pressed = buttons != 0 && buttons_prev == 0;
                buttons_prev = buttons;
                if pressed {
                    sys_write(b"shell: click ");
                    write_icon(over);
                    sys_write(b"\n");
                    if let Some(idx) = over {
                        // A click selects what it hits and opens it, so the pointer
                        // and the keyboard converge on one path (I7).
                        let prev = sel;
                        sel = idx;
                        move_selection(&fb, &mut cur, prev, sel);
                        let (nfb, nslot) =
                            open_icon(fb, fb_slot, sel, &mut cur, &mut kbd, &mut keymap);
                        fb = nfb;
                        fb_slot = nslot;
                    }
                }
            }
        }
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    sys_exit(111);
}
