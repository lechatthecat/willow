use super::*;
use std::fs;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "willow-native-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn library(&self, name: &str, symbol: &str) -> Library {
        let dir = self.0.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("lib.a"),
            archive(
                "entry.o",
                &object_bytes(symbol, object::Architecture::X86_64),
                0o100644,
            ),
        )
        .unwrap();
        Library {
            name: name.into(),
            version: "1".into(),
            license: "MIT".into(),
            provenance: "test fixture".into(),
            dependencies: vec![],
            targets: BTreeMap::from([(
                TARGET.into(),
                Artifact {
                    source: Source::Local { path: name.into() },
                    digest: digest_tree(&dir).unwrap(),
                    architecture: "x86_64".into(),
                    abi: "c-sysv".into(),
                    abi_version: "1".into(),
                    linkage: Linkage::Static,
                    loader: None,
                    inputs: vec![LinkInput::File("lib.a".into())],
                    header_roots: vec![],
                    adapter_roots: vec![],
                },
            )]),
        }
    }
    fn manifest(&self) -> NativeManifest {
        NativeManifest {
            roots: vec!["a".into()],
            reproducible: true,
            libraries: BTreeMap::from([("a".into(), self.library("a", "answer"))]),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
const TARGET: &str = "x86_64-unknown-linux-gnu";
fn object_bytes(name: &str, arch: object::Architecture) -> Vec<u8> {
    object_bytes_for(name, arch, object::BinaryFormat::Elf)
}
fn object_bytes_for(
    name: &str,
    arch: object::Architecture,
    format: object::BinaryFormat,
) -> Vec<u8> {
    use object::write::{Object, Symbol, SymbolSection};
    let mut obj = Object::new(format, arch, object::Endianness::Little);
    let section = obj.section_id(object::write::StandardSection::Text);
    obj.append_section_data(section, &[0xc3], 1);
    obj.add_symbol(Symbol {
        name: name.as_bytes().to_vec(),
        value: 0,
        size: 1,
        kind: object::SymbolKind::Text,
        scope: object::SymbolScope::Linkage,
        weak: false,
        section: SymbolSection::Section(section),
        flags: object::SymbolFlags::None,
    });
    obj.write().unwrap()
}
fn archive(name: &str, data: &[u8], mode: u32) -> Vec<u8> {
    let mut result = format!(
        "!<arch>\n{:<16}{:<12}{:<6}{:<6}{:<8o}{:<10}`\n",
        format!("{name}/"),
        0,
        0,
        0,
        mode,
        data.len()
    )
    .into_bytes();
    result.extend(data);
    if !data.len().is_multiple_of(2) {
        result.push(b'\n');
    }
    result
}
fn a(m: &mut NativeManifest) -> &mut Artifact {
    m.libraries
        .get_mut("a")
        .unwrap()
        .targets
        .get_mut(TARGET)
        .unwrap()
}
fn error(f: &Fixture, m: &NativeManifest, expected: &str) {
    let e = format!("{:#}", resolve(&f.0, m, TARGET, true).unwrap_err());
    assert!(e.contains(expected), "{e}");
}

#[test]
fn native_local_identity_and_duplicate_link_arguments() {
    let f = Fixture::new();
    let mut m = f.manifest();
    a(&mut m).inputs = vec![
        LinkInput::Search(".".into()),
        LinkInput::File("lib.a".into()),
        LinkInput::File("lib.a".into()),
    ];
    let p = resolve(&f.0, &m, TARGET, true).unwrap();
    assert_eq!(p.args[1], p.args[2]);
    assert_eq!(p.work.inputs, 3);
    assert_eq!(p.lock.target, TARGET);
    let g = Fixture::new();
    fs::create_dir(g.0.join("a")).unwrap();
    fs::copy(f.0.join("a/lib.a"), g.0.join("a/lib.a")).unwrap();
    assert_eq!(p.lock, resolve(&g.0, &m, TARGET, true).unwrap().lock);
}
#[test]
fn native_digest_changes_with_content_and_relative_names() {
    let f = Fixture::new();
    let m = f.manifest();
    fs::write(f.0.join("a/header.h"), "one").unwrap();
    error(&f, &m, "native_digest_mismatch");
    let before = digest_tree(&f.0.join("a")).unwrap();
    fs::rename(f.0.join("a/header.h"), f.0.join("a/renamed.h")).unwrap();
    assert_ne!(before, digest_tree(&f.0.join("a")).unwrap());
}
#[test]
fn native_digest_syntax_and_architecture_are_validated() {
    let f = Fixture::new();
    let mut m = f.manifest();
    a(&mut m).digest = "unpinned".into();
    error(&f, &m, "native_digest_invalid");
    m = f.manifest();
    fs::write(
        f.0.join("a/lib.a"),
        archive(
            "entry.o",
            &object_bytes("answer", object::Architecture::Aarch64),
            0o100644,
        ),
    )
    .unwrap();
    a(&mut m).digest = digest_tree(&f.0.join("a")).unwrap();
    error(&f, &m, "native_architecture_mismatch");
}
#[test]
fn native_target_and_abi_never_fall_back() {
    let f = Fixture::new();
    let mut m = f.manifest();
    assert!(
        resolve(&f.0, &m, "aarch64-apple-darwin", true)
            .unwrap_err()
            .to_string()
            .contains("native_target_missing")
    );
    a(&mut m).architecture = "aarch64".into();
    error(&f, &m, "native_abi_mismatch");
    a(&mut m).architecture = "x86_64".into();
    a(&mut m).abi = "c-msvc".into();
    error(&f, &m, "native_abi_mismatch");
    a(&mut m).abi = "c-sysv".into();
    a(&mut m).abi_version = "2".into();
    error(&f, &m, "native_abi_mismatch");
}
#[test]
fn native_paths_and_linkage_are_checked() {
    let f = Fixture::new();
    let mut m = f.manifest();
    for path in ["../a/lib.a", "/tmp/lib.a", "C:/lib.a", "..\\lib.a"] {
        a(&mut m).inputs = vec![LinkInput::File(path.into())];
        error(&f, &m, "native_path_escape");
    }
    m = f.manifest();
    a(&mut m).linkage = Linkage::Shared;
    error(&f, &m, "native_loader_policy_required");
    m = f.manifest();
    a(&mut m).header_roots = vec!["lib.a".into()];
    error(&f, &m, "native_root_not_directory");
    a(&mut m).header_roots.clear();
    a(&mut m).inputs = vec![LinkInput::System("m".into())];
    error(&f, &m, "native_ambient_lookup_forbidden");
}
#[test]
fn native_archive_traversal_links_and_empty_archives_are_rejected() {
    let f = Fixture::new();
    let mut m = f.manifest();
    for (bytes, expected) in [
        (
            archive(
                "../escape.o",
                &object_bytes("x", object::Architecture::X86_64),
                0o100644,
            ),
            "native_unsafe_archive",
        ),
        (
            archive(
                "link.o",
                &object_bytes("x", object::Architecture::X86_64),
                0o120777,
            ),
            "native_unsafe_archive",
        ),
        (b"!<arch>\n".to_vec(), "native_empty_archive"),
        (b"!<thin>\n".to_vec(), "native_unsafe_archive"),
        (
            b"PK zip is not an object archive".to_vec(),
            "native_linkage_mismatch",
        ),
    ] {
        fs::write(f.0.join("a/lib.a"), bytes).unwrap();
        a(&mut m).digest = digest_tree(&f.0.join("a")).unwrap();
        error(&f, &m, expected);
    }
}
#[cfg(unix)]
#[test]
fn native_symlink_escape_is_rejected() {
    let f = Fixture::new();
    let m = f.manifest();
    std::os::unix::fs::symlink("../../outside", f.0.join("a/link")).unwrap();
    error(&f, &m, "native_unsafe_link");
}
#[test]
fn native_symbol_and_version_conflicts_report_dependency_paths() {
    let f = Fixture::new();
    let mut m = f.manifest();
    m.libraries.insert("b".into(), f.library("b", "answer"));
    m.libraries
        .get_mut("a")
        .unwrap()
        .dependencies
        .push("b".into());
    error(&f, &m, "native_symbol_conflict: answer: a and a -> b");
    m.libraries.get_mut("b").unwrap().name = "a".into();
    m.libraries.get_mut("b").unwrap().version = "2".into();
    error(&f, &m, "native_version_conflict: a and a -> b");
}
#[test]
fn native_graph_rejects_missing_and_unreachable_nodes() {
    let f = Fixture::new();
    let mut m = f.manifest();
    m.libraries
        .get_mut("a")
        .unwrap()
        .dependencies
        .push("missing".into());
    error(&f, &m, "native_dependency_missing");
    m = f.manifest();
    m.roots = vec!["missing".into()];
    error(&f, &m, "native_root_missing");
    m.roots.clear();
    error(&f, &m, "native_unreachable_library");
}
#[test]
fn native_system_dependency_requires_explicit_nonhermetic_policy() {
    let f = Fixture::new();
    let mut m = f.manifest();
    let art = a(&mut m);
    art.source = Source::System {
        provider: "libc".into(),
        version: "1".into(),
    };
    art.linkage = Linkage::System;
    art.inputs = vec![LinkInput::System("m".into()), LinkInput::System("m".into())];
    art.digest = digest(&serde_json::to_vec(&("libc", "1", TARGET)).unwrap());
    error(&f, &m, "native_non_hermetic");
    m.reproducible = false;
    let plan = resolve(&f.0, &m, TARGET, true).unwrap();
    assert_eq!(plan.args, ["-lm", "-lm"]);
    assert_eq!(plan.notices.len(), 1);
}
#[test]
fn native_pinned_artifact_offline_and_checksum() {
    let f = Fixture::new();
    let mut m = f.manifest();
    let bytes = fs::read(f.0.join("a/lib.a")).unwrap();
    let hash = digest(&bytes);
    let art = a(&mut m);
    art.source = Source::Artifact {
        url: "https://example.invalid/library.a".into(),
    };
    art.digest = hash.clone();
    art.inputs = vec![LinkInput::File("artifact".into())];
    error(&f, &m, "native_cache_missing_offline");
    let dir = f.0.join(".willow/native").join(hash);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("artifact"), bytes).unwrap();
    resolve(&f.0, &m, TARGET, true).unwrap();
    fs::write(dir.join("artifact"), "broken").unwrap();
    error(&f, &m, "native_digest_mismatch");
    a(&mut m).source = Source::Artifact {
        url: "http://example.invalid/library.a".into(),
    };
    error(&f, &m, "native_unpinned_source");
}

fn shape(n: usize, kind: &str) -> NativeManifest {
    let mut m = NativeManifest {
        roots: vec!["n0".into()],
        reproducible: true,
        libraries: BTreeMap::new(),
    };
    for i in 0..n {
        let deps = match kind {
            "fanout" if i == 0 => (1..n).map(|j| format!("n{j}")).collect(),
            "diamond" if i + 2 < n => vec![format!("n{}", i + 1), format!("n{}", i + 2)],
            "cycle" => vec![format!("n{}", (i + 1) % n)],
            "deep" | "diamond" if i + 1 < n => vec![format!("n{}", i + 1)],
            _ => vec![],
        };
        m.libraries.insert(
            format!("n{i}"),
            Library {
                name: format!("n{i}"),
                version: "1".into(),
                license: "MIT".into(),
                provenance: "test".into(),
                dependencies: deps,
                targets: BTreeMap::new(),
            },
        );
    }
    m
}
#[test]
fn native_graph_work_is_linear_for_deep_diamond_fanout_and_cycles() {
    for kind in ["deep", "diamond", "fanout", "cycle"] {
        for n in [1, 8, 64, 4096] {
            let m = shape(n, kind);
            let e: usize = m.libraries.values().map(|l| l.dependencies.len()).sum();
            let mut work = Work::default();
            let graph = graph::Graph::new(&m, &mut work).unwrap();
            let components = graph.components(&mut work);
            assert_eq!(work.vertices, 4 * n);
            assert_eq!(work.edges, 4 * e);
            assert_eq!(components.len(), if kind == "cycle" { 1 } else { n });
            if kind != "cycle" {
                let mut ranks = vec![0; n];
                for (rank, c) in components.iter().enumerate() {
                    ranks[c[0]] = rank;
                }
                for (i, edges) in graph.edges.iter().enumerate() {
                    for &j in edges {
                        assert!(ranks[i] < ranks[j]);
                    }
                }
            }
            if n <= 64 {
                println!(
                    "{kind} V={n} E={e} vertex_visits={} edge_visits={}",
                    work.vertices, work.edges
                );
            }
        }
    }
}
#[test]
fn native_static_cycles_use_groups_and_other_linkers_reject_them() {
    let f = Fixture::new();
    let mut m = f.manifest();
    m.libraries
        .get_mut("a")
        .unwrap()
        .dependencies
        .push("a".into());
    let plan = resolve(&f.0, &m, TARGET, true).unwrap();
    assert_eq!(plan.args.first().unwrap(), "-Wl,--start-group");
    assert_eq!(plan.args.last().unwrap(), "-Wl,--end-group");
    for target in [
        "x86_64-pc-windows-msvc",
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
    ] {
        assert!(
            resolve(&f.0, &m, target, true)
                .unwrap_err()
                .to_string()
                .contains("UnsupportedLinkCycle")
        );
    }
    // No payload read is needed to decide unsupported grouping.
    let mut work = Work::default();
    assert_eq!(
        graph::Graph::new(&m, &mut work)
            .unwrap()
            .components(&mut work)
            .len(),
        1
    );
}

fn write_project(f: &Fixture, manifest: &NativeManifest) {
    let native = toml::to_string(&BTreeMap::from([("native", manifest)])).unwrap();
    fs::write(
        f.0.join("project.toml"),
        format!("[project]\nname='native_app'\nversion='1.0.0'\n{native}"),
    )
    .unwrap();
    fs::create_dir_all(f.0.join("src")).unwrap();
    fs::write(f.0.join("src/main.wi"), "fn main() {}").unwrap();
}
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn native_lock_drift_frozen_and_failed_resolution_are_atomic() {
    let f = Fixture::new();
    let mut m = f.manifest();
    write_project(&f, &m);
    let lock = f.0.join("project.lock");
    assert!(crate::package::lock::fetch_packages(&f.0, true, true).is_err());
    assert!(!lock.exists());
    crate::package::lock::fetch_packages(&f.0, false, true).unwrap();
    let before = fs::read(&lock).unwrap();
    crate::package::lock::fetch_packages(&f.0, true, true).unwrap();
    assert_eq!(before, fs::read(&lock).unwrap());
    m.libraries.get_mut("a").unwrap().version = "2".into();
    write_project(&f, &m);
    let error = crate::package::lock::fetch_packages(&f.0, true, true).unwrap_err();
    assert!(error.to_string().contains("lockfile_stale"));
    assert_eq!(before, fs::read(&lock).unwrap());
    a(&mut m).digest = "0".repeat(64);
    write_project(&f, &m);
    assert!(crate::package::lock::fetch_packages(&f.0, false, false).is_err());
    assert_eq!(before, fs::read(&lock).unwrap());
    a(&mut m).source = Source::Artifact {
        url: "https://example.invalid/interrupt.a".into(),
    };
    a(&mut m).inputs = vec![LinkInput::File("artifact".into())];
    write_project(&f, &m);
    assert!(crate::package::lock::fetch_packages(&f.0, false, true).is_err());
    assert_eq!(before, fs::read(&lock).unwrap());
    // Removing native dependencies is lock drift too.
    fs::write(
        f.0.join("project.toml"),
        "[project]\nname='native_app'\nversion='1.0.0'",
    )
    .unwrap();
    assert!(crate::package::lock::fetch_packages(&f.0, true, true).is_err());
    crate::package::lock::fetch_packages(&f.0, false, true).unwrap();
    assert!(!fs::read_to_string(lock).unwrap().contains("[native"));
}
#[test]
fn native_manifest_serialization_is_strict_and_stable() {
    let f = Fixture::new();
    let m = f.manifest();
    write_project(&f, &m);
    assert_eq!(
        crate::project::ProjectManifest::load(&f.0.join("project.toml"))
            .unwrap()
            .native,
        Some(m.clone())
    );
    let text = toml::to_string(&m).unwrap();
    assert_eq!(toml::from_str::<NativeManifest>(&text).unwrap(), m);
    assert!(
        toml::from_str::<NativeManifest>(&text.replace(
            "reproducible = true",
            "reproducible = true\nloader = 'ambient'"
        ))
        .is_err()
    );
}
#[test]
fn native_shared_source_tree_is_hashed_once() {
    let f = Fixture::new();
    let mut m = f.manifest();
    fs::write(
        f.0.join("a/second.a"),
        archive(
            "second.o",
            &object_bytes("second", object::Architecture::X86_64),
            0o100644,
        ),
    )
    .unwrap();
    a(&mut m).digest = digest_tree(&f.0.join("a")).unwrap();
    let mut second = m.libraries["a"].clone();
    second.name = "b".into();
    second.targets.get_mut(TARGET).unwrap().inputs = vec![LinkInput::File("second.a".into())];
    m.libraries
        .get_mut("a")
        .unwrap()
        .dependencies
        .push("b".into());
    m.libraries.insert("b".into(), second);
    let plan = resolve(&f.0, &m, TARGET, true).unwrap();
    assert_eq!(
        plan.work.bytes,
        fs::metadata(f.0.join("a/lib.a")).unwrap().len()
            + fs::metadata(f.0.join("a/second.a")).unwrap().len()
    );
}

#[test]
fn native_interrupted_fetch_discards_partial_artifact() {
    use std::io::Write;
    let f = Fixture::new();
    let expected = digest(b"complete");
    let error = payload::fetch_with(
        &f.0,
        "https://example.invalid/lib.a",
        &expected,
        false,
        |mut file| {
            file.write_all(b"partial")?;
            bail!("interrupted transfer")
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("interrupted transfer"));
    assert_eq!(
        fs::read_dir(f.0.join(".willow/native").join(&expected))
            .unwrap()
            .count(),
        0
    );
    let file = payload::fetch_with(
        &f.0,
        "https://example.invalid/lib.a",
        &expected,
        false,
        |mut file| {
            file.write_all(b"complete")?;
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(fs::read(file).unwrap(), b"complete");
    payload::fetch_with(
        &f.0,
        "https://example.invalid/lib.a",
        &expected,
        true,
        |_| panic!("offline fetched"),
    )
    .unwrap();
}
#[cfg(unix)]
#[test]
fn native_pinned_cache_cannot_escape_through_symlinks() {
    let f = Fixture::new();
    let outside = Fixture::new();
    std::os::unix::fs::symlink(&outside.0, f.0.join(".willow")).unwrap();
    let e = payload::fetch_with(
        &f.0,
        "https://example.invalid/a",
        &"0".repeat(64),
        false,
        |_| panic!("unsafe fetch"),
    )
    .unwrap_err();
    assert!(e.to_string().contains("native_unsafe_cache_path"));
    assert_eq!(fs::read_dir(&outside.0).unwrap().count(), 0);
}
#[test]
fn native_overlapping_source_roots_reuse_merkle_hashes() {
    let f = Fixture::new();
    fs::create_dir_all(f.0.join("a/b")).unwrap();
    fs::write(f.0.join("a/b/input"), b"payload").unwrap();
    let mut cache = HashMap::new();
    let mut bytes = 0;
    hash_tree(&f.0.join("a"), &mut cache, &mut bytes).unwrap();
    hash_tree(&f.0.join("a/b"), &mut cache, &mut bytes).unwrap();
    assert_eq!(bytes, 7);
}

#[test]
fn native_payload_headers_match_all_four_supported_targets() {
    let f = Fixture::new();
    for (target, arch, abi, format) in [
        (
            TARGET,
            object::Architecture::X86_64,
            "c-sysv",
            object::BinaryFormat::Elf,
        ),
        (
            "x86_64-pc-windows-msvc",
            object::Architecture::X86_64,
            "c-msvc",
            object::BinaryFormat::Coff,
        ),
        (
            "aarch64-apple-darwin",
            object::Architecture::Aarch64,
            "c-darwin",
            object::BinaryFormat::MachO,
        ),
        (
            "x86_64-apple-darwin",
            object::Architecture::X86_64,
            "c-darwin",
            object::BinaryFormat::MachO,
        ),
    ] {
        let mut m = f.manifest();
        let mut art = a(&mut m).clone();
        fs::write(
            f.0.join("a/lib.a"),
            archive(
                "entry.o",
                &object_bytes_for("answer", arch, format),
                0o100644,
            ),
        )
        .unwrap();
        art.digest = digest_tree(&f.0.join("a")).unwrap();
        art.abi = abi.into();
        art.architecture = if arch == object::Architecture::Aarch64 {
            "aarch64"
        } else {
            "x86_64"
        }
        .into();
        m.libraries.get_mut("a").unwrap().targets = BTreeMap::from([(target.into(), art)]);
        resolve(&f.0, &m, target, true).unwrap();
    }
}

#[test]
fn native_static_entry_requires_an_actual_payload() {
    let f = Fixture::new();
    let mut m = f.manifest();
    a(&mut m).inputs = vec![LinkInput::Search(".".into())];
    error(&f, &m, "native_artifact_missing");
}

fn coff_object(kind: Option<object::ComdatKind>, bytes: &[u8], associated: bool) -> Vec<u8> {
    use object::write::{Comdat, Object, Symbol, SymbolSection};
    let mut object = Object::new(
        object::BinaryFormat::Coff,
        object::Architecture::X86_64,
        object::Endianness::Little,
    );
    let section = object.add_section(vec![], b".text$inline".to_vec(), object::SectionKind::Text);
    object.append_section_data(section, bytes, 1);
    let symbol = object.add_symbol(Symbol {
        name: b"inline_answer".to_vec(),
        value: 0,
        size: bytes.len() as u64,
        kind: object::SymbolKind::Text,
        scope: object::SymbolScope::Linkage,
        weak: false,
        section: SymbolSection::Section(section),
        flags: object::SymbolFlags::None,
    });
    let mut sections = vec![section];
    if associated {
        let section = object.add_section(
            vec![],
            b".data$associated".to_vec(),
            object::SectionKind::Data,
        );
        object.append_section_data(section, b"associated", 1);
        object.add_symbol(Symbol {
            name: b"associated".to_vec(),
            value: 0,
            size: 10,
            kind: object::SymbolKind::Data,
            scope: object::SymbolScope::Linkage,
            weak: false,
            section: SymbolSection::Section(section),
            flags: object::SymbolFlags::None,
        });
        sections.push(section);
    }
    if let Some(kind) = kind {
        for &section in &sections {
            object.section_symbol(section);
        }
        object.add_comdat(Comdat {
            kind,
            symbol,
            sections,
        });
    }
    object.write().unwrap()
}
fn coff_pair(first: Vec<u8>, second: Vec<u8>) -> Result<NativePlan> {
    let f = Fixture::new();
    let mut m = f.manifest();
    m.libraries.insert("b".into(), f.library("b", "second"));
    m.libraries
        .get_mut("a")
        .unwrap()
        .dependencies
        .push("b".into());
    for (name, bytes) in [("a", first), ("b", second)] {
        fs::write(
            f.0.join(name).join("lib.a"),
            archive("entry.obj", &bytes, 0o100644),
        )
        .unwrap();
        let library = m.libraries.get_mut(name).unwrap();
        let mut art = library.targets.remove(TARGET).unwrap();
        art.abi = "c-msvc".into();
        art.digest = digest_tree(&f.0.join(name)).unwrap();
        library.targets.insert("x86_64-pc-windows-msvc".into(), art);
    }
    resolve(&f.0, &m, "x86_64-pc-windows-msvc", true)
}
#[test]
fn native_coff_comdat_selection_allows_linker_equivalent_definitions() {
    use object::ComdatKind::*;
    for kind in [Any, Largest, Newest] {
        coff_pair(
            coff_object(Some(kind), &[1], true),
            coff_object(Some(kind), &[2, 3], true),
        )
        .unwrap();
    }
    coff_pair(
        coff_object(Some(SameSize), &[1], true),
        coff_object(Some(SameSize), &[2], true),
    )
    .unwrap();
    coff_pair(
        coff_object(Some(ExactMatch), &[1, 2], true),
        coff_object(Some(ExactMatch), &[1, 2], true),
    )
    .unwrap();
}
#[test]
fn native_coff_comdat_selection_keeps_genuine_conflicts() {
    use object::ComdatKind::*;
    for (a, b) in [
        (
            coff_object(None, &[1], false),
            coff_object(None, &[1], false),
        ),
        (
            coff_object(Some(NoDuplicates), &[1], false),
            coff_object(Some(NoDuplicates), &[1], false),
        ),
        (
            coff_object(Some(SameSize), &[1], false),
            coff_object(Some(SameSize), &[1, 2], false),
        ),
        (
            coff_object(Some(ExactMatch), &[1], false),
            coff_object(Some(ExactMatch), &[2], false),
        ),
        (
            coff_object(None, &[1], false),
            coff_object(Some(Any), &[1], false),
        ),
        (
            coff_object(Some(Any), &[1], false),
            coff_object(Some(Largest), &[1], false),
        ),
    ] {
        let error = coff_pair(a, b).unwrap_err();
        assert!(
            error.to_string().contains("native_symbol_conflict"),
            "{error}"
        );
    }
}
#[test]
fn native_successful_download_publishes_complete_cache_file() {
    use std::io::Write;
    let f = Fixture::new();
    let bytes = b"complete downloaded artifact";
    let expected = digest(bytes);
    let file = payload::fetch_with(
        &f.0,
        "https://example.invalid/native",
        &expected,
        false,
        |mut output| {
            output.write_all(bytes)?;
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(fs::read(&file).unwrap(), bytes);
    assert_eq!(fs::read_dir(file.parent().unwrap()).unwrap().count(), 1);
    payload::fetch_with(
        &f.0,
        "https://example.invalid/native",
        &expected,
        true,
        |_| panic!("downloaded cached file"),
    )
    .unwrap();
}
fn shared_policy() -> willow_abi::native::LoaderPolicy {
    willow_abi::native::LoaderPolicy {
        distribution_name: "sample.so".into(),
        abi_symbol: "negotiate".into(),
        version: 1,
        symbols: vec!["answer".into()],
        dependency_search: willow_abi::native::DependencySearch::Platform,
        platform_provider: "test-os".into(),
        platform_version: "1".into(),
    }
}
#[test]
fn native_distribution_is_verified_and_replaces_only_owned_contents() {
    let f = Fixture::new();
    let source = f.0.join("source");
    fs::write(&source, b"verified input").unwrap();
    let artifact = distribution::SharedArtifact {
        source: source.clone(),
        library: willow_abi::native::Library {
            name: "sample".into(),
            digest: digest_file(&source).unwrap(),
            policy: shared_policy(),
        },
    };
    let executable = f.0.join("app");
    let target = willow_abi::native::directory(&executable).unwrap();
    let prepared = distribution::prepare(&executable, std::slice::from_ref(&artifact)).unwrap();
    assert!(!target.exists());
    prepared.publish().unwrap();
    assert_eq!(
        fs::read(target.join("sample.so")).unwrap(),
        b"verified input"
    );
    fs::write(&source, b"mutated").unwrap();
    assert!(distribution::prepare(&executable, std::slice::from_ref(&artifact)).is_err());
    assert_eq!(
        fs::read(target.join("sample.so")).unwrap(),
        b"verified input"
    );
    fs::write(target.join("user.txt"), b"preserve").unwrap();
    assert!(distribution::prepare(&executable, &[]).is_err());
    assert!(target.join("user.txt").exists());
    fs::remove_file(target.join("user.txt")).unwrap();
    distribution::prepare(&executable, &[])
        .unwrap()
        .publish()
        .unwrap();
    assert!(!target.exists());
}
#[test]
fn native_loader_policy_is_explicit_and_validated() {
    let f = Fixture::new();
    let mut m = f.manifest();
    a(&mut m).linkage = Linkage::Shared;
    error(&f, &m, "native_loader_policy_required");
    m.reproducible = false;
    let mut policy = shared_policy();
    policy.distribution_name = "../host.so".into();
    a(&mut m).loader = Some(policy);
    error(&f, &m, "native_loader_policy_invalid");
    a(&mut m).loader = Some(shared_policy());
    error(&f, &m, "native_artifact_invalid");
    a(&mut m).linkage = Linkage::Static;
    error(&f, &m, "native_loader_policy_on_non_shared");
}

#[test]
fn native_comdat_symbol_index_scales_in_one_pass() {
    use object::Object as _;
    use object::write::{Comdat, Object, Symbol, SymbolSection};
    for count in [1, 8, 64] {
        let f = Fixture::new();
        let mut object = Object::new(
            object::BinaryFormat::Coff,
            object::Architecture::X86_64,
            object::Endianness::Little,
        );
        for i in 0..count {
            let section = object.add_section(
                vec![],
                format!(".text${i}").into_bytes(),
                object::SectionKind::Text,
            );
            object.section_symbol(section);
            object.append_section_data(section, &[0xc3], 1);
            let symbol = object.add_symbol(Symbol {
                name: format!("function_{i}").into_bytes(),
                value: 0,
                size: 1,
                kind: object::SymbolKind::Text,
                scope: object::SymbolScope::Linkage,
                weak: false,
                section: SymbolSection::Section(section),
                flags: object::SymbolFlags::None,
            });
            object.add_comdat(Comdat {
                kind: object::ComdatKind::Any,
                symbol,
                sections: vec![section],
            });
        }
        let bytes = object.write().unwrap();
        let expected = object::File::parse(bytes.as_slice())
            .unwrap()
            .symbols()
            .count();
        let path = f.0.join("many.lib");
        fs::write(&path, archive("many.obj", &bytes, 0o100644)).unwrap();
        payload::take_symbol_visits();
        payload::validate(&path, "x86_64", "c-msvc", Linkage::Static, |_, _| Ok(())).unwrap();
        let visits = payload::take_symbol_visits();
        assert_eq!(visits, expected);
        assert_eq!(visits, 2 * count);
        println!("COMDAT groups={count} symbol_index_visits={visits}");
    }
}

#[test]
fn resolved_inputs_are_paths_the_linker_can_open() {
    let fixture = Fixture::new();
    fs::write(fixture.0.join("add.lib"), b"archive").unwrap();
    let path = rooted(&fixture.0, "add.lib").unwrap();
    assert!(path.is_absolute(), "{}", path.display());
    assert!(
        !path.to_string_lossy().starts_with(r"\\?\"),
        "{}",
        path.display()
    );
    assert_eq!(fs::read(path).unwrap(), b"archive");
}
