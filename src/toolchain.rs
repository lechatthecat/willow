//! Host object-file, runtime-library, linker, and sidecar artifact handling.

pub mod native;
pub mod rust_bridge;

use std::path::{Path, PathBuf};
#[cfg(any(not(feature = "packaged"), not(all(windows, target_env = "msvc"))))]
use std::process::Command;
use std::process::ExitStatus;

use anyhow::{Context, Result};

#[cfg(not(feature = "packaged"))]
use crate::BuildMode;
use crate::TargetOptions;

/// Platform boundary used by the compiler after native object generation.
pub trait Toolchain {
    fn write_object(&self, output: &str, bytes: &[u8]) -> Result<PathBuf>;
    fn resolve_runtime_library(&self) -> Result<PathBuf>;
    fn link(&self, object: &Path, runtime: &Path, output: &str) -> Result<ExitStatus>;
    fn update_source_map(&self, output: &str, contents: Option<&str>) -> Result<()>;
}

/// Toolchain for the host Rust target.
pub struct HostToolchain {
    target: TargetOptions,
    #[cfg(not(feature = "packaged"))]
    runtime_lock: std::cell::RefCell<Option<std::fs::File>>,
    #[cfg(not(feature = "packaged"))]
    runtime_snapshot: std::cell::RefCell<Option<RuntimeSnapshot>>,
}

/// Process-private hard link to the runtime archive, removed on drop. On Unix,
/// Cargo replaces the shared alias by unlinking it, which leaves this link's
/// inode intact, so a linker can read it without holding the runtime lock.
/// Windows share modes span hard links: a linker holding this file open could
/// make Cargo's unlink of the alias fail, so it is not used there.
#[cfg(not(feature = "packaged"))]
struct RuntimeSnapshot {
    /// The shared alias this link was taken from, for messages that outlive it.
    source: PathBuf,
    path: PathBuf,
    /// Locked for this snapshot's lifetime. The OS releases it even after
    /// SIGKILL, which is how a later compiler recognizes an abandoned link.
    owner: PathBuf,
    _owner_lock: std::fs::File,
}

#[cfg(not(feature = "packaged"))]
impl RuntimeSnapshot {
    const OWNER_SUFFIX: &str = ".owner";

    /// The caller must hold the runtime lock: it orders creation against the
    /// sweep, so a new owner file is never observed before it is locked.
    fn create(runtime: &Path) -> std::io::Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);

        let directory = runtime
            .parent()
            .expect("runtime profile directory")
            .join(".willow-runtime-links");
        std::fs::create_dir_all(&directory)?;
        Self::remove_abandoned(&directory)?;
        let name = runtime.file_name().expect("runtime library name");
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let mut file_name = std::ffi::OsString::from(format!("{}-{unique}-", std::process::id()));
        file_name.push(name);
        let path = directory.join(file_name);
        let owner = Self::owner_path(&path);
        let owner_lock = std::fs::File::create(&owner)?;
        owner_lock.lock()?;
        if let Err(error) = std::fs::hard_link(runtime, &path) {
            let _ = std::fs::remove_file(&owner);
            return Err(error);
        }
        Ok(Self {
            source: runtime.to_path_buf(),
            path,
            owner,
            _owner_lock: owner_lock,
        })
    }

    fn owner_path(path: &Path) -> PathBuf {
        let mut owner = path.as_os_str().to_owned();
        owner.push(Self::OWNER_SUFFIX);
        PathBuf::from(owner)
    }

    /// Each abandoned link can pin a whole superseded archive on disk.
    fn remove_abandoned(directory: &Path) -> std::io::Result<()> {
        for entry in std::fs::read_dir(directory)? {
            let owner = entry?.path();
            let Some(link) = owner
                .to_str()
                .and_then(|path| path.strip_suffix(Self::OWNER_SUFFIX))
            else {
                continue;
            };
            let Ok(file) = std::fs::File::open(&owner) else {
                continue;
            };
            if file.try_lock().is_ok() {
                let _ = std::fs::remove_file(link);
                let _ = std::fs::remove_file(&owner);
            }
        }
        Ok(())
    }
}

