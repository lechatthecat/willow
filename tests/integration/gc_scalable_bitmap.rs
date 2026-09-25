//! Scalable GC bitmap boundaries: twenty class and twenty async-frame
//! perspectives. Each reference is read after both minor and full collection.
use super::support::compile_and_run_with_env;

fn check_class(count: usize, stress: &str) {
    let fields = (0..count)
        .map(|i| format!("pub field_{i}: String;"))
        .collect::<Vec<_>>()
        .join("\n");
    let args = (0..count)
        .map(|i| format!("\"v{i}\" + \"!\""))
        .collect::<Vec<_>>()
        .join(",");
    let reads = (0..count)
        .map(|i| format!("println(value.field_{i});"))
        .collect::<Vec<_>>()
        .join("\n");
    let source = format!(
        "class Wide {{ {fields} }} fn main() {{ let value = new Wide({args}); gc_minor_collect(); gc_collect(); {reads} }}"
    );
    let expected = (0..count).map(|i| format!("v{i}!\n")).collect::<String>();
    let (out, ok) = compile_and_run_with_env(
        &source,
        &[("WILLOW_GC_STRESS", stress), ("WILLOW_WORKERS", "1")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, expected);
}

fn check_frame(count: usize, main: bool) {
    let locals = (0..count)
        .map(|i| format!("let v{i}: String = \"v{i}\" + \"!\";"))
        .collect::<Vec<_>>()
        .join("\n");
    let reads = (0..count)
        .map(|i| format!("println(v{i});"))
        .collect::<Vec<_>>()
        .join("\n");
    let body = format!("{locals} await sleep(0); gc_minor_collect(); gc_collect(); {reads}");
    let source = if main {
        format!("async fn main() {{ {body} }}")
    } else {
        format!("async fn worker() {{ {body} }} async fn main() {{ await worker(); }}")
    };
    let expected = (0..count).map(|i| format!("v{i}!\n")).collect::<String>();
    let (out, ok) = compile_and_run_with_env(
        &source,
        &[
            ("WILLOW_GC_STRESS", "minor"),
            ("WILLOW_WORKERS", "1"),
            ("WILLOW_TASK_BUDGET", "1"),
        ],
    );
    assert!(ok, "{out}");
    assert_eq!(out, expected);
}

#[test]
fn bitmap_class_01_alloc() {
    check_class(1, "alloc");
}

#[test]
fn bitmap_class_01_minor() {
    check_class(1, "minor");
}

#[test]
fn bitmap_frame_01_main() {
    check_frame(1, true);
}

#[test]
fn bitmap_frame_01_function() {
    check_frame(1, false);
}

#[test]
fn bitmap_class_02_alloc() {
    check_class(60, "alloc");
}

#[test]
fn bitmap_class_02_minor() {
    check_class(60, "minor");
}

#[test]
fn bitmap_frame_02_main() {
    check_frame(60, true);
}

#[test]
fn bitmap_frame_02_function() {
    check_frame(60, false);
}

#[test]
fn bitmap_class_03_alloc() {
    check_class(61, "alloc");
}

#[test]
fn bitmap_class_03_minor() {
    check_class(61, "minor");
}

#[test]
fn bitmap_frame_03_main() {
    check_frame(61, true);
}

#[test]
fn bitmap_frame_03_function() {
    check_frame(61, false);
}

#[test]
fn bitmap_class_04_alloc() {
    check_class(62, "alloc");
}

#[test]
fn bitmap_class_04_minor() {
    check_class(62, "minor");
}

#[test]
fn bitmap_frame_04_main() {
    check_frame(62, true);
}

#[test]
fn bitmap_frame_04_function() {
    check_frame(62, false);
}

#[test]
fn bitmap_class_05_alloc() {
    check_class(63, "alloc");
}

#[test]
fn bitmap_class_05_minor() {
    check_class(63, "minor");
}

#[test]
fn bitmap_frame_05_main() {
    check_frame(63, true);
}

#[test]
fn bitmap_frame_05_function() {
    check_frame(63, false);
}

#[test]
fn bitmap_class_06_alloc() {
    check_class(64, "alloc");
}

#[test]
fn bitmap_class_06_minor() {
    check_class(64, "minor");
}

#[test]
fn bitmap_frame_06_main() {
    check_frame(64, true);
}

#[test]
fn bitmap_frame_06_function() {
    check_frame(64, false);
}

#[test]
fn bitmap_class_07_alloc() {
    check_class(65, "alloc");
}

#[test]
fn bitmap_class_07_minor() {
    check_class(65, "minor");
}

#[test]
fn bitmap_frame_07_main() {
    check_frame(65, true);
}

#[test]
fn bitmap_frame_07_function() {
    check_frame(65, false);
}

#[test]
fn bitmap_class_08_alloc() {
    check_class(127, "alloc");
}

#[test]
fn bitmap_class_08_minor() {
    check_class(127, "minor");
}

#[test]
fn bitmap_frame_08_main() {
    check_frame(127, true);
}

#[test]
fn bitmap_frame_08_function() {
    check_frame(127, false);
}

#[test]
fn bitmap_class_09_alloc() {
    check_class(128, "alloc");
}

#[test]
fn bitmap_class_09_minor() {
    check_class(128, "minor");
}

#[test]
fn bitmap_frame_09_main() {
    check_frame(128, true);
}

#[test]
fn bitmap_frame_09_function() {
    check_frame(128, false);
}

#[test]
fn bitmap_class_10_alloc() {
    check_class(193, "alloc");
}

#[test]
fn bitmap_class_10_minor() {
    check_class(193, "minor");
}

#[test]
fn bitmap_frame_10_main() {
    check_frame(193, true);
}

#[test]
fn bitmap_frame_10_function() {
    check_frame(193, false);
}

#[test]
fn repeated_wide_class_sites_emit_one_bitmap_descriptor() {
    use super::support::{temp_path, unique_test_id};
    use std::{fs, process::Command};

    for sites in [1, 4, 16] {
        let id = unique_test_id();
        let src = temp_path(format!("bitmap_dedup_{id}.wi"));
        let bin = temp_path(format!("bitmap_dedup_{id}"));
        let obj = format!(
            "{bin}.{}",
            if cfg!(all(target_os = "windows", target_env = "msvc")) {
                "obj"
            } else {
                "o"
            }
        );
        let fields = (0..65)
            .map(|i| format!("pub field_{i}: String;"))
            .collect::<String>();
        let args = vec!["\"kept\""; 65].join(",");
        let functions = (0..sites)
            .map(|i| format!("fn make{i}() -> Wide {{ return new Wide({args}); }}"))
            .collect::<String>();
        let calls = (0..sites)
            .map(|i| format!("let v{i} = make{i}(); gc_minor_collect(); println(v{i}.field_64);"))
            .collect::<String>();
        fs::write(
            &src,
            format!("class Wide {{ {fields} }} {functions} fn main() {{ {calls} }}"),
        )
        .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_willow"))
            .args(["build", &src, "-o", &bin])
            .env("WILLOW_KEEP_OBJECT", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let bytes = fs::read(&obj).unwrap();
        let descriptor = [2_u64, u64::MAX - 1, 3]
            .into_iter()
            .flat_map(u64::to_ne_bytes)
            .collect::<Vec<_>>();
        assert_eq!(
            bytes
                .windows(descriptor.len())
                .filter(|window| *window == descriptor)
                .count(),
            1,
            "{sites} sites"
        );
        let output = Command::new(&bin)
            .env("WILLOW_GC_STRESS", "minor")
            .env("WILLOW_WORKERS", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "kept\n".repeat(sites)
        );
        for path in [&src, &bin, &obj] {
            let _ = fs::remove_file(path);
        }
    }
}
