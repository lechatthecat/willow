use super::*;

#[test]
fn iface_dispatch_01_basic_via_function_arg() {
    let (out, ok) = compile_and_run(&format!(
        "{IFACE_ANIMALS}\nfn say(a: Animal) {{ println(a.speak()); }}\nfn main() {{ say(new Dog()); say(new Cat()); }}"
    ));
    assert!(ok, "interface dispatch must compile and run");
    assert_eq!(out, "woof\nmeow\n");
}

#[test]
fn iface_dispatch_02_local_binding() {
    let (out, ok) = compile_and_run(&format!(
        "{IFACE_ANIMALS}\nfn main() {{ let a: Animal = new Dog(); println(a.speak()); }}"
    ));
    assert!(ok);
    assert_eq!(out, "woof\n");
}

#[test]
fn iface_dispatch_03_return_coercion() {
    let (out, ok) = compile_and_run(&format!(
        "{IFACE_ANIMALS}\nfn pick(b: bool) -> Animal {{ if b {{ return new Dog(); }} return new Cat(); }}\nfn main() {{ println(pick(true).speak()); println(pick(false).speak()); }}"
    ));
    assert!(ok);
    assert_eq!(out, "woof\nmeow\n");
}

#[test]
fn iface_dispatch_04_multi_method_slot_indexing() {
    // Calls both interface methods; the second exercises vtable slot 1.
    let (out, ok) = compile_and_run(
        r#"
interface Shape {
    fn name(self) -> String;
    fn area(self) -> i64;
}
class Square implements Shape {
    pub side: i64;
    pub fn name(self) -> String { return "square"; }
    pub fn area(self) -> i64 { return self.side * self.side; }
}
fn show(s: Shape) { println(s.name()); println(s.area()); }
fn main() { show(new Square(6)); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "square\n36\n");
}

#[test]
fn iface_dispatch_05_reassignment() {
    let (out, ok) = compile_and_run(&format!(
        "{IFACE_ANIMALS}\nfn main() {{ let mut a: Animal = new Dog(); println(a.speak()); a = new Cat(); println(a.speak()); }}"
    ));
    assert!(ok);
    assert_eq!(out, "woof\nmeow\n");
}

// spec 14.6: interface values must survive collection under GC stress.

#[test]
fn iface_gc_stress_01_local_survives() {
    let (out, ok) = compile_and_run_gc_stress(&format!(
        "{IFACE_ANIMALS}\nfn main() {{ let a: Animal = new Dog(); gc_collect(); println(a.speak()); }}"
    ));
    assert!(ok, "interface local must survive GC: {out}");
    assert_eq!(out, "woof\n");
}

#[test]
fn iface_gc_stress_02_param_survives() {
    let (out, ok) = compile_and_run_gc_stress(&format!(
        "{IFACE_ANIMALS}\nfn say(a: Animal) {{ gc_collect(); println(a.speak()); }}\nfn main() {{ say(new Dog()); }}"
    ));
    assert!(ok, "interface parameter must survive GC: {out}");
    assert_eq!(out, "woof\n");
}

#[test]
fn iface_gc_stress_03_method_result_string_survives() {
    let (out, ok) = compile_and_run_gc_stress(&format!(
        "{IFACE_ANIMALS}\nfn main() {{ let a: Animal = new Dog(); let s = a.speak(); gc_collect(); println(s); }}"
    ));
    assert!(ok, "interface method-result String must survive GC: {out}");
    assert_eq!(out, "woof\n");
}

// spec 14.4: a class field typed as an interface.
#[test]
fn iface_field_01_dispatch_through_field() {
    let (out, ok) = compile_and_run(&format!(
        "{IFACE_ANIMALS}\nclass Holder {{ pub value: Animal; }}\nfn main() {{ let h = new Holder(new Dog()); println(h.value.speak()); }}"
    ));
    assert!(ok, "interface field dispatch must work: {out}");
    assert_eq!(out, "woof\n");
}

#[test]
fn iface_field_02_gc_stress_field_survives() {
    let (out, ok) = compile_and_run_gc_stress(&format!(
        "{IFACE_ANIMALS}\nclass Holder {{ pub value: Animal; }}\nfn main() {{ let h = new Holder(new Dog()); gc_collect(); println(h.value.speak()); }}"
    ));
    assert!(ok, "interface field must survive GC: {out}");
    assert_eq!(out, "woof\n");
}

// spec 14.5: Array<Interface> (empty literal + push, the documented pattern).
#[test]
fn iface_array_01_push_and_dispatch() {
    let (out, ok) = compile_and_run(&format!(
        "import std::collections::Array;\n{IFACE_ANIMALS}\nfn main() {{ let xs: Array<Animal> = []; xs.push(new Dog()); xs.push(new Cat()); println(xs[0].speak()); println(xs[1].speak()); }}"
    ));
    assert!(ok, "Array<Interface> must work: {out}");
    assert_eq!(out, "woof\nmeow\n");
}

#[test]
fn iface_array_02_gc_stress_elements_survive() {
    let (out, ok) = compile_and_run_gc_stress(&format!(
        "import std::collections::Array;\n{IFACE_ANIMALS}\nfn main() {{ let xs: Array<Animal> = []; xs.push(new Dog()); xs.push(new Cat()); gc_collect(); println(xs[0].speak()); println(xs[1].speak()); }}"
    ));
    assert!(ok, "Array<Interface> elements must survive GC: {out}");
    assert_eq!(out, "woof\nmeow\n");
}

#[test]
fn iface_array_03_index_assign_boxes() {
    let (out, ok) = compile_and_run(&format!(
        "import std::collections::Array;\n{IFACE_ANIMALS}\nfn main() {{ let xs: Array<Animal> = []; xs.push(new Dog()); xs[0] = new Cat(); println(xs[0].speak()); }}"
    ));
    assert!(ok, "interface index-assign must box: {out}");
    assert_eq!(out, "meow\n");
}

#[test]
fn iface_array_04_nonempty_literal_with_annotation() {
    // A non-empty `Array<Interface>` literal of differing classes is checked
    // element-wise against the interface and each element is boxed.
    let (out, ok) = compile_and_run(&format!(
        "import std::collections::Array;\n{IFACE_ANIMALS}\nfn main() {{ let xs: Array<Animal> = [new Dog(), new Cat()]; println(xs[0].speak()); println(xs[1].speak()); }}"
    ));
    assert!(ok, "non-empty interface array literal must work: {out}");
    assert_eq!(out, "woof\nmeow\n");
}

// spec 11: module-qualified interface use (`animals::Animal`) where both the
// interface and the implementing class live in an imported module.
#[test]
fn iface_module_01_qualified_interface_and_class() {
    let animals = r#"
module animals;
pub interface Animal {
    fn speak(self) -> String;
}
pub class Dog implements Animal {
    pub fn speak(self) -> String { return "woof"; }
}
"#;
    let main = r#"
import animals;
fn say(a: animals::Animal) {
    println(a.speak());
}
fn main() {
    say(new animals::Dog());
    let a: animals::Animal = new animals::Dog();
    println(a.speak());
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("animals.wi", animals), ("main.wi", main)], "main.wi");
    assert!(ok, "module-qualified interface project failed: {out}");
    assert_eq!(out, "woof\nwoof\n");
}
