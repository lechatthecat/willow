//! Shared process, filesystem, and temporary-project fixtures.

pub(super) use std::fs;
pub(super) use std::path::{Path, PathBuf};
pub(super) use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

pub(super) static COUNTER: AtomicU32 = AtomicU32::new(0);

pub(super) fn unique_test_id() -> String {
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}_{}", std::process::id(), counter)
}

pub(super) fn temp_path(path: impl AsRef<Path>) -> String {
    std::env::temp_dir()
        .join(path)
        .to_string_lossy()
        .into_owned()
}

pub(super) fn remove_output_artifacts(bin_path: &str) {
    let _ = fs::remove_file(bin_path);
    let _ = fs::remove_file(format!("{bin_path}.wsmap"));
}

pub(super) fn contains_path_fragment(haystack: &str, slash_fragment: &str) -> bool {
    haystack.contains(slash_fragment) || haystack.contains(&slash_fragment.replace('/', "\\"))
}

pub(super) fn target_dir() -> std::path::PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("target"))
}

pub(super) fn build_runtime_staticlib(release: bool) -> std::path::PathBuf {
    let mut args = vec!["build", "-p", "willow_runtime"];
    if release {
        args.push("--release");
    }
    let status = Command::new("cargo")
        .args(args)
        .status()
        .expect("failed to build willow_runtime");
    assert!(status.success(), "willow_runtime build failed");
    target_dir()
        .join(if release { "release" } else { "debug" })
        .join(if cfg!(target_env = "msvc") {
            "willow_runtime.lib"
        } else {
            "libwillow_runtime.a"
        })
}

pub(super) fn collect_wi_files(root: &str) -> Vec<String> {
    fn visit(dir: &Path, files: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap_or_else(|err| {
            panic!("failed to read directory {}: {err}", dir.display());
        }) {
            let path = entry.expect("failed to read directory entry").path();
            if path.is_dir() {
                visit(&path, files);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("wi") {
                files.push(path.to_string_lossy().replace('\\', "/"));
            }
        }
    }

    let mut files = Vec::new();
    visit(Path::new(root), &mut files);
    files.sort();
    files
}

pub(super) fn collect_runnable_example_entries() -> Vec<String> {
    collect_wi_files("example")
        .into_iter()
        .filter(|path| !path.contains("/future/"))
        .filter(|path| {
            fs::read_to_string(path)
                .map(|source| source.contains("fn main("))
                .unwrap_or(false)
        })
        .collect()
}

pub(super) fn compile_and_run(source: &str) -> (String, bool) {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_test_{}.wi", id));
    let bin_path = temp_path(format!("willow_test_{}", id));

    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .output()
        .expect("failed to run compiler");

    if !output.status.success() {
        eprintln!(
            "compiler stdout:\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
        eprintln!(
            "compiler stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        return (String::new(), false);
    }

    let out = Command::new(&bin_path)
        .output()
        .expect("failed to run binary");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.success(),
    )
}

/// Like `compile_and_run` but returns `(stdout+stderr, binary_exit_ok)`.
/// Use this when the test needs to observe the binary's exit status (e.g. panic tests).
pub(super) fn compile_and_run_check_exit(source: &str) -> (String, bool) {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_exit_test_{}.wi", id));
    let bin_path = temp_path(format!("willow_exit_test_{}", id));

    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let status = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .stderr(Stdio::null())
        .status()
        .expect("failed to run compiler");

    if !status.success() {
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        return (String::new(), false);
    }

    let out = Command::new(&bin_path)
        .output()
        .expect("failed to run binary");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (combined, out.status.success())
}

/// Like `compile_and_run` but runs the binary with `WILLOW_GC_STRESS=alloc`, so
/// the garbage collector runs on *every* allocation.  This turns latent
/// GC-rooting bugs in generated code (a live value not rooted across an
/// allocation) into deterministic failures instead of rare, load-dependent
/// crashes.  Returns `(stdout+stderr, binary_exit_ok)`.
pub(super) fn compile_and_run_gc_stress(source: &str) -> (String, bool) {
    compile_and_run_gc_stress_mode(source, "alloc")
}

pub(super) fn compile_and_run_gc_stress_all(source: &str) -> (String, bool) {
    compile_and_run_gc_stress_mode(source, "all")
}

pub(super) fn compile_and_run_gc_stress_mode(source: &str, mode: &str) -> (String, bool) {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_gcstress_test_{}.wi", id));
    let bin_path = temp_path(format!("willow_gcstress_test_{}", id));

    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let compiler_output = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .output()
        .expect("failed to run compiler");

    if !compiler_output.status.success() {
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        return (
            format!(
                "{}{}",
                String::from_utf8_lossy(&compiler_output.stdout),
                String::from_utf8_lossy(&compiler_output.stderr)
            ),
            false,
        );
    }

    let out = Command::new(&bin_path)
        .env("WILLOW_GC_STRESS", mode)
        .output()
        .expect("failed to run binary");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (combined, out.status.success())
}

/// Like `compile_and_run` but runs the binary with extra environment variables.
/// Returns `(stdout, binary_exit_ok)`.
pub(super) fn compile_and_run_with_env(source: &str, env: &[(&str, &str)]) -> (String, bool) {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_env_test_{}.wi", id));
    let bin_path = temp_path(format!("willow_env_test_{}", id));

    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let mut compiler_cmd = Command::new(compiler);
    compiler_cmd.args(["build", &src_path, "-o", &bin_path]);
    for (key, value) in env {
        compiler_cmd.env(key, value);
    }
    let status = compiler_cmd
        .stderr(Stdio::null())
        .status()
        .expect("failed to run compiler");

    if !status.success() {
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        return (String::new(), false);
    }

    let mut cmd = Command::new(&bin_path);
    for (key, value) in env {
        cmd.env(key, value);
    }
    let out = cmd.output().expect("failed to run binary");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    (String::from_utf8_lossy(&out.stdout).into_owned(), true)
}

pub(super) fn compile_and_run_with_program_args(
    source: &str,
    program_args: &[&str],
) -> (String, bool) {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_args_test_{}.wi", id));
    let bin_path = temp_path(format!("willow_args_test_{}", id));

    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let status = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .stderr(Stdio::null())
        .status()
        .expect("failed to run compiler");

    if !status.success() {
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        return (String::new(), false);
    }

    let out = Command::new(&bin_path)
        .args(program_args)
        .output()
        .expect("failed to run binary");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.success(),
    )
}

pub(super) fn run_command_with_program_args(source: &str, program_args: &[&str]) -> (String, bool) {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_run_args_test_{}.wi", id));

    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let mut command = Command::new(compiler);
    command.args(["run", &src_path, "--"]);
    command.args(program_args);
    let out = command.output().expect("failed to run compiler");

    let _ = fs::remove_file(&src_path);
    let bin_path = temp_path(format!("willow_run_{}", stem_for_test(&src_path)));
    remove_output_artifacts(&bin_path);

    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.success(),
    )
}