#[cfg(not(feature = "packaged"))]
impl Drop for RuntimeSnapshot {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(&self.owner);
    }
}

impl HostToolchain {
    pub fn new(target: &TargetOptions) -> Self {
        Self {
            target: target.clone(),
            #[cfg(not(feature = "packaged"))]
            runtime_lock: std::cell::RefCell::new(None),
            #[cfg(not(feature = "packaged"))]
            runtime_snapshot: std::cell::RefCell::new(None),
        }
    }

    /// The stable archive behind `resolved`, for messages that outlive this
    /// toolchain: a snapshot path disappears when the toolchain is dropped.
    pub fn runtime_library_origin(&self, resolved: &Path) -> PathBuf {
        #[cfg(not(feature = "packaged"))]
        if let Some(snapshot) = self.runtime_snapshot.borrow().as_ref() {
            return snapshot.source.clone();
        }
        resolved.to_path_buf()
    }

    fn object_path(&self, output: &str) -> PathBuf {
        if cfg!(all(windows, target_env = "msvc")) {
            PathBuf::from(format!("{output}.obj"))
        } else {
            PathBuf::from(format!("{output}.o"))
        }
    }

    #[cfg(not(feature = "packaged"))]
    fn default_runtime_library_path(&self) -> PathBuf {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let target_dir = self
            .target
            .cargo_target_dir
            .clone()
            .unwrap_or_else(|| manifest_dir.join("target"));
        let profile = if self.target.build_mode == BuildMode::Release {
            "release"
        } else {
            "debug"
        };
        target_dir.join(profile).join(if cfg!(target_env = "msvc") {
            "willow_runtime.lib"
        } else {
            "libwillow_runtime.a"
        })
    }

    /// Read-only availability probe: doctor must not build or link anything.
    pub fn runtime_library_status(&self) -> serde_json::Value {
        let (path, available) = if let Some(path) = &self.target.runtime_lib {
            (Some(path.clone()), path.is_file())
        } else {
            let path = std::env::current_exe()
                .ok()
                .and_then(|exe| bundled_runtime_path(&exe, cfg!(target_env = "msvc")));
            let available = path.as_ref().is_some_and(|path| path.is_file());
            #[cfg(not(feature = "packaged"))]
            let (path, available) = if available {
                (path, available)
            } else {
                let path = self.default_runtime_library_path();
                let available = path.is_file();
                (Some(path), available)
            };
            (path, available)
        };
        serde_json::json!({"required":true, "available":available, "path":path})
    }

    pub fn runtime_library_help(&self) -> &'static str {
        if cfg!(feature = "packaged") {
            "restore lib/<runtime> beside bin/willow from the release archive, or pass --runtime-lib / WILLOW_RUNTIME_LIB"
        } else {
            "place the bundled runtime in ../lib relative to willow, build willow_runtime with Cargo, or pass --runtime-lib / WILLOW_RUNTIME_LIB"
        }
    }

    pub fn native_linker_status(&self) -> serde_json::Value {
        #[cfg(all(windows, target_env = "msvc"))]
        {
            let target = target_lexicon::Triple::host().to_string();
            let tool = cc::windows_registry::find_tool(&target, "cl.exe");
            serde_json::json!({"required":true, "available":tool.is_some(),
                "program":tool.map(|t| t.path().to_owned())})
        }
        #[cfg(not(all(windows, target_env = "msvc")))]
        {
            let available = Command::new("cc")
                .arg("--version")
                .output()
                .is_ok_and(|o| o.status.success());
            serde_json::json!({"required":true, "available":available, "program":"cc"})
        }
    }

    #[cfg(not(feature = "packaged"))]
    fn build_default_runtime_library(&self) -> Result<()> {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut args = vec!["build", "-p", "willow_runtime"];
        if self.target.build_mode == BuildMode::Release {
            args.push("--release");
        }

        let mut command = Command::new("cargo");
        command.args(args).current_dir(&manifest_dir);
        if let Some(target_dir) = &self.target.cargo_target_dir {
            // Cargo runs from the compiler workspace, while relative output
            // paths belong to the invoking process. Preserve that location.
            command.env(
                "CARGO_TARGET_DIR",
                if target_dir.is_absolute() {
                    target_dir.clone()
                } else {
                    std::env::current_dir()?.join(target_dir)
                },
            );
        }
        let status = command
            .stdout(std::process::Stdio::from(std::io::stderr()))
            .status()
            .with_context(|| "failed to run cargo to build willow_runtime")?;
        if status.success() {
            Ok(())
        } else {
            anyhow::bail!("cargo failed to build willow_runtime")
        }
    }
}

