use std::collections::HashMap;
use std::process::Command;

#[test]
fn query_stats_reports_real_operations_and_keeps_empty_modules_distinct() {
    let root = std::env::temp_dir().join(format!("willow-query-stats-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(root.clone());
    std::fs::write(root.join("empty.wi"), "").unwrap();
    std::fs::write(
        root.join("value.wi"),
        "pub fn value() -> i64 { return 42; }",
    )
    .unwrap();
    std::fs::write(root.join("main.wi"),
        "import empty; import value; class C { pub n: i64; } fn main() { println(new C(value::value()).n); }").unwrap();
    for enabled in [false, true] {
        let mut build = Command::new(env!("CARGO_BIN_EXE_willow"));
        build
            .arg("build")
            .arg(root.join("main.wi"))
            .arg("-o")
            .arg(root.join("app"));
        if enabled {
            build.env("WILLOW_QUERY_STATS", "1");
        } else {
            build.env_remove("WILLOW_QUERY_STATS");
        }
        let output = build.output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        let lines: Vec<_> = stderr
            .lines()
            .filter(|line| line.starts_with("[query-stats]"))
            .collect();
        assert_eq!(lines.len(), usize::from(enabled), "{stderr}");
        if enabled {
            let fields: HashMap<_, _> = lines[0]
                .split_whitespace()
                .skip(1)
                .map(|field| field.split_once('=').unwrap())
                .collect();
            for counter in [
                "type_checkers",
                "nonpreemptible_helpers",
                "class_layouts",
                "class_vslots",
                "effect_solves",
            ] {
                assert!(
                    fields[counter].parse::<usize>().unwrap() > 0,
                    "{counter}: {stderr}"
                );
            }
            assert_eq!(fields["type_checkers"], "3", "{stderr}");
            assert_eq!(fields["nonpreemptible_helpers"], "3", "{stderr}");
            assert_eq!(fields["effect_solves"], "3", "{stderr}");
            assert!(
                lines[0].contains("unit_effects[calls=3,hits=0,computations=3,"),
                "{stderr}"
            );
            let units: Vec<_> = fields["hydrates_by_file"]
                .split(',')
                .map(|pair| pair.split_once(':').unwrap())
                .collect();
            assert_eq!(
                units.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
                ["0", "1", "2"]
            );
            assert_eq!(
                units
                    .iter()
                    .map(|(_, count)| count.parse::<usize>().unwrap())
                    .sum::<usize>(),
                fields["hydrates"].parse::<usize>().unwrap()
            );
            // `C` is laid out once; the allocation and the `.n` read reuse it.
            let object_layout = lines[0]
                .split_whitespace()
                .find_map(|field| field.strip_prefix("object_layout["))
                .and_then(|field| field.strip_suffix(']'))
                .unwrap_or_else(|| panic!("no object_layout stats: {stderr}"));
            let object_layout: HashMap<_, _> = object_layout
                .split(',')
                .map(|pair| pair.split_once('=').unwrap())
                .collect();
            assert_eq!(object_layout["computations"], "1", "{stderr}");
            assert!(
                object_layout["frozen_reads"].parse::<usize>().unwrap() >= 1,
                "{stderr}"
            );
            for peak in ["peak_ast", "peak_checker", "peak_declared", "peak_lir"] {
                assert_eq!(fields[peak], "1", "{stderr}");
            }
        }
        let output = Command::new(root.join("app")).output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"42\n");
    }
    // Early errors still close the session and emit exactly one zero report.
    let output = Command::new(env!("CARGO_BIN_EXE_willow"))
        .arg("build")
        .arg(root.join("missing.wi"))
        .env("WILLOW_QUERY_STATS", "1")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.matches("[query-stats]").count(), 1, "{stderr}");
    assert!(stderr.contains("hydrates=0 type_checkers=0"), "{stderr}");
}
