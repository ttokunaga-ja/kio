//! Counts live Rust heap bytes in debug CLI tests. SQLite's native allocations
//! and the separate index rebuild do not enter the working-scan measurement.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU8, Ordering::Relaxed};

const OFF: u8 = 0;
const SCAN: u8 = 1;
const BETWEEN: u8 = 2;
const OUTPUT: u8 = 3;

static PHASE: AtomicU8 = AtomicU8::new(OFF);
static PUBLISHED: AtomicBool = AtomicBool::new(false);
static LIVE: AtomicIsize = AtomicIsize::new(0);
static BASE: AtomicIsize = AtomicIsize::new(0);
static SCAN_PEAK: AtomicIsize = AtomicIsize::new(0);
static OUTPUT_PEAK: AtomicIsize = AtomicIsize::new(0);

pub fn start() {
    if kio_core::test_control::scan_heap_requested() {
        PHASE.store(SCAN, Relaxed);
    }
}

pub fn scan_started() {
    if PHASE.load(Relaxed) == BETWEEN {
        BASE.store(LIVE.load(Relaxed), Relaxed);
        PHASE.store(SCAN, Relaxed);
    }
}

pub fn scan_published() {
    if PHASE.load(Relaxed) == SCAN {
        PUBLISHED.store(true, Relaxed);
        PHASE.store(BETWEEN, Relaxed);
    }
}

pub fn output_started() {
    if PHASE.load(Relaxed) != OFF {
        BASE.store(LIVE.load(Relaxed), Relaxed);
        PHASE.store(OUTPUT, Relaxed);
    }
}

pub fn report() {
    let phase = PHASE.swap(OFF, Relaxed);
    if phase != OFF {
        let result = if PUBLISHED.load(Relaxed) {
            "published"
        } else {
            "read"
        };
        eprintln!(
            "kio_scan_heap {} {} {result}",
            SCAN_PEAK.load(Relaxed),
            OUTPUT_PEAK.load(Relaxed)
        );
    }
}

fn add(delta: isize) {
    let phase = PHASE.load(Relaxed);
    if phase != OFF {
        let live = LIVE.fetch_add(delta, Relaxed) + delta;
        let growth = live - BASE.load(Relaxed);
        match phase {
            SCAN => {
                SCAN_PEAK.fetch_max(growth, Relaxed);
            }
            OUTPUT => {
                OUTPUT_PEAK.fetch_max(growth, Relaxed);
            }
            _ => {}
        }
    }
}

struct MeasuringAllocator;

// SAFETY: pointers and layout requirements are delegated unchanged to System.
// Accounting uses only atomics and performs no allocations.
unsafe impl GlobalAlloc for MeasuringAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller supplies a valid nonzero allocation layout.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            add(layout.size() as isize);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        add(-(layout.size() as isize));
        // SAFETY: the caller supplies the pointer and layout of a live allocation.
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller supplies a valid nonzero allocation layout.
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            add(layout.size() as isize);
        }
        pointer
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: the caller supplies a live allocation and valid new size.
        let pointer = unsafe { System.realloc(pointer, layout, size) };
        if !pointer.is_null() {
            add(size as isize - layout.size() as isize);
        }
        pointer
    }
}

#[global_allocator]
static ALLOCATOR: MeasuringAllocator = MeasuringAllocator;
