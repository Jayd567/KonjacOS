//! The desktop: a liquid-glass compositor, window manager and taskbar,
//! drawn entirely in software on the framebuffer Limine hands us. See
//! `desktop.rs` for the overall design and `glass.rs` for the material.

pub mod assets;
mod apps;
mod desktop;
mod font;
mod glass;
mod icon_ids;
mod icons;
mod math;
mod sketch;
mod surface;
mod sysmon;

use crate::console;
use crate::framebuffer::Canvas;
use crate::sync::SpinLock;
use crate::task;

/// The framebuffer, in transit from the console to the desktop task.
static CANVAS: SpinLock<Option<Canvas>> = SpinLock::new(None);

/// Starts the desktop. The console switches to grid mode right here,
/// synchronously -- before the shell prints its first line -- so nothing
/// is ever drawn over the boot logo; the desktop task picks the
/// framebuffer up when it first runs.
pub fn start() {
    let canvas = console::CONSOLE.lock().enter_grid(apps::Terminal::COLS, apps::Terminal::ROWS);
    let Some(canvas) = canvas else { return };
    *CANVAS.lock() = Some(canvas);
    sysmon::start();
    task::spawn_with_stack("desktop", desktop::run, 256 * 1024);
}

fn take_canvas() -> Option<Canvas> {
    CANVAS.lock().take()
}