pub(super) fn stem_for_test(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("a")
        .to_string()
}

pub(super) fn compile_file_and_run(src_path: &str) -> (String, bool) {
    compile_file_and_run_with_args(src_path, &[])
}

pub(super) fn compile_file_error_stderr(src_path: &str) -> String {
    let id = unique_test_id();
    let bin_path = temp_path(format!("willow_example_error_test_{}", id));

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let out = Command::new(compiler)
        .args(["build", src_path, "-o", &bin_path])
        .output()
        .expect("failed to run compiler");

    remove_output_artifacts(&bin_path);

    assert!(
        !out.status.success(),
        "expected compile error for {src_path}, got success; stdout: {}; stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    String::from_utf8_lossy(&out.stderr).into_owned()
}

pub(super) fn compile_file_and_run_with_args(
    src_path: &str,
    extra_args: &[&str],
) -> (String, bool) {
    let id = unique_test_id();
    let bin_path = temp_path(format!("willow_example_test_{}", id));

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let mut command = Command::new(compiler);
    command.args(["build", src_path, "-o", &bin_path]);
    command.args(extra_args);
    command.stderr(Stdio::null());
    let status = command.status().expect("failed to run compiler");

    if !status.success() {
        remove_output_artifacts(&bin_path);
        return (String::new(), false);
    }

    let out = Command::new(&bin_path)
        .output()
        .expect("failed to run binary");

    remove_output_artifacts(&bin_path);

    (String::from_utf8_lossy(&out.stdout).into_owned(), true)
}

/// Isolated multi-file Willow project used by module-resolution tests.
///
/// The fixture owns both its source tree and output binary, so all artifacts
/// are removed on every exit path, including assertion panics.
pub(super) struct TestProject {
    root: PathBuf,
    bin_path: PathBuf,
}

impl TestProject {
    pub(super) fn new(prefix: &str, files: &[(&str, &str)]) -> Self {
        let id = unique_test_id();
        let root = std::env::temp_dir().join(format!("willow_{prefix}_{id}"));
        let bin_path = std::env::temp_dir().join(format!("willow_{prefix}_{id}_bin"));

        fs::create_dir_all(&root).expect("failed to create temporary Willow project");
        for (relative_path, source) in files {
            let path = root.join(relative_path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .expect("failed to create temporary Willow project directory");
            }
            fs::write(path, source).expect("failed to write temporary Willow source");
        }

        Self { root, bin_path }
    }

    pub(super) fn compile(&self, entry: &str) -> std::process::Output {
        let src_path = self.root.join(entry);
        Command::new(env!("CARGO_BIN_EXE_willowc"))
            .args(["build", path_str(&src_path), "-o", path_str(&self.bin_path)])
            .output()
            .expect("failed to run compiler")
    }

    pub(super) fn run(&self) -> std::process::Output {
        Command::new(&self.bin_path)
            .output()
            .expect("failed to run binary")
    }
}

impl Drop for TestProject {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
        remove_output_artifacts(path_str(&self.bin_path));
    }
}