impl Toolchain for HostToolchain {
    fn write_object(&self, output: &str, bytes: &[u8]) -> Result<PathBuf> {
        let path = self.object_path(output);
        std::fs::write(&path, bytes)?;
        Ok(path)
    }

    fn resolve_runtime_library(&self) -> Result<PathBuf> {
        if let Some(path) = &self.target.runtime_lib {
            return validate_runtime_library(path);
        }
        // Relocatable installs keep bin/willow beside lib/<runtime>. Do not
        // touch the build tree or invoke Cargo when the bundled library exists.
        let bundled = std::env::current_exe()
            .ok()
            .and_then(|exe| bundled_runtime_path(&exe, cfg!(target_env = "msvc")));
        if let Some(path) = bundled.as_ref().filter(|path| path.is_file()) {
            return Ok(path.clone());
        }
        #[cfg(feature = "packaged")]
        anyhow::bail!(
            "searched bundled runtime {}; WILLOW_RUNTIME_LIB / --runtime-lib not set",
            bundled.map_or_else(
                || "../lib (executable location unavailable)".to_owned(),
                |path| path.display().to_string()
            )
        );
        #[cfg(not(feature = "packaged"))]
        {
            if let Some(snapshot) = self.runtime_snapshot.borrow().as_ref() {
                return Ok(snapshot.path.clone());
            }
            let path = self.default_runtime_library_path();
            // Cargo may replace the un-hashed staticlib alias even for a fresh
            // build, so only one compiler may run it at a time. On Unix, linking
            // reads a private hard link instead of the alias, which lets
            // concurrent compilers link in parallel. Elsewhere, or without hard
            // links, the lock stays held through linking so the alias cannot
            // be replaced while the linker has it open.
            if self.runtime_lock.borrow().is_none() {
                let directory = path.parent().expect("runtime profile directory");
                std::fs::create_dir_all(directory)?;
                let lock = std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true)
                    .open(directory.join(".willow-runtime.lock"))?;
                lock.lock().context("failed to lock the runtime library")?;
                *self.runtime_lock.borrow_mut() = Some(lock);
            }
            self.build_default_runtime_library()?;
            let path = validate_runtime_library(path)?;
            if !cfg!(unix) {
                return Ok(path);
            }
            let Ok(snapshot) = RuntimeSnapshot::create(&path) else {
                return Ok(path);
            };
            let linked = snapshot.path.clone();
            *self.runtime_snapshot.borrow_mut() = Some(snapshot);
            self.runtime_lock.borrow_mut().take();
            Ok(linked)
        }
    }

    fn link(&self, object: &Path, runtime: &Path, output: &str) -> Result<ExitStatus> {
        self.link_native(object, runtime, output, None, &[])
    }

    fn update_source_map(&self, output: &str, contents: Option<&str>) -> Result<()> {
        let path = source_map_path(output);
        if let Some(contents) = contents {
            std::fs::write(path, contents)?;
        } else {
            let _ = std::fs::remove_file(path);
        }
        Ok(())
    }
}

