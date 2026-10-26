//! Member calls through generic receivers report receiver-instantiated callee
//! types, and generic bodies never expose instantiation aliases (willow-25da.29.55).
use willow_compiler::{
    CompilerOptions, CompilerSession,
    ai::{QueryRequest, QuerySession, Snapshot},
    diagnostics::HumanEmitter,
};

const SOURCE: &str = include_str!("../example/generic_receiver_type_queries.wi");

fn snapshot(files: &[(&str, &str)], label: &str) -> Snapshot {
    let directory = std::env::temp_dir().join(format!(
        "willow-generic-receiver-types-{}-{label}",
        std::process::id()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    for (name, source) in files {
        std::fs::write(directory.join(name), source).unwrap();
    }
    let path = directory.join("main.wi");
    let result = CompilerSession::new(path.to_str().unwrap(), "", &CompilerOptions::debug(), None)
        .analysis_with_emitter(&mut HumanEmitter)
        .unwrap();
    std::fs::remove_dir_all(directory).unwrap();
    result
}

/// Byte of `token` inside the `occurrence`-th match of `needle`.
fn byte(source: &str, needle: &str, occurrence: usize, token: &str) -> usize {
    let start = source
        .match_indices(needle)
        .nth(occurrence)
        .unwrap_or_else(|| panic!("{needle} #{occurrence}"))
        .0;
    start + needle.find(token).unwrap()
}

fn type_at(
    session: &mut QuerySession,
    revision: &str,
    file: &str,
    byte: usize,
) -> serde_json::Value {
    session.query(QueryRequest::TypeAt {
        revision: revision.into(),
        file: file.into(),
        byte,
    })
}

fn both(snapshot: Snapshot) -> [Snapshot; 2] {
    let roundtrip: Snapshot =
        serde_json::from_slice(&serde_json::to_vec(&snapshot).unwrap()).unwrap();
    [snapshot, roundtrip]
}

#[test]
fn member_calls_substitute_receiver_type_arguments() {
    for snapshot in both(snapshot(&[("main.wi", SOURCE)], "members")) {
        let revision = snapshot.revision.clone();
        let mut session = QuerySession::new(snapshot).unwrap();
        for (needle, occurrence, token, expected) in [
            // Interface receiver: the reported ticket case and a parameter use.
            ("waiting.pop", 0, "pop", "fn() -> Option<Patient>"),
            ("waiting.push", 0, "push", "fn(Patient) -> void"),
            // Class receiver of the same instance.
            ("heap.pop", 0, "pop", "fn() -> Option<Patient>"),
            ("heap.push", 0, "push", "fn(Patient) -> void"),
            // A second instantiation of the same template.
            ("numbers.push", 0, "push", "fn(i64) -> void"),
            // Generic result types and a reordered class instance.
            ("numbers.pair", 0, "pair", "fn(i64) -> Pair<i64, i64>"),
            ("pair.swap", 0, "swap", "fn() -> Pair<i64, i64>"),
            // Nested generic argument.
            ("nested.pop", 0, "pop", "fn() -> Option<Option<Patient>>"),
            // Default interface method inherited by the class instance.
            ("numbers.peek_or", 0, "peek_or", "fn(i64) -> i64"),
            // Inside generic templates the receiver is still abstract.
            ("queue.pop", 0, "pop", "fn() -> Option<T>"),
            ("queue.peek_or", 0, "peek_or", "fn(T) -> T"),
            ("self.pop()", 0, "pop", "fn() -> Option<T>"),
            ("self.pop()", 1, "pop", "fn() -> Option<T>"),
        ] {
            let start = byte(SOURCE, needle, occurrence, token);
            // First, middle and last bytes of the member token agree.
            for offset in [0, token.len() / 2, token.len() - 1] {
                let result = type_at(&mut session, &revision, "main.wi", start + offset);
                assert_eq!(
                    result["result"]["type_display"], expected,
                    "{needle}#{occurrence}+{offset}: {result}"
                );
            }
            // The byte after the token (`(`) is not the callee token.
            let after = type_at(&mut session, &revision, "main.wi", start + token.len());
            assert_ne!(
                after["result"]["type_display"], expected,
                "{needle}: {after}"
            );
        }
    }
}

#[test]
fn generic_bodies_report_template_types_without_aliases() {
    // Live and restored snapshots render declarations identically.
    for snapshot in both(snapshot(&[("main.wi", SOURCE)], "bodies")) {
        let revision = snapshot.revision.clone();
        let mut session = QuerySession::new(snapshot).unwrap();
        for (needle, occurrence, token, expected) in [
            // Heap has three instantiations; template locals keep `T`.
            ("let less", 0, "less", "closure(T, T) -> bool"),
            ("let less", 1, "less", "closure(T, T) -> bool"),
            ("let current", 0, "current", "Option<T>"),
            ("let current", 1, "current", "Option<T>"),
            ("let held", 0, "held", "Option<T>"),
            ("less(old", 0, "less", "closure(T, T) -> bool"),
            ("self.less", 0, "less", "closure(T, T) -> bool"),
            // Default interface method and generic function bodies.
            ("let found", 0, "found", "Option<T>"),
            ("let next", 0, "next", "Option<T>"),
            // Declarations keep their generic signatures.
            (
                "fn pop(self) -> Option<T> {",
                0,
                "pop",
                "fn<T>() -> Option<T>",
            ),
            ("less: closure", 0, "less", "closure(T, T) -> bool"),
            // Concrete locals in the caller.
            ("let first", 0, "first", "Patient"),
            ("let direct", 0, "direct", "Option<Patient>"),
            ("let maybe", 0, "maybe", "Option<Option<Patient>>"),
            ("let swapped", 0, "swapped", "Pair<i64, i64>"),
        ] {
            let at = byte(SOURCE, needle, occurrence, token);
            let result = type_at(&mut session, &revision, "main.wi", at);
            assert_eq!(
                result["result"]["type_display"], expected,
                "{needle}#{occurrence}: {result}"
            );
        }
        // No position anywhere in the file reports an instantiation alias.
        for at in 0..SOURCE.len() {
            let result = type_at(&mut session, &revision, "main.wi", at);
            let display = result["result"]["type_display"].as_str().unwrap_or("");
            assert!(
                !display.contains('$'),
                "byte {at} ({:?}): {result}",
                &SOURCE[at..(at + 12).min(SOURCE.len())]
            );
        }
    }
}

#[test]
fn builtin_members_and_plain_symbols_are_unchanged() {
    let snapshot = snapshot(&[("main.wi", SOURCE)], "builtin");
    // Only resolved user member calls carry a substituted callee type.
    assert!(snapshot.semantic.references.iter().any(|r| r.ty.is_some()));
    assert!(
        snapshot
            .semantic
            .references
            .iter()
            .filter(|r| r.ty.is_some())
            .all(|r| r.role == "call")
    );
    let revision = snapshot.revision.clone();
    let mut session = QuerySession::new(snapshot).unwrap();
    // `.unwrap()` on Option<Patient> is a builtin member.
    let at = byte(SOURCE, "pop().unwrap", 0, "unwrap");
    let result = type_at(&mut session, &revision, "main.wi", at);
    let display = result["result"]["type_display"].as_str().unwrap_or("");
    assert!(!display.contains('T') && !display.contains('$'), "{result}");
    // A static generic function call keeps its declaration signature.
    let at = byte(SOURCE, "drain(waiting", 0, "drain");
    let result = type_at(&mut session, &revision, "main.wi", at);
    assert_eq!(result["result"]["type_display"], "fn<T>(PQueue<T>, T) -> T");
}

#[test]
fn absent_callee_type_is_omitted_and_older_snapshots_load() {
    let snapshot = snapshot(&[("main.wi", SOURCE)], "serde");
    let json = serde_json::to_value(&snapshot).unwrap();
    let references = json["semantic"]["references"].as_array().unwrap();
    assert!(references.iter().any(|r| r.get("ty").is_some()));
    assert!(
        references
            .iter()
            .filter(|r| r["role"] != "call")
            .all(|r| r.get("ty").is_none())
    );
    // Snapshot JSON written before the field existed still deserializes.
    let mut old = json.clone();
    for reference in old["semantic"]["references"].as_array_mut().unwrap() {
        reference.as_object_mut().unwrap().remove("ty");
    }
    let old: Snapshot = serde_json::from_value(old).unwrap();
    assert_eq!(old.semantic.references.len(), references.len());
    assert!(old.semantic.references.iter().all(|r| r.ty.is_none()));
}

#[test]
fn imported_generic_receivers_render_in_the_caller_module() {
    let queue = "pub interface PQueue<T> {\n    fn pop(self) -> Option<T>;\n}\n\
        pub class Heap<T> implements PQueue<T> {\n    item: T;\n    less: closure(T, T) -> bool;\n\
        pub init(self, item: T, less: closure(T, T) -> bool) {\n        self.item = item;\n        self.less = less;\n    }\n\
        pub fn pop(self) -> Option<T> {\n        let less = self.less;\n        if less(self.item, self.item) {\n            return None;\n        }\n        return Some(self.item);\n    }\n}\n";
    let patient = "pub class Patient {\n    pub id: i64;\n}\n";
    let main = "import queue::{PQueue, Heap};\nimport patient;\n\nfn main() {\n\
        let heap = new Heap<patient::Patient>(new patient::Patient(1), |a: patient::Patient, b: patient::Patient| a.id < b.id);\n\
        let mut waiting: PQueue<patient::Patient> = heap;\n    let p = waiting.pop().unwrap();\n    let direct = heap.pop();\n    println(p.id);\n}\n";
    let snapshot = snapshot(
        &[
            ("main.wi", main),
            ("queue.wi", queue),
            ("patient.wi", patient),
        ],
        "modules",
    );
    let revision = snapshot.revision.clone();
    let mut session = QuerySession::new(snapshot).unwrap();
    let direct = type_at(
        &mut session,
        &revision,
        "main.wi",
        byte(main, "let direct", 0, "direct"),
    );
    let expected = direct["result"]["type_display"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(expected.contains("Patient"), "{direct}");
    for needle in ["waiting.pop", "heap.pop"] {
        let result = type_at(
            &mut session,
            &revision,
            "main.wi",
            byte(main, needle, 0, "pop"),
        );
        // The callee spells the type argument exactly as the caller's locals do.
        assert_eq!(
            result["result"]["type_display"],
            format!("fn() -> {expected}"),
            "{needle}: {result}"
        );
    }
    let less = type_at(
        &mut session,
        &revision,
        "queue.wi",
        byte(queue, "let less", 0, "less"),
    );
    assert_eq!(
        less["result"]["type_display"], "closure(T, T) -> bool",
        "{less}"
    );
}

#[test]
fn typed_callee_evidence_grows_linearly_with_member_calls() {
    let template = "class Cell<T> {\n    value: T;\n    pub init(self, value: T) { self.value = value; }\n    pub fn get(self) -> T { return self.value; }\n}\n";
    for n in [1, 16, 64] {
        let calls = "    cell.get();\n".repeat(n);
        let source =
            format!("{template}fn main() {{\n    let cell = new Cell<i64>(1);\n{calls}}}\n");
        let snapshot = snapshot(&[("main.wi", &source)], &format!("scale-{n}"));
        let typed: Vec<_> = snapshot
            .semantic
            .references
            .iter()
            .filter(|r| r.ty.is_some() && !r.location.path.starts_with("<std>"))
            .collect();
        // One substituted callee type per call token; none for declarations.
        assert_eq!(typed.len(), n);
        println!("member_calls={n} typed_references={}", typed.len());
        let revision = snapshot.revision.clone();
        let mut session = QuerySession::new(snapshot).unwrap();
        let last = source.rfind("cell.get").unwrap() + "cell.".len();
        let result = type_at(&mut session, &revision, "main.wi", last);
        assert_eq!(result["result"]["type_display"], "fn() -> i64", "{result}");
    }
}

#[test]
fn non_generic_member_calls_store_no_redundant_callee_type() {
    let source = "class Plain {\n    value: i64;\n    pub init(self, value: i64) { self.value = value; }\n    pub fn get(self) -> i64 { return self.value; }\n}\nfn main() {\n    let plain = new Plain(1);\n    println(plain.get());\n}\n";
    let snapshot = snapshot(&[("main.wi", source)], "plain");
    // The declaration already answers; no per-call copy is persisted.
    assert!(
        snapshot
            .semantic
            .references
            .iter()
            .filter(|r| !r.location.path.starts_with("<std>"))
            .all(|r| r.ty.is_none())
    );
    let revision = snapshot.revision.clone();
    let mut session = QuerySession::new(snapshot).unwrap();
    let at = byte(source, "plain.get", 0, "get");
    let result = type_at(&mut session, &revision, "main.wi", at);
    assert_eq!(result["result"]["type_display"], "fn() -> i64", "{result}");
}

#[test]
fn owner_and_importer_instances_restore_one_stable_spelling() {
    let queue = "pub class Heap<T> {\n    item: T;\n    pub init(self, item: T) { self.item = item; }\n    pub fn pop(self) -> Option<T> { return Some(self.item); }\n}\n";
    // The type argument's own module instantiates the generic with its bare
    // local name; the importer spells it qualified. Both restore the one
    // module-qualified identity, independent of unit iteration order.
    let patient = "import queue::Heap;\npub class Patient {\n    pub id: i64;\n}\npub fn admit() -> i64 {\n    let local = new Heap<Patient>(new Patient(2));\n    let held = local.pop();\n    return held.unwrap().id;\n}\n";
    let main = "import queue::Heap;\nimport patient;\nfn main() {\n    let heap = new Heap<patient::Patient>(new patient::Patient(1));\n    let direct = heap.pop();\n    println(direct.unwrap().id + patient::admit());\n}\n";
    let files = [
        ("main.wi", main),
        ("queue.wi", queue),
        ("patient.wi", patient),
    ];
    let first = snapshot(&files, "owner");
    // Unit iteration order must not change the restored snapshot.
    for _ in 0..4 {
        let again = snapshot(&files, "owner");
        assert_eq!(again.revision, first.revision);
    }
    let all: Vec<_> = serde_json::to_string(&first)
        .unwrap()
        .match_indices("$type")
        .map(|m| m.0)
        .collect();
    assert!(all.is_empty(), "alias leaked at {all:?}");
    let revision = first.revision.clone();
    let mut session = QuerySession::new(first).unwrap();
    for (file, source, needle, token, expected) in [
        (
            "main.wi",
            main,
            "let direct",
            "direct",
            "Option<patient::Patient>",
        ),
        (
            "main.wi",
            main,
            "heap.pop",
            "pop",
            "fn() -> Option<patient::Patient>",
        ),
        (
            "patient.wi",
            patient,
            "let held",
            "held",
            "Option<patient::Patient>",
        ),
        (
            "patient.wi",
            patient,
            "local.pop",
            "pop",
            "fn() -> Option<patient::Patient>",
        ),
    ] {
        let result = type_at(
            &mut session,
            &revision,
            file,
            byte(source, needle, 0, token),
        );
        assert_eq!(
            result["result"]["type_display"], expected,
            "{file} {needle}: {result}"
        );
    }
}
