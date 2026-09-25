//! Bounded private mappings, owned by an address space rather than a thread.
//! Lock order: TASKS may acquire REGIONS during teardown; REGIONS never acquires
//! TASKS. All buffers accessed under REGIONS are resident kernel/HHDM memory.
//! This single-CPU implementation serializes faults and edits with IRQs masked.

use crate::{paging, pmm, sync::IrqSpinLock, task::FileBacking};

const PAGE: u64 = 4096;
const USER_END: u64 = 0x0000_8000_0000_0000;
const CAPACITY: usize = 2048;
const EINVAL: i64 = -22;
const ENOMEM: i64 = -12;

#[derive(Clone, Copy)]
struct Region {
    cr3: u64,
    start: u64,
    end: u64,
    prot: u64,
    shared_readonly: bool,
    backing: Option<FileBacking>,
    offset: u64,
}

static REGIONS: IrqSpinLock<[Option<Region>; CAPACITY]> = IrqSpinLock::new([None; CAPACITY]);

fn range(addr: u64, len: u64) -> Result<u64, i64> {
    if addr < PAGE || addr % PAGE != 0 || len == 0 { return Err(EINVAL); }
    let rounded = len.checked_add(PAGE - 1).ok_or(EINVAL)? & !(PAGE - 1);
    let end = addr.checked_add(rounded).ok_or(EINVAL)?;
    if end > USER_END { return Err(EINVAL); }
    Ok(end)
}

fn flags(prot: u64) -> u64 {
    let mut flags = if prot != 0 { paging::PAGE_USER } else { 0 };
    if prot & 2 != 0 { flags |= paging::PAGE_WRITABLE; }
    if prot & 4 == 0 && paging::nx_available() { flags |= paging::PAGE_NX; }
    flags
}

fn insert(table: &mut [Option<Region>; CAPACITY], region: Region) {
    // Every caller preflights capacity before changing any metadata or PTE.
    *table.iter_mut().find(|r| r.is_none()).expect("vm capacity invariant") = Some(region);
}

fn splits(table: &[Option<Region>; CAPACITY], cr3: u64, start: u64, end: u64) -> usize {
    table.iter().flatten().filter(|r| r.cr3 == cr3)
        .map(|r| usize::from(r.start < start && start < r.end)
            + usize::from(r.start < end && end < r.end)).sum()
}

fn split_at(table: &mut [Option<Region>; CAPACITY], cr3: u64, at: u64) {
    for i in 0..CAPACITY {
        if let Some(mut r) = table[i] {
            if r.cr3 == cr3 && r.start < at && at < r.end {
                let mut right = r;
                right.start = at;
                right.offset += at - r.start;
                r.end = at;
                table[i] = Some(r);
                insert(table, right);
                return;
            }
        }
    }
}

fn free_pages(cr3: u64, start: u64, end: u64) {
    for addr in (start..end).step_by(PAGE as usize) {
        // Safety: callers checked a page-aligned, canonical user-only range.
        // Each mapped frame has one owning PTE in this private address space.
        if let Some(phys) = unsafe { paging::unmap_page(cr3, addr) } {
            unsafe { pmm::free_frame(phys) };
        }
    }
}

fn top(table: &[Option<Region>; CAPACITY], cr3: u64) -> u64 {
    table.iter().flatten().filter(|r| r.cr3 == cr3)
        .map(|r| r.end).max().unwrap_or(crate::task::USER_MMAP_BASE)
        .max(crate::task::USER_MMAP_BASE)
}

pub fn map(addr: u64, len: u64, prot: u64, map_flags: u64,
    backing: Option<FileBacking>, offset: u64) -> Result<i64, i64> {
    if prot & !7 != 0 || offset % PAGE != 0 || offset > i64::MAX as u64 {
        return Err(EINVAL);
    }
    let kind = map_flags & 3;
    if kind != 1 && kind != 2 { return Err(EINVAL); }
    // Shared writable pages require cache/writeback semantics we do not have.
    if kind == 1 && (prot & 2 != 0 || backing.is_none()) { return Err(-38); }
    let cr3 = paging::current_cr3();
    let mut table = REGIONS.lock();
    let fixed = map_flags & 0x10 != 0;
    let start = if fixed { addr } else { top(&table, cr3) };
    let end = range(start, len)?;
    offset.checked_add(end - start).filter(|n| *n <= i64::MAX as u64).ok_or(EINVAL)?;
    let needed = if fixed { splits(&table, cr3, start, end) + 1 } else { 1 };
    if table.iter().filter(|r| r.is_none()).count() < needed { return Err(ENOMEM); }
    if fixed {
        split_at(&mut table, cr3, start);
        split_at(&mut table, cr3, end);
        for slot in table.iter_mut() {
            if slot.is_some_and(|r| r.cr3 == cr3 && r.start >= start && r.end <= end) { *slot = None; }
        }
        free_pages(cr3, start, end);
    }
    insert(&mut table, Region { cr3, start, end, prot, shared_readonly: kind == 1, backing, offset });
    Ok(start as i64)
}

