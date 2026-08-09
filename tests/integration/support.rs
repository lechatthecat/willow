//! Shared process, filesystem, and temporary-project fixtures.

pub(super) use std::fs;
pub(super) use std::path::{Path, PathBuf};
pub(super) use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

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
                .map(|source| source.contains("fn main(") && !source.contains("// test: manual"))
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

/// Compile and run a program with `--release`, for perspectives that must hold
/// with optimizations on (debug-only instrumentation absent).
pub(super) fn compile_and_run_release(source: &str) -> (String, bool) {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_test_{}.wi", id));
    let bin_path = temp_path(format!("willow_test_{}", id));

    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path, "--release"])
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

/// Compile and run a program with a hard runtime deadline.
///
/// Returns `(stdout+stderr, binary_exit_ok, timed_out)`. This is reserved for
/// regressions whose broken behavior can park forever; ordinary tests should
/// continue to use `compile_and_run`.
pub(super) fn compile_and_run_with_env_timeout(
    source: &str,
    env: &[(&str, &str)],
    timeout: Duration,
) -> (String, bool, bool) {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_timeout_test_{id}.wi"));
    let bin_path = temp_path(format!("willow_timeout_test_{id}"));

    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let mut compiler_cmd = Command::new(compiler);
    compiler_cmd.args(["build", &src_path, "-o", &bin_path]);
    for (key, value) in env {
        compiler_cmd.env(key, value);
    }
    let compiled = compiler_cmd.output().expect("failed to run compiler");
    if !compiled.status.success() {
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&compiled.stdout),
            String::from_utf8_lossy(&compiled.stderr)
        );
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        return (combined, false, false);
    }

    let mut command = Command::new(&bin_path);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command.spawn().expect("failed to run binary");
    let deadline = Instant::now() + timeout;
    let timed_out = loop {
        match child.try_wait().expect("failed to poll binary") {
            Some(_) => break false,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                break true;
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    };
    let output = child
        .wait_with_output()
        .expect("failed to collect binary output");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (combined, output.status.success() && !timed_out, timed_out)
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
///
/// Returns `(output, ok)`. `ok` is false when compilation fails OR when the
/// compiled binary exits unsuccessfully; a program that prints the expected
/// stdout and then aborts must not report success (willow-4t7t). On a
/// successful run `output` is stdout only, so snapshot expectations stay
/// stable; on binary failure it is stdout followed by stderr, so the runtime
/// diagnostic is visible in the test failure.
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

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if out.status.success() {
        return (stdout, true);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    (format!("{stdout}{stderr}"), false)
}

/// Compiles `source` and returns every symbol name referenced by a relocation
/// in the emitted *object file* — that is, the symbols the generated machine
/// code actually reaches out to.
///
/// This is the only vantage point from which "the backend emitted a call to X"
/// is observable, and relocations specifically are the signal:
///
///   * the linked executable cannot answer the question at all, because the
///     runtime staticlib contributes its own definition of a symbol such as
///     `willow_pow_f64` once any archive member is pulled in, so a listing of
///     the binary cannot separate "this program calls it" from "it was linked
///     in";
///   * the object's *symbol table* cannot answer it either, because the backend
///     declares the whole runtime ABI as imports up front — every
///     `RUNTIME_SYMBOLS` entry is an undefined symbol in every object we emit,
///     used or not.
///
/// A relocation exists only where an instruction refers to the symbol, so this
/// list is the emitted call graph's external edges. `WILLOW_KEEP_OBJECT=1`
/// keeps the intermediate object alive past linking so it can be parsed here.
///
/// Names are normalised by stripping leading underscores, because Mach-O
/// prefixes every C symbol with `_` while ELF and COFF/x86_64 do not.
pub(super) fn compile_and_collect_relocation_targets(
    source: &str,
    env: &[(&str, &str)],
) -> Vec<String> {
    use object::read::{Object, ObjectSection, ObjectSymbol, RelocationTarget};

    let id = unique_test_id();
    let src_path = temp_path(format!("willow_obj_test_{}.wi", id));
    let bin_path = temp_path(format!("willow_obj_test_{}", id));
    // Mirrors HostToolchain::object_path.
    let obj_path = if cfg!(all(target_os = "windows", target_env = "msvc")) {
        format!("{bin_path}.obj")
    } else {
        format!("{bin_path}.o")
    };

    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let mut command = Command::new(compiler);
    command
        .args(["build", &src_path, "-o", &bin_path])
        .env("WILLOW_KEEP_OBJECT", "1");
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().expect("failed to run compiler");
    assert!(
        output.status.success(),
        "compilation failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let bytes = fs::read(&obj_path)
        .unwrap_or_else(|err| panic!("intermediate object {obj_path} unreadable: {err}"));
    let object = object::File::parse(&*bytes)
        .unwrap_or_else(|err| panic!("intermediate object {obj_path} unparseable: {err}"));

    let mut names = Vec::new();
    for section in object.sections() {
        for (_offset, relocation) in section.relocations() {
            let RelocationTarget::Symbol(index) = relocation.target() else {
                continue;
            };
            let Ok(symbol) = object.symbol_by_index(index) else {
                continue;
            };
            if let Ok(name) = symbol.name() {
                names.push(name.trim_start_matches('_').to_string());
            }
        }
    }
    names.sort();
    names.dedup();
    assert!(
        !names.is_empty(),
        "no relocations found in {obj_path}; the inspection itself is broken"
    );

    let _ = fs::remove_file(&src_path);
    let _ = fs::remove_file(&obj_path);
    remove_output_artifacts(&bin_path);

    names
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

    /// [`Self::compile`] with extra COMPILER environment variables, so a
    /// multi-file project can be built twice under different backend settings.
    pub(super) fn compile_with_env(
        &self,
        entry: &str,
        env: &[(&str, &str)],
    ) -> std::process::Output {
        let src_path = self.root.join(entry);
        let mut command = Command::new(env!("CARGO_BIN_EXE_willowc"));
        command.args(["build", path_str(&src_path), "-o", path_str(&self.bin_path)]);
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().expect("failed to run compiler")
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

/// [`compile_temp_project_and_run`] with compiler environment variables. On a
/// failed build the compiler's stderr comes back as the output string, so an
/// assertion can report why.
pub(super) fn compile_temp_project_with_env_and_run(
    files: &[(&str, &str)],
    entry: &str,
    env: &[(&str, &str)],
) -> (String, bool) {
    let project = TestProject::new("project_env_test", files);
    let output = project.compile_with_env(entry, env);

    if !output.status.success() {
        return (String::from_utf8_lossy(&output.stderr).into_owned(), false);
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
    // The LIR backend switches must come from `env` only, so a test can assert
    // on the default (fallback allowed) regardless of the ambient environment.
    cmd.env_remove("WILLOW_LIR_BACKEND");
    cmd.env_remove("WILLOW_LIR_REQUIRE");
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

/// Compile normally, then run the binary with extra RUNTIME environment
/// variables and a hard timeout: a preemption/scheduler regression must fail
/// the test, not hang CI (willow-0a6k.2 review fix).
pub(super) fn compile_and_run_with_runtime_env(
    source: &str,
    env: &[(&str, &str)],
    timeout: std::time::Duration,
) -> (String, bool) {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_rtenv_test_{}.wi", id));
    let bin_path = temp_path(format!("willow_rtenv_test_{}", id));

    fs::write(&src_path, source).unwrap();
    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .output()
        .expect("failed to run compiler");
    if !output.status.success() {
        eprintln!(
            "compiler stderr:
{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        return (String::new(), false);
    }

    let mut cmd = Command::new(&bin_path);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().expect("failed to run binary");
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait().expect("wait failed") {
            Some(status) => break Some(status),
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    };
    let mut stdout = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        use std::io::Read;
        let _ = pipe.read_to_string(&mut stdout);
    }
    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);
    match status {
        Some(status) => (stdout, status.success()),
        None => (
            format!("TIMEOUT after {timeout:?}; stdout so far: {stdout}"),
            false,
        ),
    }
}

/// Like [`compile_with_env_and_run`], but returns the binary's stdout AND
/// stderr, so a test can assert on a runtime panic message (willow-0g8j.4).
pub(super) fn compile_with_env_and_run_combined(
    source: &str,
    compile_env: &[(&str, &str)],
) -> (String, bool) {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_lircomb_test_{}.wi", id));
    let bin_path = temp_path(format!("willow_lircomb_test_{}", id));

    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let mut cmd = Command::new(compiler);
    cmd.args(["build", &src_path, "-o", &bin_path]);
    for (k, v) in compile_env {
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
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
        out.status.success(),
    )
}

/// Compile with extra COMPILER environment variables, then run the binary with
/// extra RUNTIME ones, reporting the binary's real exit status. Used by the
/// LIR-backend GC-stress differential tests (willow-0g8j.1), which need
/// `WILLOW_LIR_BACKEND` at compile time and `WILLOW_GC_STRESS` at run time.
pub(super) fn compile_with_env_and_run_under(
    source: &str,
    compile_env: &[(&str, &str)],
    run_env: &[(&str, &str)],
) -> (String, bool) {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_lirgc_test_{}.wi", id));
    let bin_path = temp_path(format!("willow_lirgc_test_{}", id));

    fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let mut cmd = Command::new(compiler);
    cmd.args(["build", &src_path, "-o", &bin_path]);
    for (k, v) in compile_env {
        cmd.env(k, v);
    }
    let output = cmd.output().expect("failed to run compiler");
    if !output.status.success() {
        let _ = fs::remove_file(&src_path);
        remove_output_artifacts(&bin_path);
        return (
            format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ),
            false,
        );
    }

    let mut run = Command::new(&bin_path);
    for (k, v) in run_env {
        run.env(k, v);
    }
    let out = run.output().expect("failed to run binary");

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);

    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.success(),
    )
}

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

/// Contract tests for the environment-based runner itself (willow-4t7t).
///
/// `compile_and_run_with_env` used to return `true` unconditionally once
/// compilation succeeded, so a program that printed the expected stdout and
/// then aborted was reported as a passing test. These tests pin the runner's
/// own behavior rather than a language feature; each one compiles a real
/// binary, so related perspectives are grouped instead of split one per test.
///
/// Perspectives covered here:
///   1. a normally exiting binary reports ok
///   2. a successful run returns stdout only
///   3. runtime environment variables really reach the compiled binary
///   4. runtime stderr chatter from a successful run is not mixed into stdout
///   5. multi-line stdout keeps its order and trailing newline
///   6. an aborting binary reports NOT ok
///   7. an aborting binary still returns the stdout printed before the abort
///   8. an aborting binary appends the runtime stderr diagnostic
///   9. the panic message text itself survives into the returned output
///  10. an abort inside an async task is reported the same way
///  11. a failing compile reports NOT ok
///  12. a failing compile returns empty output (unchanged legacy contract)
///  13. compile failure and binary failure stay distinguishable to callers
///  14. the empty environment slice is accepted
///  15. repeated invocations are independent (no leaked state between runs)
mod env_runner_contract {
    use super::*;

    /// Perspectives 1, 2, 3, 4, 5.
    #[test]
    fn env_runner_reports_binary_success_with_stdout_only() {
        let source = r#"
fn main() {
    println("first");
    println("second");
}
"#;
        // WILLOW_GC_LOG makes the runtime write `[gc]` lines to stderr, so a
        // successful run proves both that the environment reached the binary
        // and that stderr is kept out of the returned stdout.
        let (out, ok) = compile_and_run_with_env(
            source,
            &[("WILLOW_GC_LOG", "1"), ("WILLOW_GC_STRESS", "alloc")],
        );
        assert!(ok, "a normally exiting binary must report success: {out}");
        assert_eq!(out, "first\nsecond\n", "successful runs stay stdout-only");
    }

    /// Perspectives 6, 7, 8, 9.
    #[test]
    fn env_runner_reports_binary_failure() {
        let source = r#"
fn main() {
    println("before");
    panic("boom");
}
"#;
        let (out, ok) = compile_and_run_with_env(source, &[("WILLOW_WORKERS", "2")]);
        assert!(
            !ok,
            "a binary that aborts must not be reported as success: {out}"
        );
        assert!(
            out.starts_with("before\n"),
            "stdout printed before the abort must survive: {out:?}"
        );
        assert!(
            out.contains("boom"),
            "the runtime diagnostic must reach the caller: {out:?}"
        );
    }

    /// Perspective 10.
    #[test]
    fn env_runner_reports_async_binary_failure() {
        let source = r#"
async fn work() -> i64 {
    await sleep(1);
    panic("async boom");
    return 1;
}

async fn main() {
    println("started");
    let value = await work();
    println(value);
}
"#;
        let (out, ok) = compile_and_run_with_env(source, &[("WILLOW_WORKERS", "2")]);
        assert!(!ok, "an abort inside a task must fail the run too: {out}");
        assert!(
            out.contains("async boom"),
            "the task panic message must reach the caller: {out:?}"
        );
    }

    /// Perspectives 11, 12, 13, 14, 15.
    #[test]
    fn env_runner_reports_compile_failure_distinctly() {
        let source = r#"
fn main() {
    let x: i64 = true;
}
"#;
        let (out, ok) = compile_and_run_with_env(source, &[]);
        assert!(!ok, "a program that does not compile must report failure");
        assert_eq!(
            out, "",
            "compile failure keeps its empty-output contract, so a caller can \
             tell it apart from a binary that failed after printing"
        );

        let (second_out, second_ok) = compile_and_run_with_env(
            r#"
fn main() {
    println("ok");
}
"#,
            &[],
        );
        assert!(
            second_ok,
            "a later run must not inherit the earlier failure"
        );
        assert_eq!(second_out, "ok\n");
    }
}
