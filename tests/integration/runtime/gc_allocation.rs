use crate::support::*;

// ── GC ───────────────────────────────────────────────────────────────────────

#[test]
fn test_gc_allocated_bytes_increases_on_class_alloc() {
    let src = r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    }
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
fn main() {
    let before = gc_allocated_bytes();
    let b = new Box(42);
    let after = gc_allocated_bytes();
    println(b.get());
    println(after > before);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "42\ntrue\n");
}

// Nursery/TLAB counters describe different paths under allocation stress:
// alloc collects on allocation and uses old regions, bypassing the nursery.
// Compile each fixture once and check all three runtime modes explicitly so
// ambient WILLOW_GC_STRESS never removes normal-mode movement coverage.
fn assert_gc_allocation_modes(source: &str, nursery_expected: &str, alloc_expected: &str) {
    let project = TestProject::new("gc_allocation_modes", &[("main.wi", source)]);
    let compiled = project.compile("main.wi");
    assert!(
        compiled.status.success(),
        "GC fixture compilation failed: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    for (mode, expected) in [
        ("", nursery_expected),
        ("minor", nursery_expected),
        ("alloc", alloc_expected),
    ] {
        let output = project.run_with_env(&[("WILLOW_GC_STRESS", mode)]);
        assert!(
            output.status.success(),
            "GC fixture failed in mode {mode:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            expected,
            "GC fixture output in mode {mode:?}"
        );
    }
}

#[test]
fn gc_tlab_01_first_allocation_refills_then_next_allocations_use_fast_path() {
    let src = r#"
class Box {
    pub value: i64;
}
fn main() {
    let fast_before = gc_tlab_fast_allocations();
    let slow_before = gc_tlab_slow_allocations();
    let refill_before = gc_tlab_refills();
    let a = new Box(1);
    let b = new Box(2);
    let c = new Box(3);
    println(a.value + b.value + c.value);
    println(gc_tlab_slow_allocations() > slow_before);
    println(gc_tlab_fast_allocations() >= fast_before + 2);
    println(gc_tlab_refills() > refill_before);
    println(gc_tlab_reserved_bytes() > 0);
}
"#;
    assert_gc_allocation_modes(
        src,
        "6\ntrue\ntrue\ntrue\ntrue\n",
        "6\ntrue\nfalse\nfalse\nfalse\n",
    );
}

#[test]
fn gc_tlab_02_collection_reclaims_fast_and_slow_objects_and_empty_chunk() {
    let src = r#"
class Box {
    pub value: i64;
}
fn allocate_pair() -> i64 {
    let a = new Box(10);
    let b = new Box(20);
    return a.value + b.value;
}
fn main() {
    println(allocate_pair());
    println(gc_tlab_fast_allocations() > 0);
    gc_collect();
    println(gc_allocated_bytes());
    println(gc_tlab_reserved_bytes());
}
"#;
    assert_gc_allocation_modes(src, "30\ntrue\n0\n0\n", "30\nfalse\n0\n0\n");
}

#[test]
fn gc_tlab_03_exhausted_chunk_refills_without_changing_pointer_semantics() {
    let src = r#"
class Box {
    pub value: i64;
}
fn main() {
    let refill_before = gc_tlab_refills();
    let mut sum = 0;
    for i in 0..1400 {
        let b = new Box(i);
        sum = sum + b.value;
    }
    println(sum);
    println(gc_tlab_refills() >= refill_before + 2);
    println(gc_tlab_fast_allocations() > 0);
}
"#;
    assert_gc_allocation_modes(src, "979300\ntrue\ntrue\n", "979300\nfalse\nfalse\n");
}

#[test]
fn gc_young_01_compiler_field_barrier_tracks_old_to_young_store() {
    let src = r#"
class Node {
    pub value: i64;
}
class Holder {
    pub child: Node;
}
fn main() {
    let h = new Holder(new Node(1));
    gc_minor_collect();
    gc_minor_collect();
    let hits = gc_write_barrier_hits();
    let moved = gc_moved_objects();
    h.child = new Node(42);
    println(gc_write_barrier_hits() > hits);
    println(gc_remembered_set_size() > 0);
    gc_minor_collect();
    println(h.child.value);
    println(gc_moved_objects() > moved);
    println(gc_remembered_set_size());
}
"#;
    assert_gc_allocation_modes(
        src,
        "true\ntrue\n42\ntrue\n1\n",
        "false\nfalse\n42\nfalse\n0\n",
    );
}

