use super::*;

// ── Class codegen ────────────────────────────────────────────────────────────

#[test]
fn test_class_instantiation_and_field_access() {
    let src = r#"
class Point {
    pub init(self, x: i64, y: i64) {
        self.x = x;
        self.y = y;
    }
    x: i64;
    y: i64;

    pub fn get_x(self) -> i64 { return self.x; }
    pub fn get_y(self) -> i64 { return self.y; }
}

fn main() {
    let p = new Point(10, 20);
    println(p.get_x());
    println(p.get_y());
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "10\n20\n");
}

#[test]
fn test_class_gc_bitmap_traces_gc_field_beyond_inline_coverage() {
    let mut fields = String::new();
    let mut args = Vec::new();
    for i in 0..63 {
        fields.push_str(&format!("    pub n{i}: i64;\n"));
        args.push(i.to_string());
    }
    fields.push_str("    pub late: String;\n");
    args.push("\"late\"".to_string());

    let src = format!(
        r#"
class TooWide {{
{fields}}}

fn main() {{
    let value = new TooWide({});
    gc_collect();
    println(value.late);
}}
"#,
        args.join(", ")
    );
    let (out, ok) = compile_and_run(&src);
    assert!(ok, "{out}");
    assert_eq!(out, "late\n");
}

#[test]
fn test_class_method_with_arithmetic() {
    let src = r#"
class Counter {
    pub init(self, count: i64) {
        self.count = count;
    }
    count: i64;

    pub fn value(self) -> i64 { return self.count; }
    pub fn doubled(self) -> i64 { return self.count * 2; }
    pub fn add(self, n: i64) -> i64 { return self.count + n; }
}

fn main() {
    let c = new Counter(5);
    println(c.value());
    println(c.doubled());
    println(c.add(10));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "5\n10\n15\n");
}

#[test]
fn test_class_method_call_chained_in_println() {
    let src = r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    }
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}

fn main() {
    let b = new Box(99);
    println(b.get());
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "99\n");
}
