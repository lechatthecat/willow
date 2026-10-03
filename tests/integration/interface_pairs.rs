//! Interface pair semantics across typed storage, erased transport and GC.
use super::support::{compile_and_run, compile_and_run_with_env};

const HEADER: &str = r#"
import std::collections::Array;
import std::collections::Map;
interface Value extends Sync { fn get(self) -> i64; fn copy(self) -> Self; }
class Number implements Value {
    pub n: i64;
    pub fn get(self) -> i64 { return self.n; }
    pub fn copy(self) -> Number { return new Number(self.n); }
}
"#;

fn check(body: &str, expected: &str) {
    let (out, ok) =
        compile_and_run_with_env(&format!("{HEADER}\n{body}"), &[("WILLOW_GC_STRESS", "all")]);
    assert!(ok, "{out}");
    assert_eq!(out, expected);
}

macro_rules! perspective {
    ($name:ident, $source:literal, $expected:literal) => {
        #[test]
        fn $name() {
            check($source, $expected);
        }
    };
}

perspective!(
    iface_pair_01_direct_conversion,
    r#"fn main() { let v: Value = new Number(7); println(v.get()); }"#,
    "7\n"
);

perspective!(
    iface_pair_02_argument,
    r#"fn read(v: Value) -> i64 { return v.get(); } fn main() { println(read(new Number(11))); }"#,
    "11\n"
);

perspective!(
    iface_pair_03_return,
    r#"fn make() -> Value { return new Number(13); } fn main() { println(make().get()); }"#,
    "13\n"
);

perspective!(
    iface_pair_04_identity_argument_return,
    r#"fn same(v: Value) -> Value { return v; } fn main() { let v: Value = new Number(17); println(same(v).get()); }"#,
    "17\n"
);

perspective!(
    iface_pair_05_rewiden_second_super,
    r#"interface Pad { fn pad(self) -> i64; } interface More extends Pad, Value { fn more(self) -> i64; } class Extra implements More { pub fn pad(self) -> i64 { return 1; } pub fn get(self) -> i64 { return 19; } pub fn copy(self) -> Extra { return new Extra(); } pub fn more(self) -> i64 { return 2; } } fn main() { let m: More = new Extra(); let v: Value = m; println(v.get()); }"#,
    "19\n"
);

perspective!(
    iface_pair_06_self_return,
    r#"fn main() { let v: Value = new Number(23); let copy: Value = v.copy(); gc_collect(); println(copy.get()); }"#,
    "23\n"
);

perspective!(
    iface_pair_07_field_neighbors,
    r#"class Holder { pub head: i64; pub value: Value; pub tail: i64; } fn main() { let h = new Holder(3, new Number(29), 5); gc_collect(); println(h.head); println(h.value.get()); println(h.tail); }"#,
    "3\n29\n5\n"
);

perspective!(
    iface_pair_08_enum_payload_neighbors,
    r#"enum Packet { Data(i64, Value, i64), Empty } fn main() { let p = Packet::Data(3, new Number(31), 5); gc_collect(); match p { Packet::Data(a, v, b) => { println(a); println(v.get()); println(b); }, Packet::Empty => println(0) } }"#,
    "3\n31\n5\n"
);

perspective!(
    iface_pair_09_option_some_none,
    r#"fn main() { let a: Option<Value> = Some(new Number(37)); let b: Option<Value> = None; gc_collect(); println(a.unwrap().get()); println(b.is_none()); }"#,
    "37\ntrue\n"
);

perspective!(
    iface_pair_10_result_payload,
    r#"fn main() { let a: Result<Value, i64> = Ok(new Number(41)); let b: Result<i64, Value> = Err(new Number(43)); gc_collect(); println(a.unwrap().get()); println(b.unwrap_err().get()); }"#,
    "41\n43\n"
);

perspective!(
    iface_pair_11_closure_capture,
    r#"fn make() -> closure() -> i64 { let v: Value = new Number(47); let text = "kept" + "!"; return || -> i64 { gc_collect(); println(text); return v.get(); }; } fn main() { let f = make(); gc_collect(); println(f()); }"#,
    "kept!\n47\n"
);

perspective!(
    iface_pair_12_closure_argument_return,
    r#"fn main() { let f = |v: Value| -> Value { return v; }; let v: Value = new Number(53); println(f(v).get()); }"#,
    "53\n"
);

perspective!(
    iface_pair_13_array_roundtrip,
    r#"fn main() { let a: Array<Value> = [new Number(59), new Number(61)]; gc_collect(); println(a[0].get()); println(a[1].get()); }"#,
    "59\n61\n"
);

perspective!(
    iface_pair_14_map_roundtrip,
    r#"fn main() { let m: Map<i64, Value> = Map::new(); let v: Value = new Number(67); m.insert(1, v); gc_collect(); println(m.get(1).unwrap().get()); println(m.get(2).is_none()); }"#,
    "67\ntrue\n"
);

perspective!(
    iface_pair_15_reference_local,
    r#"fn replace(v: &mut Value) { v = new Number(71); } fn main() { let mut v: Value = new Number(1); replace(&v); gc_collect(); println(v.get()); }"#,
    "71\n"
);