#[test]
fn gc_young_02_array_slot_is_updated_after_young_object_moves() {
    let src = r#"
import std::collections::Array;
class Box {
    pub value: i64;
}
fn main() {
    let xs: Array<Box> = [];
    xs.push(new Box(77));
    let moved = gc_moved_objects();
    println(gc_remembered_set_size() > 0);
    gc_minor_collect();
    println(xs[0].value);
    println(gc_moved_objects() > moved);
    println(gc_remembered_set_size());
}
"#;
    assert_gc_allocation_modes(src, "true\n77\ntrue\n1\n", "false\n77\nfalse\n0\n");
}

#[test]
fn gc_young_03_async_frame_slot_is_updated_after_minor_collection() {
    let src = r#"
class Box {
    pub value: i64;
}
async fn worker() -> i64 {
    let b = new Box(88);
    let moved = gc_moved_objects();
    gc_minor_collect();
    println(gc_moved_objects() > moved);
    return b.value;
}
async fn main() {
    println(await worker());
}
"#;
    assert_gc_allocation_modes(src, "true\n88\n", "false\n88\n");
}

#[test]
fn gc_young_04_nursery_threshold_triggers_minor_collection() {
    let src = r#"
class Box {
    pub value: i64;
}
fn main() {
    let before = gc_minor_collections();
    let mut sum = 0;
    for i in 0..12000 {
        let b = new Box(i);
        sum = sum + b.value;
    }
    println(sum);
    println(gc_minor_collections() > before);
}
"#;
    assert_gc_allocation_modes(src, "71994000\ntrue\n", "71994000\nfalse\n");
}

#[test]
fn gc_young_05_minor_stress_preserves_live_graphs() {
    let src = r#"
class Node {
    pub value: i64;
}
class Holder {
    pub child: Node;
}
fn main() {
    let h = new Holder(new Node(123));
    let mut sum = 0;
    for i in 0..1200 {
        let garbage = new Node(i);
        sum = sum + garbage.value;
    }
    println(h.child.value);
    println(sum);
    println(gc_minor_collections() > 0);
}
"#;
    let (out, ok) = compile_and_run_gc_stress_mode(src, "minor");
    assert!(ok, "minor-GC stress program failed: {out}");
    assert_eq!(out, "123\n719400\ntrue\n");
}

#[test]
fn gc_young_06_enum_and_interface_payloads_are_rewritten() {
    let src = r#"
interface Animal {
    fn value(self) -> i64;
}
class Dog implements Animal {
    pub n: i64;
    pub fn value(self) -> i64 { return self.n; }
}
class Node {
    pub n: i64;
}
fn main() {
    let animal: Animal = new Dog(17);
    let option = Option::Some(new Node(25));
    let moved = gc_moved_objects();
    gc_minor_collect();
    println(animal.value());
    println(option.unwrap().n);
    // Option<Node> is the Node pointer itself (willow-glaj.3), so this graph
    // contains one fewer movable enum wrapper than the old tagged layout.
    println(gc_moved_objects() >= moved + 1);
}
"#;
    assert_gc_allocation_modes(src, "17\n25\ntrue\n", "17\n25\nfalse\n");
}

#[test]
fn gc_young_07_barrier_verifier_accepts_generated_old_to_young_store() {
    let src = r#"
class Node {
    pub value: i64;
}
class Holder {
    pub child: Node;
}
fn main() {
    let h = new Holder(new Node(1));
    gc_minor_collect();
    h.child = new Node(2);
    gc_minor_collect();
    println(h.child.value);
}
"#;
    let (out, ok) = compile_and_run_with_runtime_env(
        src,
        &[("WILLOW_GC_VERIFY_BARRIER", "1")],
        std::time::Duration::from_secs(10),
    );
    assert!(ok, "barrier verification rejected a valid store: {out}");
    assert_eq!(out, "2\n");
}

#[test]
fn gc_region_01_runtime_old_allocations_expose_region_metadata() {
    let src = r#"
import std::collections::Array;
fn main() {
    let xs: Array<i64> = [];
    println(xs.len());
    println(gc_old_region_count() > 0);
    println(gc_old_region_reserved_bytes() > 0);
    println(gc_old_region_live_bytes() > 0);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "old-region metadata program failed");
    assert_eq!(out, "0\ntrue\ntrue\ntrue\n");
}

