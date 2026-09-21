use super::*;

#[test]
fn test_debug_build_emits_source_map_sidecar() {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_sourcemap_{}.wi", id));
    let bin_path = temp_path(format!("willow_sourcemap_{}", id));

    let source = r#"
fn helper(x: i64) -> i64 {
    let doubled = x * 2;
    if doubled > 10 {
        return doubled;
    }
    return doubled + 1;
}

pub class Counter {
    pub fn value(self) -> i64 {
        return 1;
    }
}

fn main() {
    println(helper(6));
}
"#;
    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .output()
        .expect("failed to run compiler");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        panic!("debug compilation failed: {stderr}");
    }

    let map_path = format!("{bin_path}.wsmap");
    let map = fs::read_to_string(&map_path).expect("debug build should emit a source map");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    assert!(map.contains("willow_debug_source_map_v1"));
    assert!(map.contains(&format!("file={src_path}")));
    assert!(map.contains("function name=helper"));
    assert!(map.contains("function name=Counter::value"));
    assert!(map.contains("function name=main"));
    assert!(map.contains("statement kind=let"));
    assert!(map.contains("statement kind=if"));
    assert!(map.contains("statement kind=return"));
    assert!(map.contains("statement kind=expr"));
    assert!(map.contains(" line="));
    assert!(map.contains(" col="));
}

#[test]
fn test_release_build_removes_source_map_sidecar() {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_release_sourcemap_{}.wi", id));
    let bin_path = temp_path(format!("willow_release_sourcemap_{}", id));
    let map_path = format!("{bin_path}.wsmap");

    fs::write(&src_path, "fn main() { println(1); }").unwrap();
    fs::write(&map_path, "stale debug source map").unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path, "--release"])
        .output()
        .expect("failed to run compiler");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        panic!("release compilation failed: {stderr}");
    }

    let source_map_exists = Path::new(&map_path).exists();

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    assert!(
        !source_map_exists,
        "release build should not keep {map_path}"
    );
}

#[test]
fn test_release_with_debug_info_emits_source_map_sidecar() {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_release_debug_sourcemap_{}.wi", id));
    let bin_path = temp_path(format!("willow_release_debug_sourcemap_{}", id));
    let map_path = format!("{bin_path}.wsmap");

    fs::write(
        &src_path,
        r#"
fn helper() -> i64 {
    return 7;
}

fn main() {
    println(helper());
}
"#,
    )
    .unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = Command::new(compiler)
        .args([
            "build",
            &src_path,
            "-o",
            &bin_path,
            "--release",
            "--debug-info",
        ])
        .output()
        .expect("failed to run compiler");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        panic!("release-with-debug-info compilation failed: {stderr}");
    }

    let map = fs::read_to_string(&map_path).expect("release --debug-info should emit a source map");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    assert!(map.contains("willow_debug_source_map_v1"));
    assert!(map.contains(&format!("file={src_path}")));
    assert!(map.contains("function name=helper"));
    assert!(map.contains("function name=main"));
}

#[test]
fn test_debug_build_embeds_runtime_metadata_in_binary() {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_runtime_metadata_{}.wi", id));
    let bin_path = temp_path(format!("willow_runtime_metadata_{}", id));

    let source = r#"
fn helper(x: i64) -> i64 {
    return x + 1;
}

pub class Counter {
    pub value: i64;

    pub fn read(self) -> i64 {
        return 1;
    }
}

fn main() {
    println(helper(41));
}
"#;
    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .output()
        .expect("failed to run compiler");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        panic!("debug compilation failed: {stderr}");
    }

    let binary = fs::read(&bin_path).expect("debug binary should exist");
    let metadata = String::from_utf8_lossy(&binary);

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    assert!(metadata.contains("willow_runtime_metadata_v1"));
    assert!(metadata.contains("willow_debug_source_map_v1"));
    assert!(metadata.contains(&format!("file={src_path}")));
    assert!(metadata.contains("function name=helper line="));
    assert!(metadata.contains("function name=main line="));
    assert!(metadata.contains("class name=Counter line="));
    assert!(metadata.contains("gc_type name=Counter"));
    assert!(metadata.contains("field name=value line="));
    assert!(metadata.contains("method name=read line="));
    assert!(metadata.contains("function name=Counter::read line="));
}

