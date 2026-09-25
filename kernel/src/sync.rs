//! Spinlocks. [`SpinLock`] is the original minimal one -- no interrupt-
//! safety, no fairness -- just enough to guard a handful of global
//! singletons (the serial port, the framebuffer console, the physical
//! frame allocator, ...) that are only ever touched from ordinary task
//! context. [`IrqSpinLock`] is for the one exception that needs more (see
//! its own doc comment): `task::TASKS`, which a timer-tick ISR also locks.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

pub struct SpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// Safety: access to `value` is only ever handed out through `lock()`, which
// enforces mutual exclusion via `locked`.
unsafe impl<T: Send> Sync for SpinLock<T> {}

pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        SpinLock {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        SpinLockGuard { lock: self }
    }
}

impl<T> core::ops::Deref for SpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> core::ops::DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

/// An interrupt-safe spinlock -- like [`SpinLock`], but `lock()` disables
/// interrupts for as long as the guard lives, restored to whatever they
/// actually were right before the call (not unconditionally re-enabled) on
/// drop, so nesting under an already-`cli`'d caller stays safe.
///
/// # The deadlock this exists to prevent (found via item 28's DOOM window
/// work)
/// Every IRQ handler on this single-core kernel runs through an interrupt
/// gate (see `idt.rs`'s `0x8E` type-attr byte), which the CPU itself clears
/// `IF` for on entry -- so an ISR can never be preempted by another
/// interrupt, timer tick included, for as long as it runs. `timer.rs`'s own
/// tick handler calls `task::schedule()`, which locks `task::TASKS`. Plenty
/// of ordinary task-context code (`task_exit`, `spawn`, `kill`, ...) locks
/// that same `TASKS` table too. If a plain (non-interrupt-safe) lock guarded
/// it and a timer tick landed while task-context code held that lock, the
/// tick handler's own attempt to lock it would spin forever: it can't be
/// preempted (IF already clear), and the actual holder can never run again
/// to release it (nothing else is running on a single core to switch back to
/// it). Busy-spinning at ~100% CPU, no crash, no further output -- exactly
/// what a full system hang looked like when `doom_driver.rs`'s new close-
/// button handler (`task::exit_current` -> `task_exit` -> `TASKS.lock()`)
/// hit this window, which was rare enough (a ~100Hz tick against a handful-
/// of-instructions critical section) to pass plenty of earlier testing
/// before finally reproducing here.
///
/// This is deliberately its *own* type rather than a change to every
/// [`SpinLock`] in the kernel: an earlier attempt at exactly that (make
/// `SpinLock` itself always `cli` while held) fixed this deadlock but broke
/// DOOM's own startup instead -- it stalled forever partway through loading
/// status-bar graphics (`ST_Init`/`Z_Malloc`/disk reads), something that had
/// always worked fine before. Something in that path apparently depends on
/// interrupts actually staying enabled across an unrelated lock elsewhere
/// (`console::CONSOLE` and `heap`'s allocator lock were the suspects, never
/// pinned down further since the narrower fix below sidesteps needing to).
/// `task::TASKS` is the only lock actually shared between task context and
/// an ISR, so it's the only one that actually needs this; every other lock
/// in the kernel keeps the plain, cheaper [`SpinLock`] unchanged.
pub struct IrqSpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for IrqSpinLock<T> {}

pub struct IrqSpinLockGuard<'a, T> {
    lock: &'a IrqSpinLock<T>,
    /// Whether `IF` was actually set right before this guard cleared it --
    /// `false` means some outer lock (or other `cli` caller) already had
    /// interrupts off, so `Drop` must leave them off rather than turning
    /// them back on out from under that outer holder.
    restore_interrupts: bool,
}

/// Reads `IF` out of `rflags` without disturbing it -- `pushfq`/`pop` into a
/// GPR, the standard no_std way to read flags that don't have their own
/// dedicated read instruction the way `cli`/`sti` write them.
#[inline]
fn interrupts_enabled() -> bool {
    let flags: u64;
    unsafe {
        core::arch::asm!("pushfq", "pop {}", out(reg) flags);
    }
    flags & (1 << 9) != 0
}

impl<T> IrqSpinLock<T> {
    pub const fn new(value: T) -> Self {
        IrqSpinLock {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    pub fn lock(&self) -> IrqSpinLockGuard<'_, T> {
        let restore_interrupts = interrupts_enabled();
        unsafe {
            core::arch::asm!("cli");
        }
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        IrqSpinLockGuard { lock: self, restore_interrupts }
    }
}

impl<T> core::ops::Deref for IrqSpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> core::ops::DerefMut for IrqSpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for IrqSpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
        if self.restore_interrupts {
            unsafe {
                core::arch::asm!("sti");
            }
        }
    }
}
