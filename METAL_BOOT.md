# Booting Plinth on real hardware

Plinth normally runs under QEMU. It can also boot from a USB stick on a real
UEFI machine and draw its shell -- the "it lives outside a VM" proof. This is a
**live USB**: it runs entirely from the stick and RAM, and the machine's
internal disk is never touched. Plinth has no NVMe or AHCI driver, so it cannot
see an internal disk, let alone write one; remove the stick and reboot and the
machine is exactly as it was.

> **Status: not yet booted on physical hardware.** These are the instructions,
> not a guarantee. The first real run is what turns the open questions
> (real ACPI tables, GOP pixel formats, firmware quirks) into answers. Expect to
> iterate.

## What "success" means here

Success is **boots and draws**: the kernel initializes (paging, memory map,
ACPI, APIC, IOMMU discovery, scheduler) and draws its shell / home screen on the
framebuffer. That is the whole milestone.

It is **not** a working OS on metal. Two things are expected to be absent, and
that is fine:

- **Storage** -- no NVMe/AHCI driver, so the boot is diskless. The storage demos
  simply do not run (proven under QEMU by `cargo xtask smoke-nostorage`).
- **Input** -- best-effort only (see below). If no key ever registers, the
  machine still booted and drew, which is the win.

## 1. Build the image

```text
cargo xtask image
```

This stages `plinth-usb.img` (a FAT32 UEFI removable-media image with the kernel
at `\EFI\BOOT\BOOTX64.EFI`) and prints its path, size, and write instructions.
The staged image is the **scripted** build; it self-drives the demo tour and
does not wait for input.

## 2. Write it to a USB stick

Identify the target device first and check it twice -- writing to the wrong disk
destroys it. xtask deliberately does not write the device for you.

- **Windows:** [Rufus](https://rufus.ie) in **DD mode**, or find the disk number
  with `wmic diskdrive list brief` and use a raw image writer.
- **Linux:** `lsblk` to find the device, then
  `sudo dd if=<path-to>/plinth-usb.img of=/dev/sdX bs=4M status=progress conv=fsync`.

## 3. Firmware settings (the usual cause of a first-attempt failure)

In the target machine's firmware setup:

- **Boot in UEFI mode**, not CSM / legacy BIOS. Plinth is a UEFI application.
- **Secure Boot OFF.** The image is unsigned; Secure Boot will refuse it.
- **USB boot enabled** and the stick ahead of the internal disk in the boot
  order (or use the one-time boot menu).
- **USB Legacy Support ON** *if* you intend to use a **USB** keyboard. Plinth has
  no USB HID driver yet, so a USB keyboard works only through the firmware's
  legacy (SMM) emulation, which presents it at the PS/2 ports. This is common on
  desktops but unreliable on pure-UEFI systems and laptops. A **PS/2** keyboard
  on a machine with a real PS/2 port works directly and needs none of this.

None of these touch the internal disk; they are all boot/firmware settings.

## 4. What you should see

- The shell's home screen drawn on the display.
- If the machine has no serial port, kernel diagnostics render to the framebuffer
  console instead (the no-serial path, exercised under QEMU by
  `cargo xtask smoke-fbcon-shell`). A boot failure that reaches the kernel draws
  a panic line on screen rather than hanging black -- so a **black screen**
  usually means the firmware never launched the image (recheck section 3), while
  a **panic line** means it booted far enough to report why it stopped.

## Reverting

Remove the stick and reboot. Nothing was installed and no disk was written.

## Not on this path

A real storage driver (NVMe/AHCI) and a real USB HID stack (xHCI + USB-HID) are
each their own milestone; they are deliberately out of scope for first boot. See
[ROADMAP.md](ROADMAP.md).