#[test]
fn test_debug_build_embeds_async_stack_metadata_in_binary() {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_async_metadata_{}.wi", id));
    let bin_path = temp_path(format!("willow_async_metadata_{}", id));

    let source = r#"
async fn wait_value() -> i64 {
    await sleep(1);
    return 42;
}

async fn main() {
    let value = await wait_value();
    println(value);
}
"#;
    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .output()
        .expect("failed to run compiler");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        panic!("async debug compilation failed: {stderr}");
    }

    let binary = fs::read(&bin_path).expect("debug binary should exist");
    let metadata = String::from_utf8_lossy(&binary);

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    assert!(metadata.contains("function name=wait_value line="));
    assert!(metadata.contains("function name=main line="));
    assert!(metadata.contains("  async=true"));
    assert!(metadata.contains("  async_stack_frame name=wait_value"));
    assert!(metadata.contains("  async_stack_frame name=main"));
    assert!(metadata.contains("  await line="));
}

#[test]
fn test_debug_source_map_records_reference_params_and_call_sites() {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_ref_metadata_{}.wi", id));
    let bin_path = temp_path(format!("willow_ref_metadata_{}", id));

    let source = r#"
fn read(x: & i64) -> i64 {
    return x;
}

fn bump(x: &mut i64) {
    x = x + 1;
}

fn main() {
    let mut n = 1;
    println(read(&n));
    bump(&n);
}
"#;
    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .output()
        .expect("failed to run compiler");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        panic!("reference metadata compilation failed: {stderr}");
    }

    let map = fs::read_to_string(format!("{bin_path}.wsmap"))
        .expect("debug build should emit reference metadata");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    assert!(map.contains("function name=read line="));
    assert!(map.contains("param name=x mode=& type=i64"));
    assert!(map.contains("function name=bump line="));
    assert!(map.contains("param name=x mode=&mut type=i64"));
    assert!(
        map.contains("reference_call callee=read param=x mode=& type=i64 place_kind=local place=n")
    );
    assert!(map.contains(
        "reference_call callee=bump param=x mode=&mut type=i64 place_kind=local place=n"
    ));
}

#[test]
fn test_reference_runtime_debug_hook_reports_array_element_call_site() {
    let src = r#"
import std::collections::Array;

fn increment(x: &mut i64) {
    x = x + 1;
}

fn main() {
    let mut xs: Array<i64> = [1];
    increment(&xs[3]);
}
"#;
    let (out, ok) = compile_and_run_with_env(src, &[]);
    assert!(!ok, "out-of-bounds reference call should abort");
    assert!(
        out.contains("array index out of bounds: the length is 1 but the index is 3"),
        "missing array bounds diagnostic:\n{out}"
    );
    assert!(
        out.contains("reference call: increment parameter `x` &mut i64"),
        "missing reference call context:\n{out}"
    );
    assert!(
        out.contains("using array_element `xs[3]`"),
        "missing referenced array element context:\n{out}"
    );
}

#[test]
fn test_release_build_omits_runtime_metadata_from_binary() {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_release_runtime_metadata_{}.wi", id));
    let bin_path = temp_path(format!("willow_release_runtime_metadata_{}", id));

    fs::write(&src_path, "fn main() { println(1); }").unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path, "--release"])
        .output()
        .expect("failed to run compiler");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        panic!("release compilation failed: {stderr}");
    }

    let binary = fs::read(&bin_path).expect("release binary should exist");
    let metadata = String::from_utf8_lossy(&binary);

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    assert!(
        !metadata.contains("willow_runtime_metadata_v1"),
        "release binary should not embed runtime metadata"
    );
}