perspective!(
    iface_pair_16_reference_field,
    r#"class Holder { pub v: Value; pub tail: i64; } fn replace(v: &mut Value) { v = new Number(73); } fn main() { let h = new Holder(new Number(1), 9); replace(&h.v); gc_collect(); println(h.v.get()); println(h.tail); }"#,
    "73\n9\n"
);

perspective!(
    iface_pair_17_reference_array,
    r#"fn replace(v: &mut Value) { v = new Number(79); } fn main() { let a: Array<Value> = [new Number(1), new Number(2)]; replace(&a[0]); gc_collect(); println(a[0].get()); println(a[1].get()); }"#,
    "79\n2\n"
);

perspective!(
    iface_pair_18_reference_array_growth,
    r#"fn grow(a: Array<Value>) -> i64 { let v: Value = new Number(2); a.push(v); gc_minor_collect(); return 0; } fn write(v: &mut Value, ignored: i64) { v = new Number(83); println(v.get()); } fn main() { let a: Array<Value> = [new Number(1)]; write(&a[0], grow(a)); println(a[0].get()); }"#,
    "83\n1\n"
);

perspective!(
    iface_pair_19_async_parameter,
    r#"async fn read(v: Value, tail: i64) -> i64 { await sleep(0); gc_collect(); return v.get() + tail; } async fn main() { let v: Value = new Number(89); println(await read(v, 2)); }"#,
    "91\n"
);

perspective!(
    iface_pair_20_async_local,
    r#"async fn main() { let v: Value = new Number(97); await sleep(0); gc_collect(); println(v.get()); }"#,
    "97\n"
);

perspective!(
    iface_pair_21_async_result,
    r#"async fn make() -> Value { await sleep(0); return new Number(101); } async fn main() { let v = await make(); gc_collect(); println(v.get()); }"#,
    "101\n"
);

perspective!(
    iface_pair_22_channel_roundtrip,
    r#"fn main() { let ch: Channel<Value> = Channel::new(); let v: Value = new Number(103); ch.send(v); gc_collect(); println(ch.recv().get()); }"#,
    "103\n"
);

perspective!(
    iface_pair_23_async_channel_blocked_send,
    r#"async fn send(ch: Channel<Value>) { let v: Value = new Number(109); ch.send(v); } async fn main() { let ch: Channel<Value> = Channel::with_capacity(1); let first: Value = new Number(107); ch.send(first); let task = send(ch); await sleep(1); gc_collect(); println(ch.recv().get()); await task; println(ch.recv().get()); }"#,
    "107\n109\n"
);

perspective!(
    iface_pair_24_indirect_function,
    r#"fn same(v: Value) -> Value { return v; } fn invoke(f: fn(Value) -> Value, v: Value) -> Value { return f(v); } fn main() { let v: Value = new Number(113); println(invoke(same, v).get()); }"#,
    "113\n"
);

perspective!(
    iface_pair_25_array_store_and_growth,
    r#"fn main() { let a: Array<Value> = [new Number(1)]; a[0] = new Number(127); let v: Value = new Number(131); a.push(v); gc_collect(); println(a[0].get()); println(a[1].get()); }"#,
    "127\n131\n"
);

#[test]
fn iface_pair_26_conversion_allocation_scaling() {
    for count in [1, 16, 128] {
        let mut source = format!(
            "{HEADER} fn read(v: Value) -> i64 {{ return v.get(); }} fn main() {{ let n = new Number(7); let before = gc_allocated_bytes(); let mut sum = 0;"
        );
        for _ in 0..count {
            source.push_str("sum = sum + read(n);");
        }
        source.push_str(
            "let allocated = gc_allocated_bytes() - before; println(allocated); println(sum); }",
        );
        let (out, ok) = compile_and_run(&source);
        assert!(ok, "count={count}: {out}");
        assert_eq!(out, format!("0\n{}\n", count * 7));
        let source = format!(
            "{HEADER} fn read(v: Value) -> i64 {{ return v.get(); }} fn main() {{ let n = new Number(7); let before = gc_allocated_bytes(); let mut sum = 0; let mut i = 0; while i < {count} {{ sum = sum + read(n); i = i + 1; }} let allocated = gc_allocated_bytes() - before; println(allocated); println(sum); }}"
        );
        let (out, ok) = compile_and_run(&source);
        assert!(ok, "loop count={count}: {out}");
        assert_eq!(out, format!("0\n{}\n", count * 7));
    }
}

#[test]
fn iface_pair_27_gc_stress_object_and_neighbors() {
    let body = r#"class Holder { pub v: Value; pub text: String; }
fn make() -> Holder { return new Holder(new Number(137), "kept" + "!"); }
fn main() { let h = make(); let v: Value = h.v; let f = || { gc_collect(); println(v.get()); println(h.text); }; gc_minor_collect(); f(); }"#;
    for mode in ["alloc", "minor", "all"] {
        let (out, ok) =
            compile_and_run_with_env(&format!("{HEADER} {body}"), &[("WILLOW_GC_STRESS", mode)]);
        assert!(ok, "{mode}: {out}");
        assert_eq!(out, "137\nkept!\n");
    }
}

