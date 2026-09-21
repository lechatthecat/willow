use super::*;

// ── Interface -> concrete downcast via match (willow-1js.4) ──────────────────

#[test]
fn downcast_01_matches_concrete_and_calls_method() {
    let (out, ok) = compile_and_run(
        r#"
interface Animal { fn name(self) -> String; }
class Dog implements Animal {
    pub fn name(self) -> String { return "Rex"; }
    pub fn bark(self) -> String { return "woof"; }
}
class Cat implements Animal {
    pub fn name(self) -> String { return "Tom"; }
    pub fn meow(self) -> String { return "meow"; }
}
fn sound(a: Animal) -> String {
    return match a {
        Dog(d) => d.bark(),
        Cat(c) => c.meow(),
        _ => "?",
    };
}
fn main() {
    println(sound(new Dog()));
    println(sound(new Cat()));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "woof\nmeow\n");
}

#[test]
fn downcast_02_wildcard_handles_other_classes() {
    let (out, ok) = compile_and_run(
        r#"
interface Animal { fn name(self) -> String; }
class Dog implements Animal { pub fn name(self) -> String { return "Rex"; } pub fn bark(self) -> String { return "woof"; } }
class Fish implements Animal { pub fn name(self) -> String { return "Nemo"; } }
fn sound(a: Animal) -> String {
    return match a {
        Dog(d) => d.bark(),
        _ => a.name(),
    };
}
fn main() {
    println(sound(new Dog()));
    println(sound(new Fish()));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "woof\nNemo\n");
}

#[test]
fn downcast_03_underscore_binding_no_bind() {
    let (out, ok) = compile_and_run(
        r#"
interface Animal { fn name(self) -> String; }
class Dog implements Animal { pub fn name(self) -> String { return "Rex"; } }
class Cat implements Animal { pub fn name(self) -> String { return "Tom"; } }
fn kind(a: Animal) -> String {
    return match a {
        Dog(_) => "dog",
        Cat(_) => "cat",
        _ => "other",
    };
}
fn main() {
    println(kind(new Dog()));
    println(kind(new Cat()));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "dog\ncat\n");
}

#[test]
fn downcast_04_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
interface Animal { fn name(self) -> String; }
class Dog implements Animal { pub fn name(self) -> String { return "Rex"; } pub fn bark(self) -> String { return "woof " + self.name(); } }
class Cat implements Animal { pub fn name(self) -> String { return "Tom"; } }
fn sound(a: Animal) -> String {
    return match a {
        Dog(d) => d.bark(),
        _ => a.name(),
    };
}
fn main() {
    println(sound(new Dog()));
    println(sound(new Cat()));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "woof Rex\nTom\n");
}

#[test]
fn downcast_04b_debug_binary_preserves_matching_and_fallback() {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_downcast_guard_{}.wi", id));
    let bin_path = temp_path(format!("willow_downcast_guard_{}", id));
    let source = r#"
interface Animal { fn name(self) -> String; }
class Dog implements Animal { pub fn name(self) -> String { return "Rex"; } }
class Cat implements Animal { pub fn name(self) -> String { return "Tom"; } }
fn kind(a: Animal) -> String {
    return match a {
        Dog(_) => "dog",
        _ => "other",
    };
}
fn main() { println(kind(new Cat())); println(kind(new Dog())); }
"#;
    std::fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = std::process::Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .output()
        .expect("failed to compile");
    assert!(output.status.success(), "should compile");

    // Unreferenced builtin strings are not a diagnostic contract: native
    // linkers may remove them. Verify the emitted downcast paths execute.
    // Interface guard calls/contexts are pinned by emit_interface's object test.
    let execution = std::process::Command::new(&bin_path)
        .output()
        .expect("failed to run debug downcast binary");

    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&bin_path);
    let _ = std::fs::remove_file(format!("{bin_path}.wsmap"));

    assert!(execution.status.success());
    assert_eq!(execution.stdout, b"other\ndog\n");
}

#[test]
fn downcast_neg_01_non_interface_scrutinee() {
    assert!(expect_compile_error(
        r#"
class Dog { pub fn bark(self) -> String { return "w"; } }
fn main() {
    let d = new Dog();
    let s = match d { Dog(x) => x.bark(), _ => "no" };
    println(s);
}
"#,
    ));
}

#[test]
fn downcast_neg_02_class_not_implementing_interface() {
    assert!(expect_compile_error(
        r#"
interface Animal { fn name(self) -> String; }
class Dog implements Animal { pub fn name(self) -> String { return "R"; } }
class Tree { pub fn h(self) -> i64 { return 1; } }
fn f(a: Animal) -> i64 { return match a { Tree(t) => t.h(), _ => 0 }; }
fn main() {}
"#,
    ));
}