fn bundled_runtime_path(executable: &Path, msvc: bool) -> Option<PathBuf> {
    Some(executable.parent()?.parent()?.join("lib").join(if msvc {
        "willow_runtime.lib"
    } else {
        "libwillow_runtime.a"
    }))
}

/// Keep unsupported linker families unchanged rather than assuming GNU flags.
fn dead_strip_args(os: &str, environment: &str) -> &'static [&'static str] {
    match (os, environment) {
        ("windows", "msvc") => &["/OPT:REF", "/OPT:ICF"],
        ("macos" | "ios", _) => &["-Wl,-dead_strip"],
        ("linux", _) => &["-Wl,--gc-sections"],
        _ => &[],
    }
}

/// Debuggers discover this exported blob by symbol; generated code never
/// references it. Make it a link root whenever codegen emits the metadata.
fn retain_debug_metadata_args(os: &str, environment: &str) -> &'static [&'static str] {
    match (os, environment) {
        ("windows", "msvc") => &["/INCLUDE:willow_runtime_metadata_v1"],
        ("macos" | "ios", _) => &["-Wl,-u,_willow_runtime_metadata_v1"],
        ("linux", _) => &["-Wl,--undefined=willow_runtime_metadata_v1"],
        _ => &[],
    }
}

fn validate_runtime_library(path: impl Into<PathBuf>) -> Result<PathBuf> {
    let path = path.into();
    if path.is_file() {
        Ok(path)
    } else {
        anyhow::bail!("{} does not exist or is not a file", path.display())
    }
}

fn source_map_path(output: &str) -> PathBuf {
    PathBuf::from(format!("{output}.wsmap"))
}

impl HostToolchain {
    pub fn link_rust_bridge(
        &self,
        object: &Path,
        runtime: &Path,
        output: &str,
        bridge: &rust_bridge::BridgeBuild,
    ) -> Result<ExitStatus> {
        bridge.ensure_target(&target_lexicon::Triple::host().to_string())?;
        self.link_native(object, runtime, output, Some(bridge), &[])
            .context("rust_bridge_link_error")
    }
    pub fn link_native(
        &self,
        object: &Path,
        runtime: &Path,
        output: &str,
        bridge: Option<&rust_bridge::BridgeBuild>,
        native_args: &[String],
    ) -> Result<ExitStatus> {
        if let Some(bridge) = bridge {
            bridge.ensure_target(&target_lexicon::HOST.to_string())?;
        }
        #[cfg(all(windows, target_env = "msvc"))]
        {
            let target = if cfg!(target_arch = "x86_64") {
                "x86_64-pc-windows-msvc"
            } else if cfg!(target_arch = "aarch64") {
                "aarch64-pc-windows-msvc"
            } else if cfg!(target_arch = "x86") {
                "i686-pc-windows-msvc"
            } else {
                anyhow::bail!("unsupported Windows MSVC target architecture");
            };
            let mut command = cc::windows_registry::find_tool(target, "cl.exe")
                .ok_or_else(|| anyhow::anyhow!("failed to find MSVC cl.exe"))?
                .to_command();
            command.arg("/nologo").arg(object);
            if let Some(bridge) = bridge {
                command.arg(
                    bridge
                        .staticlib
                        .as_ref()
                        .context("rust_bridge_link_error: missing archive")?,
                );
            }
            command
                .arg(runtime)
                .arg("/link")
                .args(native_args)
                .args(dead_strip_args("windows", "msvc"))
                .arg(format!("/OUT:{output}"))
                .arg("/SUBSYSTEM:CONSOLE")
                .arg("legacy_stdio_definitions.lib")
                .arg("kernel32.lib")
                .arg("ntdll.lib")
                .arg("userenv.lib")
                .arg("ws2_32.lib")
                .arg("dbghelp.lib")
                .arg("psapi.lib")
                .arg("/defaultlib:msvcrt");
            if let Some(bridge) = bridge {
                command.args(split_native_libs(
                    bridge
                        .messages
                        .native_static_libs
                        .as_deref()
                        .context("rust_bridge_link_error: missing system libraries")?,
                )?);
            }
            if self.target.emit_debug_info {
                command.args(retain_debug_metadata_args("windows", "msvc"));
            }
            command
                .stdout(std::process::Stdio::from(std::io::stderr()))
                .status()
                .with_context(|| "failed to run MSVC compiler driver")
        }

        #[cfg(not(all(windows, target_env = "msvc")))]
        {
            let mut command = Command::new("cc");
            command.arg(object);
            if let Some(bridge) = bridge {
                command.arg(
                    bridge
                        .staticlib
                        .as_ref()
                        .context("rust_bridge_link_error: missing archive")?,
                );
            }
            command.args(native_args);
            command.arg(runtime).arg("-o").arg(output);
            command.args(dead_strip_args(std::env::consts::OS, ""));
            if let Some(bridge) = bridge {
                command.args(split_native_libs(
                    bridge
                        .messages
                        .native_static_libs
                        .as_deref()
                        .context("rust_bridge_link_error: missing system libraries")?,
                )?);
            }
            if self.target.emit_debug_info {
                command.args(retain_debug_metadata_args(std::env::consts::OS, ""));
            }
            // Apple targets emit PIC and use the platform default PIE link.
            if !cfg!(target_vendor = "apple") {
                command.arg("-no-pie");
            }
            command.arg("-lm").arg("-lpthread");
            // Rust's Apple staticlib reports libiconv as a native dependency.
            // libSystem/libc are supplied automatically by the clang driver.
            if cfg!(any(target_os = "macos", target_os = "ios")) {
                command.arg("-liconv");
            }
            // dlopen/dlsym live in libSystem on Apple platforms, which do not
            // ship a separate libdl.
            if !cfg!(any(target_os = "macos", target_os = "ios")) {
                command.arg("-ldl");
            }
            if self.target.strip_symbols {
                command.arg("-s");
            }
            command
                .stdout(std::process::Stdio::from(std::io::stderr()))
                .status()
                .with_context(|| "failed to run linker")
        }
    }
}