fn path_str(path: &Path) -> &str {
    path.to_str()
        .expect("temporary test path must contain valid UTF-8")
}

pub(super) fn compile_temp_project_and_run(files: &[(&str, &str)], entry: &str) -> (String, bool) {
    let project = TestProject::new("project_test", files);
    let output = project.compile(entry);

    if !output.status.success() {
        eprintln!("{}", String::from_utf8_lossy(&output.stderr));
        return (String::new(), false);
    }

    let out = project.run();

    (String::from_utf8_lossy(&out.stdout).into_owned(), true)
}

pub(super) fn compile_temp_project_error_stderr(files: &[(&str, &str)], entry: &str) -> String {
    let project = TestProject::new("project_error_test", files);
    let output = project.compile(entry);

    assert!(
        !output.status.success(),
        "expected compile error, got success; stdout: {}; stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Compile source that is expected to fail; returns true if compiler rejected it.
pub(super) fn expect_compile_error(source: &str) -> bool {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_err_{}.wi", id));
    let bin_path = temp_path(format!("willow_err_{}", id));

    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let status = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .stderr(Stdio::null())
        .status()
        .expect("failed to run compiler");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    !status.success()
}

pub(super) fn compile_error_stderr(source: &str) -> String {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_diag_{}.wi", id));
    let bin_path = temp_path(format!("willow_diag_{}", id));

    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let out = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .output()
        .expect("failed to run compiler");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    assert!(
        !out.status.success(),
        "expected compile error, got success; stdout: {}; stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    String::from_utf8_lossy(&out.stderr).into_owned()
}

pub(super) fn assert_compile_error_contains(source: &str, expected_parts: &[&str]) {
    let stderr = compile_error_stderr(source);
    for part in expected_parts {
        assert!(
            stderr.contains(part),
            "stderr did not contain `{part}`:\n{stderr}"
        );
    }
}

pub(super) fn compile_with_compiler_env(source: &str, env: &[(&str, &str)]) -> (bool, String) {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_drc_{}.wi", id));
    let bin_path = temp_path(format!("willow_drc_{}", id));
    fs::write(&src_path, source).unwrap();
    let compiler = env!("CARGO_BIN_EXE_willowc");
    let mut cmd = Command::new(compiler);
    cmd.args(["build", &src_path, "-o", &bin_path]);
    cmd.env_remove("WILLOW_DATA_RACE_CHECK");
    cmd.env_remove("WILLOW_WORKERS");
    for (key, value) in env {
        cmd.env(key, value);
    }
    let out = cmd.output().expect("failed to run compiler");
    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Compile with the Send/Sync async checks enabled
/// (`WILLOW_DATA_RACE_CHECK=1`), returning `(compiled_ok, stderr)`.
pub(super) fn compile_with_data_race_check(source: &str) -> (bool, String) {
    compile_with_compiler_env(source, &[("WILLOW_DATA_RACE_CHECK", "1")])
}

// ── Basic output ─────────────────────────────────────────────────────────────

/// Compile with extra COMPILER environment variables, then run the binary.
/// Used by the LIR-backend differential tests (willow-0g8j).
pub(super) fn compile_with_env_and_run(source: &str, env: &[(&str, &str)]) -> (String, bool) {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_lirdiff_test_{}.wi", id));
    let bin_path = temp_path(format!("willow_lirdiff_test_{}", id));

    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let mut cmd = Command::new(compiler);
    cmd.args(["build", &src_path, "-o", &bin_path]);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let output = cmd.output().expect("failed to run compiler");

    if !output.status.success() {
        eprintln!(
            "compiler stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        return (String::new(), false);
    }

    let out = Command::new(&bin_path)
        .output()
        .expect("failed to run binary");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.success(),
    )
}
