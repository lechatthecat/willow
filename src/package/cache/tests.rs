use super::*;

#[test]
fn home_defaults_and_explicit_override_are_platform_independent() {
    for windows in [false, true] {
        assert_eq!(
            home_from(Some("custom".into()), None, None, windows).unwrap(),
            PathBuf::from("custom")
        );
        assert!(home_from(None, None, None, windows).is_err());
    }
    for _platform in ["Linux", "macOS"] {
        assert_eq!(
            home_from(None, Some("/users/me".into()), None, false).unwrap(),
            PathBuf::from("/users/me/.willow")
        );
    }
    assert_eq!(
        home_from(None, Some("other".into()), Some("profile".into()), true).unwrap(),
        PathBuf::from("profile/.willow")
    );
    assert_eq!(
        home_from(None, Some("fallback".into()), None, true).unwrap(),
        PathBuf::from("fallback/.willow")
    );
}

#[test]
fn radix_matches_lexical_order_with_prefixes_and_unicode() {
    let mut names: Vec<_> = ["", "a", "abc", "ab", "b", "あ", "é"]
        .into_iter()
        .map(|s| s.as_bytes().to_vec())
        .collect();
    for n in 0..1024usize {
        names.push(format!("shared-prefix-{:04x}", n.wrapping_mul(7919) % 1024).into_bytes());
    }
    let mut actual: Vec<_> = names
        .iter()
        .rev()
        .map(|name| Child {
            name: name.clone(),
            digest: [0; 32],
        })
        .collect();
    radix(&mut actual);
    names.sort();
    assert_eq!(
        actual.into_iter().map(|c| c.name).collect::<Vec<_>>(),
        names
    );
}

#[test]
fn checksum_is_order_independent_and_detects_names_content_and_empty_dirs() {
    let temp = Temporary::new(&std::env::temp_dir()).unwrap();
    let a = temp.0.join("a");
    let b = temp.0.join("b");
    fs::create_dir(&a).unwrap();
    fs::create_dir(&b).unwrap();
    for name in ["one", "two", "three"] {
        fs::write(a.join(name), name).unwrap();
    }
    for name in ["three", "two", "one"] {
        fs::write(b.join(name), name).unwrap();
    }
    let original = tree_checksum(&a).unwrap();
    assert_eq!(original, tree_checksum(&b).unwrap());
    fs::write(b.join("one"), "changed").unwrap();
    assert_ne!(original, tree_checksum(&b).unwrap());
    fs::write(b.join("one"), "one").unwrap();
    fs::rename(b.join("one"), b.join("renamed")).unwrap();
    assert_ne!(original, tree_checksum(&b).unwrap());
    fs::rename(b.join("renamed"), b.join("one")).unwrap();
    fs::create_dir(b.join("empty")).unwrap();
    assert_ne!(original, tree_checksum(&b).unwrap());
}

#[cfg(unix)]
#[test]
fn checksum_hashes_symlink_text_without_reading_external_content() {
    let temp = Temporary::new(&std::env::temp_dir()).unwrap();
    let root = temp.0.join("tree");
    fs::create_dir(&root).unwrap();
    let external = temp.0.join("external");
    fs::write(&external, "one").unwrap();
    std::os::unix::fs::symlink("../external", root.join("link")).unwrap();
    let original = tree_checksum(&root).unwrap();
    fs::write(external, "two").unwrap();
    assert_eq!(original, tree_checksum(&root).unwrap());
    fs::remove_file(root.join("link")).unwrap();
    std::os::unix::fs::symlink("../different", root.join("link")).unwrap();
    assert_ne!(original, tree_checksum(&root).unwrap());
}

#[test]
fn checksum_scaling_counts_wide_and_deep_trees() {
    let temp = Temporary::new(&std::env::temp_dir()).unwrap();
    for n in [32, 64, 128] {
        for deep in [false, true] {
            let root = temp.0.join(format!("{n}-{deep}"));
            fs::create_dir(&root).unwrap();
            let mut directory = root.clone();
            for i in 0..n {
                if deep {
                    directory = directory.join("d");
                    fs::create_dir(&directory).unwrap();
                }
                fs::write(directory.join(format!("f{i:04}")), [42; 1024]).unwrap();
            }
            let (_, stats) = checksum_counted(&root).unwrap();
            assert_eq!(stats.entries, n * if deep { 2 } else { 1 });
            assert_eq!(stats.bytes, n * 1024);
            assert_eq!(stats.name_bytes, n * if deep { 6 } else { 5 });
            assert!(stats.radix_inspections <= 3 * (stats.name_bytes + stats.entries));
            println!(
                "shape={} n={n} entries={} content_bytes={} name_bytes={} radix_inspections={}",
                if deep { "deep" } else { "wide" },
                stats.entries,
                stats.bytes,
                stats.name_bytes,
                stats.radix_inspections
            );
        }
    }
}
