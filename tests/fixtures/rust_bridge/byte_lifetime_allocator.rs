//! Final executable's allocator for the byte-lifetime test.
#![no_main]
use std::alloc::{GlobalAlloc, Layout, System};

unsafe extern "C" {
    fn observe_bridge_deallocation(ptr: *mut u8);
}

struct ObservedAllocator;
#[global_allocator]
static ALLOCATOR: ObservedAllocator = ObservedAllocator;

// SAFETY: every allocation and deallocation uses System with its original layout.
// The observer only compares the address of a block before it is freed.
unsafe impl GlobalAlloc for ObservedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller supplies a valid allocation layout.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: ptr is still allocated. The fixture compares its address before deallocation.
        unsafe { observe_bridge_deallocation(ptr) };
        // SAFETY: ptr came from System and layout matches the allocation.
        unsafe { System.dealloc(ptr, layout) }
    }
}
