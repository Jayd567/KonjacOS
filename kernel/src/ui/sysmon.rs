//! The global state monitor: a small background task that samples system
//! metrics twice a second -- CPU load (from the idle task's share of timer
//! ticks), physical memory, the task table and the wall clock -- and
//! publishes them as one [`Snapshot`]. The taskbar tray and the Monitor
//! window just read the latest snapshot, so keeping those numbers live
//! never stalls the desktop's render loop on port I/O or table walks.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::rtc::{self, DateTime};
use crate::sync::SpinLock;
use crate::{pmm, task, timer};

pub const HISTORY: usize = 60;
pub const MAX_TASKS: usize = 24;
const PERIOD_TICKS: u64 = 50;

#[derive(Clone, Copy)]
pub struct TaskInfo {
    pub id: u64,
    pub name: &'static str,
    pub state: &'static str,
}

#[derive(Clone, Copy)]
pub struct Snapshot {
    /// CPU busy percentage over the last sample period.
    pub cpu: u8,
    /// The last [`HISTORY`] CPU samples, oldest first.
    pub cpu_history: [u8; HISTORY],
    pub mem_used: u64,
    pub mem_total: u64,
    pub tasks: [TaskInfo; MAX_TASKS],
    pub task_count: usize,
    pub time: DateTime,
    pub uptime_secs: u64,
}

const EMPTY_TASK: TaskInfo = TaskInfo { id: 0, name: "", state: "" };

pub static SNAPSHOT: SpinLock<Snapshot> = SpinLock::new(Snapshot {
    cpu: 0,
    cpu_history: [0; HISTORY],
    mem_used: 0,
    mem_total: 0,
    tasks: [EMPTY_TASK; MAX_TASKS],
    task_count: 0,
    time: DateTime { year: 0, month: 0, day: 0, hour: 0, minute: 0, second: 0 },
    uptime_secs: 0,
});

/// Bumped after every new snapshot.
pub static SEQ: AtomicU64 = AtomicU64::new(0);

pub fn latest() -> Snapshot {
    *SNAPSHOT.lock()
}

pub fn start() {
    task::spawn("sysmon", run);
}

fn run() {
    let mut last_ticks = timer::ticks();
    let mut last_idle = task::idle_ticks();
    loop {
        let ticks = timer::ticks();
        let idle = task::idle_ticks();
        let dt = ticks.saturating_sub(last_ticks).max(1);
        let di = idle.saturating_sub(last_idle).min(dt);
        last_ticks = ticks;
        last_idle = idle;
        let cpu = (100 - di * 100 / dt) as u8;

        let (total_frames, free_frames) = pmm::stats();
        let list = task::list();
        let time = rtc::now();

        {
            let mut s = SNAPSHOT.lock();
            s.cpu = cpu;
            s.cpu_history.copy_within(1.., 0);
            s.cpu_history[HISTORY - 1] = cpu;
            s.mem_total = total_frames * pmm::FRAME_SIZE;
            s.mem_used = (total_frames - free_frames) * pmm::FRAME_SIZE;
            s.task_count = list.len().min(MAX_TASKS);
            for (slot, &(id, name, state, ..)) in s.tasks.iter_mut().zip(list.iter()) {
                *slot = TaskInfo { id, name, state: state.label() };
            }
            s.time = time;
            s.uptime_secs = timer::uptime_seconds();
        }
        drop(list);
        SEQ.fetch_add(1, Ordering::Release);
        task::sleep_ticks(PERIOD_TICKS);
    }
}