#[test]
fn gc_region_02_large_array_buffer_uses_dedicated_region() {
    let src = r#"
import std::collections::Array;
fn main() {
    let before = gc_large_object_region_count();
    let xs: Array<i64> = [];
    for i in 0..20000 {
        xs.push(i);
    }
    println(xs.len());
    println(gc_large_object_region_count() > before);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "large-object region program failed");
    assert_eq!(out, "20000\ntrue\n");
}

#[test]
fn gc_region_03_major_collection_releases_dead_large_regions() {
    let src = r#"
import std::collections::Array;
fn build_large_array() -> i64 {
    let xs: Array<i64> = [];
    for i in 0..20000 {
        xs.push(i);
    }
    return xs.len();
}
fn main() {
    let large_before = gc_large_object_region_count();
    let released_before = gc_old_regions_released();
    println(build_large_array());
    gc_collect();
    println(gc_large_object_region_count() == large_before);
    println(gc_old_regions_released() > released_before);
    println(gc_major_collections() > 0);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "large-region release program failed");
    assert_eq!(out, "20000\ntrue\ntrue\ntrue\n");
}

#[test]
fn gc_region_04_direct_young_root_becomes_pinned_region() {
    let src = r#"
class Box {
    pub value: i64;
}
fn main() {
    let before = gc_pinned_region_count();
    let b = new Box(73);
    gc_minor_collect();
    println(b.value);
    println(gc_pinned_region_count() > before);
}
"#;
    assert_gc_allocation_modes(src, "73\ntrue\n", "73\nfalse\n");
}

#[test]
fn gc_region_05_region_verifier_accepts_minor_and_major_collections() {
    let src = r#"
import std::collections::Array;
class Node {
    pub value: i64;
}
class Holder {
    pub child: Node;
}
fn main() {
    let h = new Holder(new Node(1));
    let xs: Array<Node> = [];
    xs.push(new Node(2));
    gc_minor_collect();
    h.child = new Node(3);
    xs.push(new Node(4));
    gc_minor_collect();
    gc_collect();
    println(h.child.value + xs[0].value + xs[1].value);
}
"#;
    let (out, ok) = compile_and_run_with_runtime_env(
        src,
        &[("WILLOW_GC_VERIFY_REGIONS", "1")],
        std::time::Duration::from_secs(10),
    );
    assert!(ok, "region verifier rejected valid collections: {out}");
    assert_eq!(out, "9\n");
}

#[test]
fn test_gc_collect_reclaims_unrooted_objects() {
    // alloc_node allocates a Node and returns its value field (i64, not a GC pointer).
    // When alloc_node returns, the Node's root is popped, so the Node has no live roots.
    // gc_collect() in main can then reclaim it, leaving gc_allocated_bytes() == 0.
    let src = r#"
class Node {
    pub init(self, value: i64) {
        self.value = value;
    }
    value: i64;
    pub fn get(self) -> i64 { return self.value; }
}
fn alloc_node() -> i64 {
    let n = new Node(7);
    return n.get();
}
fn main() {
    let v = alloc_node();
    println(v);
    gc_collect();
    println(gc_allocated_bytes());
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "7\n0\n");
}

#[test]
fn test_gc_does_not_collect_live_rooted_objects() {
    // A rooted object (n is in scope when gc_collect() runs) must not be freed.
    let src = r#"
class Node {
    pub init(self, value: i64) {
        self.value = value;
    }
    value: i64;
    pub fn get(self) -> i64 { return self.value; }
}
fn main() {
    let n = new Node(42);
    gc_collect();
    println(n.get());
    println(gc_allocated_bytes() > 0);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "42\ntrue\n");
}

#[test]
fn test_gc_traces_option_reference_fields() {
    let src = r#"
class Node {
    pub value: i64;
    pub next: Option<Node>;
}

fn make_pair() -> Node {
    let tail = new Node(2, None);
    return new Node(1, Some(tail));
}

fn main() {
    let head = make_pair();
    gc_collect();
    println(head.value);
    let next = head.next;
    match next { Some(value) => println(value.value), None => println(0) }
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option reference field should keep child object alive");
    assert_eq!(out, "1\n2\n");
}

#[test]
fn test_gc_ignores_none_option_reference_fields() {
    let src = r#"
class Node {
    pub init(self, value: i64, next: Option<Node>) {
        self.value = value;
        self.next = next;
    }
    pub value: i64;
    next: Option<Node>;
}

fn main() {
    let head = new Node(1, None);
    gc_collect();
    println(head.value);
    println(gc_allocated_bytes() > 0);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "None Option field should be ignored safely by GC");
    assert_eq!(out, "1\ntrue\n");
}
