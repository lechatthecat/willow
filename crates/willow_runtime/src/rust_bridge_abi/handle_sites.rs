//! Post-return creation sites. Keys are slot numbers, never allocation history.
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use willow_abi::handle_debug as op;

#[derive(Clone, Copy)]
struct Site {
    generation: u32,
    file: &'static str,
    line: u32,
}

#[derive(Default)]
struct Sites(HashMap<u32, Site>);

impl Sites {
    fn record(&mut self, id: u64, file: &'static str, line: u32) {
        let slot = id as u32;
        let generation = (id >> 32) as u32;
        if slot == 0 || generation == 0 {
            return;
        }
        let site = Site {
            generation,
            file,
            line,
        };
        self.0
            .entry(slot)
            .and_modify(|old| {
                // Passing the same handle through Willow functions must retain its
                // original creation site. Late stale observations cannot overwrite
                // the location of a newer occupant of this slot.
                if old.generation < generation {
                    *old = site;
                }
            })
            .or_insert(site);
    }

    fn get(&self, id: u64) -> Option<Site> {
        self.0
            .get(&(id as u32))
            .copied()
            .filter(|site| site.generation == (id >> 32) as u32)
    }
}

static SITES: LazyLock<Mutex<Sites>> = LazyLock::new(|| Mutex::new(Sites::default()));

enum Visit {
    Value(u64, usize),
    Array {
        value: u64,
        child: usize,
        next: i64,
        len: i64,
    },
    Fields {
        value: u64,
        node: usize,
        next: usize,
        len: usize,
    },
}

/// # Safety
/// `shape` is a valid immutable compiler descriptor with process lifetime;
/// `value` matches it and all reachable objects are live and stable for this
/// non-GC, non-suspending call. `file` is static UTF-8 of `len` readable bytes.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_debug_record_handle_output(
    value: u64,
    shape: *const u64,
    file: *const u8,
    len: usize,
    line: u32,
) {
    // SAFETY: generated code supplies immutable static metadata and valid UTF-8.
    let file = unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(file, len)) };
    let mut sites = SITES.lock().unwrap_or_else(|p| p.into_inner());
    // SAFETY: word zero is the root node offset of the compiler descriptor.
    let mut work = vec![Visit::Value(value, unsafe { *shape } as usize)];
    while let Some(visit) = work.pop() {
        // SAFETY: all work items are derived from the checked descriptor and
        // the matching return value. Only live variant fields are visited.
        // No collector, user callback, or task switch runs while reading them.
        unsafe {
            visit_one(visit, shape, &mut work, &mut sites, file, line);
        }
    }
}

unsafe fn visit_one(
    visit: Visit,
    shape: *const u64,
    work: &mut Vec<Visit>,
    sites: &mut Sites,
    file: &'static str,
    line: u32,
) {
    // SAFETY: the caller maintains the descriptor/value invariant above. Array
    // and record continuations keep auxiliary storage bounded by shape depth.
    unsafe {
        match visit {
            Visit::Value(value, node) if node != 0 && value != 0 => match *shape.add(node) {
                op::HANDLE => sites.record(*(value as *const u64).add(1), file, line),
                op::NULLABLE => work.push(Visit::Value(value, *shape.add(node + 1) as usize)),
                op::TAGGED => {
                    let tag = *(value as *const u64) as usize;
                    let child = *shape.add(node + 1 + tag) as usize;
                    if child != 0 {
                        work.push(Visit::Value(*(value as *const u64).add(1), child));
                    }
                }
                op::ARRAY => work.push(Visit::Array {
                    value,
                    child: *shape.add(node + 1) as usize,
                    next: 0,
                    len: crate::array::array_len(value as *mut u8),
                }),
                op::RECORD => work.push(Visit::Fields {
                    value,
                    node: node + 2,
                    next: 0,
                    len: *shape.add(node + 1) as usize,
                }),
                _ => unreachable!("compiler debug handle descriptor"),
            },
            Visit::Array {
                value,
                child,
                next,
                len,
            } if next < len => {
                // Every handle-bearing element is a GC reference, never an
                // inline scalar pair or a packed byte.
                let element = crate::array::willow_array_get(value as *mut u8, next) as u64;
                work.push(Visit::Array {
                    value,
                    child,
                    next: next + 1,
                    len,
                });
                work.push(Visit::Value(element, child));
            }
            Visit::Fields {
                value,
                node,
                next,
                len,
            } if next < len => {
                let offset = *shape.add(node + next * 2) as usize;
                let child = *shape.add(node + next * 2 + 1) as usize;
                work.push(Visit::Fields {
                    value,
                    node,
                    next: next + 1,
                    len,
                });
                work.push(Visit::Value(*(value as *const u64).add(offset), child));
            }
            _ => {}
        }
    }
}

/// # Safety
/// The bridge supplies a static UTF-8 type name of `len` readable bytes.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_debug_report_handle_site(id: u64, name: *const u8, len: usize) {
    use std::io::Write;
    // SAFETY: Rust type names are valid UTF-8 with process lifetime.
    let name = unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(name, len)) };
    let site = SITES.lock().unwrap_or_else(|p| p.into_inner()).get(id);
    let mut stderr = std::io::stderr().lock();
    if let Some(site) = site {
        let _ = writeln!(stderr, "  {name} created at {}:{}", site.file, site.line);
    } else {
        let _ = writeln!(stderr, "  {name} created at <unknown>");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creation_sites_are_bounded_by_peak_slots_and_preserve_generations() {
        for width in [1, 16, 128] {
            for rounds in [1, 16, 128] {
                let mut sites = Sites::default();
                let mut handles = super::super::handles::Table::default();
                let name = std::any::TypeId::of::<u64>();
                for generation in 1..=rounds {
                    let ids: Vec<_> = (0..width)
                        .map(|_| handles.insert(Box::new(0u64), name))
                        .collect();
                    for &id in &ids {
                        sites.record(id, "creation.wi", generation as u32);
                        sites.record(id, "forward.wi", 99);
                        sites.record(id - (1 << 32), "stale.wi", 1);
                        let site = sites.get(id).unwrap();
                        assert_eq!(site.file, "creation.wi");
                        assert_eq!(site.line, generation as u32);
                        assert!(sites.get(id + (1 << 32)).is_none());
                    }
                    assert_eq!(sites.0.len(), width as usize);
                    for id in ids {
                        drop(handles.close(id, name).unwrap());
                    }
                }
                eprintln!(
                    "peak_slots={width} generations={rounds} site_entries={}",
                    sites.0.len()
                );
            }
        }
    }
}
