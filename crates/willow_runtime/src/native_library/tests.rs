use super::*;
use willow_abi::native::{DependencySearch, Library, LoaderPolicy, SCHEMA};

struct Fixture {
    root: std::path::PathBuf,
    executable: std::path::PathBuf,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "willow-loader-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let executable = root.join("app");
        std::fs::create_dir(willow_abi::native::directory(&executable).unwrap()).unwrap();
        Self { root, executable }
    }
    fn directory(&self) -> std::path::PathBuf {
        willow_abi::native::directory(&self.executable).unwrap()
    }
    fn write(&self, policy: &Distribution) {
        std::fs::write(
            self.directory().join(POLICY_FILE),
            serde_json::to_vec(policy).unwrap(),
        )
        .unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn policy() -> Distribution {
    Distribution {
        schema: SCHEMA,
        libraries: vec![Library {
            name: "sample".into(),
            digest: "0".repeat(64),
            policy: LoaderPolicy {
                distribution_name: "sample.so".into(),
                abi_symbol: "negotiate".into(),
                version: 1,
                symbols: vec!["answer".into()],
                dependency_search: DependencySearch::Platform,
                platform_provider: "test-os".into(),
                platform_version: "1".into(),
            },
        }],
    }
}
fn failure(f: &Fixture, text: &str) {
    match Registry::load(&f.executable) {
        Ok(_) => panic!("unexpected success"),
        Err(e) => assert!(e.contains(text), "{e}"),
    }
}

#[test]
fn native_loader_policy_rejects_paths_duplicates_and_invalid_versions() {
    for invalid in [
        "../outside.so",
        "/tmp/a.so",
        "C:\\a.dll",
        "a/b.so",
        ".",
        "a.",
        "NUL.dll",
        "COM1",
        "lpt9.txt",
    ] {
        let mut p = policy();
        p.libraries[0].policy.distribution_name = invalid.into();
        assert!(p.validate().is_err());
    }
    let mut p = policy();
    p.libraries[0].policy.version = 0;
    assert!(p.validate().is_err());
    p = policy();
    p.libraries.push(p.libraries[0].clone());
    assert!(p.validate().is_err());
    p = policy();
    p.libraries[0].policy.symbols.push("answer".into());
    assert!(p.validate().is_err());
    p = policy();
    p.schema += 1;
    assert!(p.validate().is_err());
}
#[test]
fn native_loader_checks_digest_before_executing_payload() {
    let f = Fixture::new();
    let p = policy();
    f.write(&p);
    std::fs::write(f.directory().join("sample.so"), b"not a library").unwrap();
    failure(&f, "native_digest_mismatch");
}
#[test]
fn native_loader_missing_policy_and_unknown_names_fail_closed() {
    let f = Fixture::new();
    assert!(Registry::load(&f.executable).is_err());
    let p = Distribution {
        schema: SCHEMA,
        libraries: vec![],
    };
    f.write(&p);
    let registry = Registry::load(&f.executable).unwrap();
    assert!(registry.symbol("/tmp/host.so", "answer").is_err());
    assert!(registry.symbol("sample", "not_allowed").is_err());
}
#[cfg(unix)]
#[test]
fn native_loader_rejects_distribution_and_payload_symlinks() {
    let f = Fixture::new();
    let p = policy();
    f.write(&p);
    std::fs::write(f.root.join("host.so"), "host").unwrap();
    std::os::unix::fs::symlink(f.root.join("host.so"), f.directory().join("sample.so")).unwrap();
    failure(&f, "native_distribution_escape");
    std::fs::remove_dir_all(f.directory()).unwrap();
    std::os::unix::fs::symlink(&f.root, f.directory()).unwrap();
    failure(&f, "native_distribution_escape");
}
#[test]
fn native_loader_abi_signature_and_blocking_effect() {
    let _: willow_abi::native::Symbol = willow_native_symbol_v1;
    let row = willow_abi::runtime_symbol("willow_native_symbol_v1").unwrap();
    assert_eq!(
        row.params,
        &[willow_abi::AbiTy::Ptr, willow_abi::AbiTy::Ptr]
    );
    assert_eq!(row.ret, Some(willow_abi::AbiTy::Ptr));
    assert!(row.effects.contains(willow_abi::RuntimeEffects::MAY_BLOCK));
}

#[test]
fn native_loader_rejects_oversized_policy_before_parsing() {
    let f = Fixture::new();
    f.write(&policy());
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(f.directory().join(POLICY_FILE))
        .unwrap();
    file.set_len(MAX_POLICY_BYTES + 1).unwrap();
    drop(file);
    failure(&f, "native_policy_invalid");
}
