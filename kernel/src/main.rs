//! KonjacOS -- a Limine-booted x86_64 kernel.
//!
//! Boot flow: Limine loads this ELF at the fixed higher-half address from
//! `linker.ld`, sets up long mode + paging + a stack, and jumps to
//! `_entry` (see `boot.rs`) with interrupts disabled. `_entry` enables SSE
//! and then calls [`kstart`], which brings up serial output, installs our
//! own GDT/TSS and IDT (so CPU exceptions print a message instead of
//! silently triple-faulting), reads the framebuffer/memory map Limine
//! handed us, draws a boot splash, brings up the PS/2 keyboard (masking
//! every other legacy-PIC IRQ, since nothing else has a handler yet), and
//! finally hands off to a small interactive shell. There's still no
//! scheduler, no physical memory allocator, no heap -- see the README for
//! a suggested order to add those next.

#![no_std]
#![no_main]

mod apex;
mod ata;
mod boot;
mod cfile;
mod commands;
mod console;
mod cursor;
mod doom_driver;
mod fat16;
mod font;
mod framebuffer;
mod gdt;
mod heap;
mod idt;
mod intrinsics;
mod keyboard;
mod libc_shim;
mod limine;
mod linux_syscall;
mod loader;
mod memory;
mod mouse;
mod paging;
mod pic;
mod pmm;
mod port;
mod serial;
mod sha256;
mod shell;
mod sync;
mod syscall;
mod task;
mod timer;
mod usermode;
mod vm;
mod wm;

use core::panic::PanicInfo;

use framebuffer::Canvas;

const OS_NAME: &str = "KonjacOS";
const OS_VERSION: &str = "0.1.0";

#[unsafe(no_mangle)]
pub extern "C" fn kstart() -> ! {
    serial::SERIAL1.lock().init();

    sprintln!();
    sprintln!("{OS_NAME} v{OS_VERSION} -- booting via Limine");

    unsafe {
        gdt::init();
        idt::init();
        paging::init();
    }
    sprintln!("GDT/TSS and IDT installed -- CPU exceptions are now handled.");
    sprintln!("NX (no-execute) page protection enabled -- see paging.rs and item 27's README entry.");

    if !limine::base_revision_supported() {
        sprintln!("PANIC: Limine did not accept our requested base revision");
        hcf();
    }

    if let Some(info) = limine::bootloader_info() {
        sprintln!("bootloader: {} {}", info.name, info.version);
    } else {
        sprintln!("bootloader: (no bootloader_info response)");
    }

    let hhdm_offset = limine::hhdm_offset().unwrap_or_else(|| {
        sprintln!("PANIC: no HHDM offset from Limine");
        hcf();
    });
    sprintln!("HHDM offset: {hhdm_offset:#x}");

    print_memory_map();

    unsafe {
        memory::init(hhdm_offset);
    }
    let (total_frames, free_frames) = pmm::stats();
    sprintln!(
        "physical memory allocator: {} MiB free / {} MiB total",
        free_frames * pmm::FRAME_SIZE / (1024 * 1024),
        total_frames * pmm::FRAME_SIZE / (1024 * 1024)
    );
    sprintln!("kernel heap mapped and ready.");

    unsafe {
        cfile::init();
    }
    sprintln!("stdout/stderr (C libc shim) ready.");

    unsafe {
        task::init();
    }
    commands::spawn_demo_tasks();
    sprintln!("preemptive scheduler installed -- a couple of demo kernel threads are running.");

    unsafe {
        syscall::init();
        linux_syscall::init();
        usermode::spawn_demo();
    }
    sprintln!("user mode installed -- two isolated ring-3 demo tasks are running via int 0x80 syscalls, each in its own address space.");
    sprintln!("real x86_64 syscall/sysretq gate installed too -- for actual Linux (musl-built) userspace binaries; see linux_syscall.rs.");

    let mut fb_dims: Option<(u64, u64)> = None;
    let have_console = if let Some(fb) = limine::framebuffer() {
        sprintln!(
            "framebuffer: {}x{} @ {} bpp, pitch {}",
            fb.width,
            fb.height,
            fb.bpp,
            fb.pitch
        );
        fb_dims = Some((fb.width, fb.height));
        let mut canvas = Canvas::new(fb);
        canvas.draw_boot_splash();
        sprintln!("boot splash drawn -- {OS_NAME} is alive.");
        console::CONSOLE.lock().attach(canvas);
        true
    } else {
        sprintln!("no framebuffer response from Limine (headless boot?) -- no on-screen console.");
        false
    };

    unsafe {
        keyboard::init();
    }
    sprintln!("PS/2 keyboard driver installed, legacy PIC remapped and masked.");

    if let Some((w, h)) = fb_dims {
        unsafe {
            mouse::init(w, h);
        }
        sprintln!("PS/2 mouse driver installed -- try the `gui` shell command.");
    }

    unsafe {
        timer::init();
    }
    sprintln!("PIT timer installed at {}Hz.", timer::HZ);

    match unsafe { fat16::init() } {
        Ok(()) => sprintln!("FAT16 filesystem mounted from the primary ATA disk."),
        Err(e) => sprintln!("no filesystem mounted ({e}) -- `ls`/`cat` won't work."),
    }

    // Safe to enable interrupts now: every CPU exception has a handler
    // (idt::init(), above), and every unmasked IRQ (keyboard, timer) does
    // too (keyboard::init()/timer::init(), just above).
    unsafe {
        core::arch::asm!("sti");
    }
    sprintln!("interrupts enabled -- handing off to the shell.");

    if have_console {
        shell::run();
    } else {
        sprintln!("no console to run the shell on -- halting.");
        hcf();
    }
}

fn print_memory_map() {
    let Some(entries) = limine::memmap() else {
        sprintln!("no memory map response from Limine");
        return;
    };

    let mut usable_bytes: u64 = 0;
    let mut count = 0u64;
    for entry in entries {
        if entry.kind == limine::MemmapKind::Usable {
            usable_bytes += entry.length;
        }
        count += 1;
    }
    sprintln!(
        "memory map: {count} entries, {} MiB usable",
        usable_bytes / (1024 * 1024)
    );
}

/// "Halt and catch fire": disable interrupts and spin forever executing
/// `hlt`. This is the only sane thing to do when there's no scheduler to
/// hand control back to -- used both for normal end-of-boot and panics.
fn hcf() -> ! {
    unsafe {
        core::arch::asm!("cli");
    }
    loop {
        unsafe {
            core::arch::asm!("hlt");
        }
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    sprintln!("\n*** KERNEL PANIC ***");
    sprintln!("{info}");
    hcf();
}
