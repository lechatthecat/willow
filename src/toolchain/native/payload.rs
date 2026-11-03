use super::*;
use object::{Object, ObjectSymbol};

mod comdat;
pub(super) use comdat::Definition;
#[cfg(test)]
pub(super) use comdat::take_symbol_visits;

/// Pinned downloads are raw archives/objects. No tar/zip extraction or host path
/// interpretation is performed. A completed verified file alone enters cache.
pub(super) fn fetch(root: &Path, url: &str, expected: &str, offline: bool) -> Result<PathBuf> {
    fetch_with(root, url, expected, offline, |output| {
        let result = std::process::Command::new("curl")
            .args([
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--proto",
                "=https",
                "--proto-redir",
                "=https",
                "--max-time",
                "120",
                "--",
                url,
            ])
            .stdout(output)
            .stderr(std::process::Stdio::piped())
            .output()
            .context("native_fetch_failed: curl is required for HTTPS artifacts")?;
        ensure!(result.status.success(), "native_fetch_failed: {url}");
        Ok(())
    })
}
pub(super) fn fetch_with(
    root: &Path,
    url: &str,
    expected: &str,
    offline: bool,
    transfer: impl FnOnce(std::fs::File) -> Result<()>,
) -> Result<PathBuf> {
    ensure!(
        url.starts_with("https://"),
        "native_unpinned_source: HTTPS and digest required"
    );
    let mut directory = root.canonicalize()?;
    for part in [".willow", "native", expected] {
        directory.push(part);
        match std::fs::symlink_metadata(&directory) {
            Ok(meta) => ensure!(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "native_unsafe_cache_path"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                ensure!(!offline, "native_cache_missing_offline: {url}");
                std::fs::create_dir(&directory)?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    let path = directory.join("artifact");
    if let Ok(meta) = std::fs::symlink_metadata(&path) {
        ensure!(
            meta.is_file() && !meta.file_type().is_symlink(),
            "native_unsafe_cache_path"
        );
        ensure!(
            digest_file(&path)? == expected,
            "native_digest_mismatch: cached artifact"
        );
        return Ok(path);
    }
    ensure!(!offline, "native_cache_missing_offline: {url}");
    std::fs::create_dir_all(&directory)?;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let temp = directory.join(format!(
        "download-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    struct Remove(PathBuf);
    impl Drop for Remove {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    // create_new avoids overwriting stale files or following a planted symlink.
    let output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    let guard = Remove(temp);
    transfer(output.try_clone()?)?;
    ensure!(
        digest_file(&guard.0)? == expected,
        "native_digest_mismatch: {url}"
    );
    output.sync_all()?;
    drop(output);
    std::fs::rename(&guard.0, &path)?;
    Ok(path)
}

pub(super) fn validate(
    path: &Path,
    arch: &str,
    abi: &str,
    linkage: Linkage,
    mut symbol: impl FnMut(&str, Definition) -> Result<()>,
) -> Result<()> {
    let source = std::fs::File::open(path)?;
    let cache = object::read::ReadCache::new(source.try_clone()?);
    if linkage == Linkage::Static {
        let archive = object::read::archive::ArchiveFile::parse(&cache)
            .context("native_linkage_mismatch: expected static archive")?;
        ensure!(!archive.is_thin(), "native_unsafe_archive: thin archive");
        let mut count = 0;
        for member in archive.members() {
            let member = member?;
            let name = std::str::from_utf8(member.name())?;
            relative(name).context("native_unsafe_archive")?;
            if let Some(mode) = member.mode() {
                let ty = mode & 0o170000;
                ensure!(
                    ty == 0 || ty == 0o100000,
                    "native_unsafe_archive: non-regular member"
                );
            }
            let (offset, size) = member.file_range();
            let member_cache = object::read::ReadCache::new(source.try_clone()?);
            validate_object(
                member_cache.range(offset, size),
                arch,
                abi,
                object::ObjectKind::Relocatable,
                &mut symbol,
            )?;
            count += 1;
        }
        ensure!(count > 0, "native_empty_archive");
    } else {
        ensure!(linkage == Linkage::Shared, "native_linkage_mismatch");
        validate_object(&cache, arch, abi, object::ObjectKind::Dynamic, &mut symbol)?;
    }
    Ok(())
}
fn validate_object<'a>(
    bytes: impl object::read::ReadRef<'a>,
    arch: &str,
    abi: &str,
    kind: object::ObjectKind,
    symbol: &mut impl FnMut(&str, Definition) -> Result<()>,
) -> Result<()> {
    let file = object::File::parse(bytes).context("native_artifact_invalid")?;
    let actual = match file.architecture() {
        object::Architecture::X86_64 => "x86_64",
        object::Architecture::Aarch64 => "aarch64",
        _ => "unsupported",
    };
    // COFF headers use 32-bit offsets even for x86_64/AArch64 machines.
    // The architecture field, not Object::is_64(), establishes their CPU width.
    ensure!(
        actual == arch
            && (file.format() == object::BinaryFormat::Coff || file.is_64())
            && file.is_little_endian(),
        "native_architecture_mismatch: expected {arch}, found {actual}"
    );
    let format = match abi {
        "c-msvc" => {
            if kind == object::ObjectKind::Dynamic {
                object::BinaryFormat::Pe
            } else {
                object::BinaryFormat::Coff
            }
        }
        "c-darwin" => object::BinaryFormat::MachO,
        _ => object::BinaryFormat::Elf,
    };
    ensure!(
        file.format() == format && file.kind() == kind,
        "native_linkage_mismatch: object format/kind"
    );
    if kind == object::ObjectKind::Dynamic {
        for export in file.exports()? {
            if let object::read::NameOrOrdinal::Name(name) = export?.name() {
                symbol(std::str::from_utf8(name)?, Definition::Strong)?;
            }
        }
        return Ok(());
    }
    let definitions = if file.format() == object::BinaryFormat::Coff {
        comdat::sections(&file)?
    } else {
        HashMap::new()
    };
    let entries =
        if kind == object::ObjectKind::Dynamic && file.format() == object::BinaryFormat::Elf {
            file.dynamic_symbols()
        } else {
            file.symbols()
        };
    for entry in entries {
        if entry.is_definition()
            && entry.is_global()
            && !entry.is_weak()
            && let Ok(name) = entry.name()
        {
            let definition = entry
                .section_index()
                .and_then(|section| definitions.get(&section))
                .cloned()
                .unwrap_or(Definition::Strong);
            symbol(name, definition)?;
        }
    }
    Ok(())
}