pub fn unmap(addr: u64, len: u64) -> Result<(), i64> {
    let end = range(addr, len)?;
    let cr3 = paging::current_cr3();
    let mut table = REGIONS.lock();
    if table.iter().filter(|r| r.is_none()).count() < splits(&table, cr3, addr, end) {
        return Err(ENOMEM);
    }
    split_at(&mut table, cr3, addr);
    split_at(&mut table, cr3, end);
    for slot in table.iter_mut() {
        if slot.is_some_and(|r| r.cr3 == cr3 && r.start >= addr && r.end <= end) { *slot = None; }
    }
    free_pages(cr3, addr, end);
    Ok(())
}

pub fn protect(addr: u64, len: u64, prot: u64) -> Result<(), i64> {
    if prot & !7 != 0 || addr % PAGE != 0 { return Err(EINVAL); }
    if len == 0 { return Ok(()); }
    let end = range(addr, len)?;
    let cr3 = paging::current_cr3();
    let mut table = REGIONS.lock();
    // Also permit resident ELF/stack/brk pages created by the loader. Validate
    // the whole interval before changing permissions; holes return ENOMEM.
    let mut at = addr;
    while at < end {
        if let Some(r) = table.iter().flatten().find(|r| r.cr3 == cr3 && r.start <= at && at < r.end) {
            if r.shared_readonly && prot & 2 != 0 { return Err(-13); } // EACCES
            at = r.end.min(end);
        } else {
            if unsafe { paging::translate(cr3, at) }.is_none() { return Err(ENOMEM); }
            at += PAGE;
        }
    }
    if table.iter().filter(|r| r.is_none()).count() < splits(&table, cr3, addr, end) {
        return Err(ENOMEM);
    }
    split_at(&mut table, cr3, addr);
    split_at(&mut table, cr3, end);
    for r in table.iter_mut().flatten() {
        if r.cr3 == cr3 && r.start >= addr && r.end <= end { r.prot = prot; }
    }
    // Safety: valid current page tables, validated user range, IRQs disabled.
    unsafe { paging::protect_range_in(cr3, addr, end - addr, flags(prot)) };
    Ok(())
}

pub fn fault(addr: u64, error: u64) -> bool {
    let cr3 = paging::current_cr3();
    let page = addr & !(PAGE - 1);
    let mut table = REGIONS.lock();
    let Some(r) = table.iter_mut().flatten().find(|r| r.cr3 == cr3 && r.start <= page && page < r.end) else {
        return false;
    };
    if r.prot == 0 || (error & 2 != 0 && r.prot & 2 == 0)
        || (error & 16 != 0 && r.prot & 4 == 0) { return false; }
    let file_offset = r.offset + page - r.start;
    if r.backing.is_some_and(|b| file_offset >= b.len() as u64) { return false; }
    let Some(phys) = pmm::alloc_frame() else { return false; };
    // Safety: a fresh, exclusively owned frame in the permanent HHDM. It is
    // populated before a user PTE is published, so even read-only pages need
    // no temporary writable user mapping. No user pointer is accessed here.
    let buffer = unsafe { core::slice::from_raw_parts_mut((pmm::hhdm_offset() + phys) as *mut u8, PAGE as usize) };
    buffer.fill(0);
    if let Some(backing) = &mut r.backing {
        if backing.read_at(file_offset, buffer).is_err() {
            unsafe { pmm::free_frame(phys) };
            return false;
        }
    }
    if !unsafe { paging::try_map_page_in(cr3, page, phys, flags(r.prot)) } {
        unsafe { pmm::free_frame(phys) };
        return false;
    }
    true
}

/// Called only when the last task sharing CR3 is reaped, before frame teardown.
pub fn destroy(cr3: u64) {
    let mut table = REGIONS.lock();
    for slot in table.iter_mut() {
        if slot.is_some_and(|r| r.cr3 == cr3) { *slot = None; }
    }
}