#[test]
fn iface_pair_28_runnable_example() {
    let (out, ok) = compile_and_run_with_env(
        include_str!("../../example/interface_pairs.wi"),
        &[("WILLOW_GC_STRESS", "all")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n11\n7\n11\n");
}

perspective!(
    iface_pair_29_mutex_return_cleanup,
    r#"async fn replace(m: Mutex<Value>) -> i64 {
        lock m as mut v { v = new Number(149); gc_collect(); return v.get(); }
        return 0;
    }
    async fn main() {
        let initial: Value = new Number(1);
        let m: Mutex<Value> = Mutex::new(initial);
        println(await replace(m));
        lock m as v { gc_minor_collect(); println(v.get()); }
    }"#,
    "149\n149\n"
);

perspective!(
    iface_pair_30_rwlock_write_read_cleanup,
    r#"async fn main() {
        let initial: Value = new Number(1);
        let m: RwLock<Value> = RwLock::new(initial);
        lock write m as mut v { v = new Number(151); gc_collect(); }
        lock read m as v { gc_minor_collect(); println(v.get()); }
        lock write m as mut v { v = new Number(157); }
        lock read m as v { println(v.get()); }
    }"#,
    "151\n157\n"
);

#[test]
fn iface_pair_31_wide_class_bitmap_boundary() {
    for count in [30, 31, 32] {
        let fields = (0..count)
            .map(|i| format!("pub v{i}: Value; "))
            .collect::<String>();
        let values = (0..count)
            .map(|i| format!("new Number({i}), "))
            .collect::<String>();
        let sum = (0..count)
            .map(|i| format!("h.v{i}.get()"))
            .collect::<Vec<_>>()
            .join(" + ");
        check(
            &format!(
                r#"class Wide {{ {fields} pub text: String; }}
            fn make() -> Wide {{ return new Wide({values} "kept" + "!"); }}
            fn main() {{ let h = make(); gc_collect(); gc_minor_collect(); println(h.text); println({sum}); }}"#
            ),
            &format!("kept!\n{}\n", count * (count - 1) / 2),
        );
    }
}

#[test]
fn iface_pair_32_wide_closure_bitmap_boundary() {
    for count in [30, 31, 32] {
        let bindings = (0..count)
            .map(|i| format!("let v{i:03}: Value = new Number({i}); "))
            .collect::<String>();
        let sum = (0..count)
            .map(|i| format!("v{i:03}.get()"))
            .collect::<Vec<_>>()
            .join(" + ");
        check(
            &format!(
                r#"fn make() -> closure() -> i64 {{
            {bindings} let ztext = "kept" + "!";
            return || -> i64 {{ gc_collect(); gc_minor_collect(); println(ztext); return {sum}; }};
        }} fn main() {{ let f = make(); gc_collect(); println(f()); }}"#
            ),
            &format!("kept!\n{}\n", count * (count - 1) / 2),
        );
    }
}

#[test]
fn iface_pair_33_wide_enum_bitmap_boundary() {
    for count in [30, 31, 32] {
        let types = std::iter::repeat_n("Value", count)
            .collect::<Vec<_>>()
            .join(", ");
        let values = (0..count)
            .map(|i| format!("new Number({i}), "))
            .collect::<String>();
        let names = (0..count).map(|i| format!("v{i}, ")).collect::<String>();
        let sum = (0..count)
            .map(|i| format!("v{i}.get()"))
            .collect::<Vec<_>>()
            .join(" + ");
        check(
            &format!(
                r#"enum Wide {{ Data({types}, String) }}
            fn make() -> Wide {{ return Wide::Data({values} "kept" + "!"); }}
            fn main() {{ let p = make(); gc_collect(); match p {{
                Wide::Data({names} text) => {{ gc_minor_collect(); println(text); println({sum}); }}
            }} }}"#
            ),
            &format!("kept!\n{}\n", count * (count - 1) / 2),
        );
    }
}

perspective!(
    iface_pair_34_reference_pop_reuse_and_different_implementor,
    r#"interface Pad { fn pad(self) -> i64; }
    interface More extends Pad, Value {}
    class Other implements More {
        pub n: i64;
        pub fn pad(self) -> i64 { return 0; }
        pub fn get(self) -> i64 { return self.n + 100; }
        pub fn copy(self) -> Other { return new Other(self.n); }
    }
    fn write(v: &mut Value, a: Array<Value>) {
        let old = a.pop(); gc_collect();
        let replacement: More = new Other(2);
        let wide: Value = replacement;
        a.push(wide); gc_minor_collect();
        println(v.get());
        v = new Other(3);
        println(old.get());
    }
    fn main() {
        let a: Array<Value> = [new Number(1)];
        write(&a[0], a);
        println(a[0].get());
        a[0] = new Number(7);
        gc_collect(); println(a[0].get());
    }"#,
    "102\n1\n103\n7\n"
);