fn split_native_libs(text: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    for ch in text.chars() {
        if Some(ch) == quote {
            quote = None;
        } else if quote.is_some() {
            word.push(ch);
        } else if ch == '\"' || ch == '\'' {
            quote = Some(ch);
        } else if ch.is_whitespace() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push(ch);
        }
    }
    anyhow::ensure!(
        quote.is_none(),
        "rust_bridge_link_error: unmatched native library quote"
    );
    if !word.is_empty() {
        words.push(word);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    #[test]
    fn native_libraries_keep_order_duplicates_and_quoted_arguments() {
        for text in [
            "-lgcc_s -lutil -lrt -lpthread -lm -ldl -lc",
            "-lSystem -liconv -lSystem",
            "kernel32.lib ws2_32.lib kernel32.lib",
        ] {
            assert_eq!(
                super::split_native_libs(text).unwrap(),
                text.split_whitespace().collect::<Vec<_>>()
            );
        }
        assert_eq!(
            super::split_native_libs("\"C:\\Program Files\\native.lib\" -framework 'Some Kit'")
                .unwrap(),
            ["C:\\Program Files\\native.lib", "-framework", "Some Kit"]
        );
        assert!(super::split_native_libs("'broken").is_err());
    }
    use super::*;

    #[test]
    fn dead_strip_flags_match_native_linker_family() {
        assert_eq!(dead_strip_args("linux", "gnu"), &["-Wl,--gc-sections"]);
        assert_eq!(dead_strip_args("linux", "musl"), &["-Wl,--gc-sections"]);
        assert_eq!(dead_strip_args("macos", ""), &["-Wl,-dead_strip"]);
        assert_eq!(dead_strip_args("ios", ""), &["-Wl,-dead_strip"]);
        assert_eq!(
            dead_strip_args("windows", "msvc"),
            &["/OPT:REF", "/OPT:ICF"]
        );
        assert!(dead_strip_args("windows", "gnu").is_empty());
        assert!(dead_strip_args("unknown", "").is_empty());
        assert_eq!(
            retain_debug_metadata_args("linux", "gnu"),
            &["-Wl,--undefined=willow_runtime_metadata_v1"]
        );
        assert_eq!(
            retain_debug_metadata_args("macos", ""),
            &["-Wl,-u,_willow_runtime_metadata_v1"]
        );
        assert_eq!(
            retain_debug_metadata_args("windows", "msvc"),
            &["/INCLUDE:willow_runtime_metadata_v1"]
        );
    }

    #[test]
    fn object_extension_matches_host_abi() {
        let toolchain = HostToolchain::new(&crate::CompilerOptions::debug().target);
        let path = toolchain.object_path("program");
        if cfg!(all(windows, target_env = "msvc")) {
            assert_eq!(path, PathBuf::from("program.obj"));
        } else {
            assert_eq!(path, PathBuf::from("program.o"));
        }
    }

    #[test]
    #[cfg(not(feature = "packaged"))]
    fn runtime_library_path_uses_profile_and_platform_name() {
        let mut target = crate::CompilerOptions::release().target;
        target.cargo_target_dir = Some(PathBuf::from("custom-target"));
        let path = HostToolchain::new(&target).default_runtime_library_path();
        assert!(path.starts_with("custom-target"));
        assert!(path.components().any(|part| part.as_os_str() == "release"));
        assert_eq!(
            path.file_name().unwrap(),
            if cfg!(target_env = "msvc") {
                "willow_runtime.lib"
            } else {
                "libwillow_runtime.a"
            }
        );
    }

    #[test]
    fn bundled_layouts_use_native_archive_names() {
        for (root, binary, msvc, archive) in [
            ("linux", "willow", false, "libwillow_runtime.a"),
            ("macos-arm64", "willow", false, "libwillow_runtime.a"),
            ("macos-x86_64", "willow", false, "libwillow_runtime.a"),
            ("windows", "willow.exe", true, "willow_runtime.lib"),
        ] {
            let root = PathBuf::from(root);
            assert_eq!(
                bundled_runtime_path(&root.join("bin").join(binary), msvc),
                Some(root.join("lib").join(archive))
            );
        }
        assert_eq!(bundled_runtime_path(Path::new("willow"), false), None);
        for depth in [1, 8, 64, 256] {
            let mut root = PathBuf::from("root with spaces");
            for _ in 0..depth {
                root.push("nested");
            }
            assert_eq!(
                bundled_runtime_path(&root.join("bin/willow"), false),
                Some(root.join("lib/libwillow_runtime.a"))
            );
        }
    }

    #[test]
    fn runtime_help_matches_distribution_mode() {
        let host = HostToolchain::new(&crate::CompilerOptions::debug().target);
        assert_eq!(
            host.runtime_library_help().contains("Cargo"),
            !cfg!(feature = "packaged")
        );
        assert!(host.runtime_library_help().contains("WILLOW_RUNTIME_LIB"));
        assert!(host.runtime_library_help().contains("--runtime-lib"));
    }

    #[cfg(all(unix, not(feature = "packaged")))]
    fn snapshot_fixture(name: &str) -> (PathBuf, PathBuf) {
        let directory = std::env::temp_dir().join(format!("willow_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let alias = directory.join("libwillow_runtime.a");
        std::fs::write(&alias, b"first").unwrap();
        (directory, alias)
    }

    #[test]
    #[cfg(all(unix, not(feature = "packaged")))]
    fn runtime_snapshot_survives_alias_replacement_and_is_removed_on_drop() {
        let (directory, alias) = snapshot_fixture("snapshot_replace");

        let first = RuntimeSnapshot::create(&alias).unwrap();
        let second = RuntimeSnapshot::create(&alias).unwrap();
        assert_ne!(first.path, second.path);
        assert_eq!(
            first.path.extension(),
            alias.extension(),
            "linkers select the archive handling from the extension"
        );

        // Cargo replaces the alias by unlinking it and creating a new file.
        std::fs::remove_file(&alias).unwrap();
        std::fs::write(&alias, b"second").unwrap();
        assert_eq!(std::fs::read(&first.path).unwrap(), b"first");

        let (path, owner) = (first.path.clone(), first.owner.clone());
        drop(first);
        assert!(!path.exists());
        assert!(!owner.exists());
        assert_eq!(std::fs::read(&second.path).unwrap(), b"first");
        drop(second);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    #[cfg(all(unix, not(feature = "packaged")))]
    fn abandoned_runtime_links_are_removed_but_live_ones_are_kept() {
        let (directory, alias) = snapshot_fixture("snapshot_sweep");
        let live = RuntimeSnapshot::create(&alias).unwrap();

        // An unlocked owner file is what a killed compiler leaves behind.
        let links = live.path.parent().unwrap().to_path_buf();
        let abandoned = links.join("1-0-libwillow_runtime.a");
        std::fs::hard_link(&alias, &abandoned).unwrap();
        let abandoned_owner = RuntimeSnapshot::owner_path(&abandoned);
        std::fs::write(&abandoned_owner, b"").unwrap();

        let next = RuntimeSnapshot::create(&alias).unwrap();
        assert!(!abandoned.exists());
        assert!(!abandoned_owner.exists());
        assert!(live.path.exists());
        assert!(live.owner.exists());
        drop((live, next));
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    #[cfg(all(unix, not(feature = "packaged")))]
    fn runtime_origin_names_the_alias_rather_than_the_snapshot() {
        let (directory, alias) = snapshot_fixture("snapshot_origin");
        let host = HostToolchain::new(&crate::CompilerOptions::debug().target);
        let unrelated = Path::new("explicit-runtime");
        assert_eq!(host.runtime_library_origin(unrelated), unrelated);

        let snapshot = RuntimeSnapshot::create(&alias).unwrap();
        let resolved = snapshot.path.clone();
        *host.runtime_snapshot.borrow_mut() = Some(snapshot);
        assert_eq!(host.runtime_library_origin(&resolved), alias);
        drop(host);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn runtime_probe_honors_explicit_path_without_building() {
        let mut target = crate::CompilerOptions::debug().target;
        target.runtime_lib = Some(PathBuf::from("absent-explicit-runtime"));
        let host = HostToolchain::new(&target);
        let status = host.runtime_library_status();
        assert_eq!(status["path"], "absent-explicit-runtime");
        assert_eq!(status["available"], false);
        assert_eq!(status["required"], true);
        assert!(
            host.resolve_runtime_library()
                .unwrap_err()
                .to_string()
                .contains("absent-explicit-runtime")
        );
    }

    #[test]
    #[cfg(feature = "packaged")]
    fn packaged_probe_never_selects_source_runtime() {
        let host = HostToolchain::new(&crate::CompilerOptions::debug().target);
        let expected =
            bundled_runtime_path(&std::env::current_exe().unwrap(), cfg!(target_env = "msvc"));
        let status = host.runtime_library_status();
        assert_eq!(status["path"], serde_json::json!(expected));
        assert_eq!(
            status["available"],
            expected.is_some_and(|path| path.is_file())
        );
    }

    #[test]
    fn source_map_update_writes_and_removes_sidecar() {
        let output =
            std::env::temp_dir().join(format!("willow_toolchain_test_{}", std::process::id()));
        let output = output.to_string_lossy();
        let toolchain = HostToolchain::new(&crate::CompilerOptions::debug().target);
        toolchain
            .update_source_map(&output, Some("metadata"))
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(source_map_path(&output)).unwrap(),
            "metadata"
        );
        toolchain.update_source_map(&output, None).unwrap();
        assert!(!source_map_path(&output).exists());
    }
}
