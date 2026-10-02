use super::*;
use source_map::SourceLookup;
use std::{cell::Cell, path::Path};

#[test]
fn project_paths_borrow_sources_and_preserve_external_and_virtual_names() {
    struct Inspect<'a> {
        map: &'a SourceMap,
        expected: &'a str,
    }
    impl DiagnosticEmitter for Inspect<'_> {
        fn emit(&mut self, _: &Diagnostic, sources: &dyn SourceLookup) -> std::io::Result<()> {
            assert!(std::ptr::eq(sources.get(FileId::ENTRY).unwrap(), self.map));
            assert_eq!(sources.display_path(FileId::ENTRY), Some(self.expected));
            assert!(sources.display_path(FileId(999)).is_none());
            Ok(())
        }
    }
    let root = std::env::temp_dir().join("project-path-root");
    let internal = root.join("src").join("main.wi");
    let sibling = root
        .with_file_name("project-path-root-other")
        .join("lib.wi");
    let relative = Path::new("src").join("main.wi");
    for (path, expected) in [
        (internal.to_str().unwrap(), relative.to_str().unwrap()),
        (sibling.to_str().unwrap(), sibling.to_str().unwrap()),
        ("src/main.wi", "src/main.wi"),
        ("<std::option>", "<std::option>"),
    ] {
        let map = SourceMap::new(path, "x\n".repeat(4096));
        ProjectEmitter {
            root: &root,
            inner: &mut Inspect {
                map: &map,
                expected,
            },
        }
        .emit(
            &Diagnostic::new(Severity::Error, ErrorCode::E0001, "example"),
            &map,
        )
        .unwrap();
        assert_eq!(map.path, path);
    }
}

#[test]
fn project_path_lookup_counts_and_write_failure() {
    struct Counted {
        map: SourceMap,
        calls: Cell<usize>,
    }
    impl SourceLookup for Counted {
        fn get(&self, _: FileId) -> Option<&SourceMap> {
            self.calls.set(self.calls.get() + 1);
            Some(&self.map)
        }
    }
    struct Render;
    impl DiagnosticEmitter for Render {
        fn emit(&mut self, d: &Diagnostic, sources: &dyn SourceLookup) -> std::io::Result<()> {
            reporter::emit_with(d, sources, &mut std::io::sink())
        }
    }
    let root = std::env::temp_dir().join("project-path-counts");
    for n in [16, 64, 256, 1024] {
        for separate_files in [false, true] {
            let source = Counted {
                map: SourceMap::new(root.join("main.wi").to_str().unwrap(), "x"),
                calls: Cell::new(0),
            };
            let mut d = Diagnostic::new(Severity::Error, ErrorCode::E0001, "counts");
            for i in 0..n {
                d.labels.push(Label::primary(
                    Span::in_file(
                        FileId(if separate_files { i as u32 } else { 0 }),
                        0,
                        1,
                        1,
                        1,
                    ),
                    "",
                ));
            }
            ProjectEmitter {
                root: &root,
                inner: &mut Render,
            }
            .emit(&d, &source)
            .unwrap();
            let expected = if separate_files { 2 * n } else { 2 };
            assert_eq!(source.calls.get(), expected);
            eprintln!(
                "project-path-count n={n} separate_files={separate_files} lookups={expected}"
            );
        }
    }
    struct Broken;
    impl DiagnosticEmitter for Broken {
        fn emit(&mut self, _: &Diagnostic, _: &dyn SourceLookup) -> std::io::Result<()> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
    }
    assert_eq!(
        ProjectEmitter {
            root: &root,
            inner: &mut Broken
        }
        .emit(
            &Diagnostic::new(Severity::Error, ErrorCode::E0001, "fail"),
            &SourceMap::new("x", "")
        )
        .unwrap_err()
        .kind(),
        std::io::ErrorKind::BrokenPipe
    );
}
