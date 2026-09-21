use super::*;

// ── std::fs v1 (willow-2s3 Stage 5 slice) ───────────────────────────────────
// Unsuffixed operations are synchronous compatibility APIs. Their `_async`
// counterparts execute in the scheduler blocking pool and return Tasks;
// all fallible ops return Result<_, IoError> with the
// failing path + OS message in IoError::Failed. 20 perspectives: 1 write+
// read roundtrip, 2 exists true/false, 3 read of missing file is Err with
// path in message, 4 write to unwritable dir is Err, 5 remove_file Ok +
// exists false, 6 remove of missing file is Err, 7 overwrite replaces
// contents, 8 empty file roundtrip, 9 multibyte UTF-8 contents, 10 newlines
// preserved, 11 `?` propagation of IoError, 12 `?` on the void write form,
// 13 fs in async fn, 14 GC stress roundtrip, 15 usable without import (builtin module, like env), 16 wrong arg count rejected, 17 wrong arg type
// rejected, 18 result must be matched (println of it rejected E1402),
// 19 large-ish contents (10k), 20 two files independent.

#[test]
fn fs_01_roundtrip() {
    let (out, ok) = compile_and_run(
        "import std::fs;\nfn main() { let p = fs::temp_path(\"willow_t01\"); fs::write_string(p, \"abc\"); match fs::read_to_string(p) { Ok(t) => println(t), Err(e) => println(\"no\"), } fs::remove_file(p); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "abc\n");
}

#[test]
fn fs_02_exists() {
    let (out, ok) = compile_and_run(
        "import std::fs;\nfn main() { let p = fs::temp_path(\"willow_t02\"); println(fs::exists(p)); fs::write_string(p, \"x\"); println(fs::exists(p)); fs::remove_file(p); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "false\ntrue\n");
}

#[test]
fn fs_03_missing_read_err_with_path() {
    let (out, ok) = compile_and_run(
        "import std::fs;\nfn main() { let p = fs::temp_path(\"willow_t03_missing\"); match fs::read_to_string(p) { Ok(t) => println(t), Err(e) => { match e { Failed(m) => println(m), } } } }",
    );
    assert!(ok, "{out}");
    assert!(out.contains("willow_t03_missing"), "{out}");
}

#[test]
fn fs_04_unwritable_err() {
    let (out, ok) = compile_and_run(
        "import std::fs;\nfn main() { let p = fs::temp_path(\"willow_t04_missing_dir\") + \"/t.txt\"; match fs::write_string(p, \"x\") { Ok(v) => println(\"ok\"), Err(e) => println(\"err\"), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "err\n");
}

#[test]
fn fs_05_remove_ok() {
    let (out, ok) = compile_and_run(
        "import std::fs;\nfn main() { let p = fs::temp_path(\"willow_t05\"); fs::write_string(p, \"x\"); match fs::remove_file(p) { Ok(v) => println(\"gone\"), Err(e) => println(\"err\"), } println(fs::exists(p)); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "gone\nfalse\n");
}

#[test]
fn fs_06_remove_missing_err() {
    let (out, ok) = compile_and_run(
        "import std::fs;\nfn main() { let p = fs::temp_path(\"willow_t06_missing\"); match fs::remove_file(p) { Ok(v) => println(\"ok\"), Err(e) => println(\"err\"), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "err\n");
}

#[test]
fn fs_07_overwrite() {
    let (out, ok) = compile_and_run(
        "import std::fs;\nfn main() { let p = fs::temp_path(\"willow_t07\"); fs::write_string(p, \"first\"); fs::write_string(p, \"second\"); match fs::read_to_string(p) { Ok(t) => println(t), Err(e) => println(\"no\"), } fs::remove_file(p); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "second\n");
}

#[test]
fn fs_08_empty_file() {
    let (out, ok) = compile_and_run(
        "import std::fs;\nfn main() { let p = fs::temp_path(\"willow_t08\"); fs::write_string(p, \"\"); match fs::read_to_string(p) { Ok(t) => println(t == \"\"), Err(e) => println(false), } fs::remove_file(p); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn fs_09_multibyte() {
    let (out, ok) = compile_and_run(
        "import std::fs;\nfn main() { let p = fs::temp_path(\"willow_t09\"); fs::write_string(p, \"日本語\"); match fs::read_to_string(p) { Ok(t) => println(t), Err(e) => println(\"no\"), } fs::remove_file(p); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "日本語\n");
}

#[test]
fn fs_10_newlines_preserved() {
    let (out, ok) = compile_and_run(
        "import std::fs;\nfn main() { let p = fs::temp_path(\"willow_t10\"); fs::write_string(p, \"a\\nb\"); match fs::read_to_string(p) { Ok(t) => println(t), Err(e) => println(\"no\"), } fs::remove_file(p); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "a\nb\n");
}

#[test]
fn fs_11_question_mark_read() {
    let (out, ok) = compile_and_run(
        "import std::fs;\nfn load(p: String) -> Result<String, IoError> { let t = fs::read_to_string(p)?; return Ok(t + \"!\"); }\nfn main() { match load(fs::temp_path(\"willow_t11_missing\")) { Ok(t) => println(t), Err(e) => println(\"propagated\"), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "propagated\n");
}

#[test]
fn fs_12_question_mark_write() {
    let (out, ok) = compile_and_run(
        "import std::fs;\nfn save(p: String) -> Result<void, IoError> { fs::write_string(p, \"x\")?; fs::remove_file(p)?; return Result::Ok(); }\nfn main() { match save(fs::temp_path(\"willow_t12\")) { Ok(v) => println(\"saved\"), Err(e) => println(\"err\"), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "saved\n");
}

#[test]
fn fs_13_in_async_fn() {
    let (out, ok) = compile_and_run(
        "import std::fs;\nasync fn work() -> i64 { let p = fs::temp_path(\"willow_t13\"); fs::write_string(p, \"async\"); await sleep(1); match fs::read_to_string(p) { Ok(t) => println(t), Err(e) => println(\"no\"), } fs::remove_file(p); return 1; }\nasync fn main() { await work(); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "async\n");
}

#[test]
fn fs_14_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        "import std::fs;\nfn main() { let p = fs::temp_path(\"willow_t14\"); fs::write_string(p, \"g\" + \"c\"); match fs::read_to_string(p) { Ok(t) => println(t + \"!\"), Err(e) => println(\"no\"), } fs::remove_file(p); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "gc!\n");
}

#[test]
fn fs_15_usable_without_import_like_env() {
    // Builtin schema modules (env, fs) are always visible; `import std::fs`
    // is stylistic — consistent with std::env.
    let (out, ok) = compile_and_run(
        "fn main() { let p = fs::temp_path(\"willow_t15_missing\"); println(fs::exists(p)); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "false\n");
}

#[test]
fn fs_16_wrong_arg_count() {
    let (ok, stderr) =
        compile_with_compiler_env("import std::fs;\nfn main() { fs::read_to_string(); }", &[]);
    assert!(!ok);
    assert!(!stderr.is_empty());
}

#[test]
fn fs_17_wrong_arg_type() {
    let (ok, stderr) = compile_with_compiler_env(
        "import std::fs;\nfn main() { fs::read_to_string(42); }",
        &[],
    );
    assert!(!ok);
    assert!(!stderr.is_empty());
}

#[test]
fn fs_18_result_not_printable() {
    let (ok, stderr) = compile_with_compiler_env(
        "import std::fs;\nfn main() { println(fs::read_to_string(\"/tmp/x\")); }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("E1402"), "{stderr}");
}

#[test]
fn fs_19_large_contents() {
    let (out, ok) = compile_and_run(
        "import std::fs;\nfn main() { let p = fs::temp_path(\"willow_t19\"); let mut s = \"\"; let mut i = 0; while i < 1000 { s = s + \"0123456789\"; i = i + 1; } fs::write_string(p, s); match fs::read_to_string(p) { Ok(t) => println(t == s), Err(e) => println(false), } fs::remove_file(p); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn fs_20_two_files_independent() {
    let (out, ok) = compile_and_run(
        "import std::fs;\nfn main() { let a = fs::temp_path(\"willow_t20a\"); let b = fs::temp_path(\"willow_t20b\"); fs::write_string(a, \"A\"); fs::write_string(b, \"B\"); match fs::read_to_string(a) { Ok(t) => println(t), Err(e) => println(\"no\"), } match fs::read_to_string(b) { Ok(t) => println(t), Err(e) => println(\"no\"), } fs::remove_file(a); fs::remove_file(b); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "A\nB\n");
}

// Review fixes on std::fs dispatch (willow-2s3): 21 a USER module imported
// as `fs` must win over the builtin (the emit arm used to fire on the bare
// string name before user-module resolution — the user's exists() was
// silently replaced by willow_fs_exists); 22 `import std::fs as files;`
// resolves the alias to the builtin (used to pass import validation then die
// E0350 at the call); 23 aliased env module works the same way; 24 aliases
// are normalized independently inside imported module bodies.

#[test]
fn fs_21_user_module_named_fs_wins() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import mine as fs;\nfn main() { println(fs::exists(\"/definitely/not/there\")); }\n",
            ),
            (
                "mine.wi",
                "module mine;\npub fn exists(p: String) -> bool { return true; }\n",
            ),
        ],
        "main.wi",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n", "user module must shadow the builtin fs");
}

#[test]
fn fs_22_std_fs_alias() {
    let (out, ok) = compile_and_run(
        "import std::fs as files;\nfn main() { let p = files::temp_path(\"willow_t22_missing\"); println(files::exists(p)); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "false\n");
}

#[test]
fn fs_23_std_env_alias() {
    let (out, ok) = compile_and_run(
        "import std::env as environment;\nfn main() { println(environment::args_len()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n");
}

#[test]
fn fs_24_std_fs_alias_inside_imported_module() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "main.wi",
                "import helper;\nfn main() { println(helper::missing()); }\n",
            ),
            (
                "helper.wi",
                "module helper;\nimport std::fs as files;\npub fn missing() -> bool { let p = files::temp_path(\"willow_t24_missing\"); return files::exists(p); }\n",
            ),
        ],
        "main.wi",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "false\n");
}

#[test]
fn fs_25_async_blocking_pool_roundtrip() {
    let (out, ok) = compile_and_run(
        r#"
import std::fs;

async fn main() {
    let path = fs::temp_path("willow_t25");
    match await fs::write_string_async(path, "pool") {
        Ok(v) => println("written"),
        Err(e) => println("write-error"),
    }
    match await fs::read_to_string_async(path) {
        Ok(text) => println(text),
        Err(e) => println("read-error"),
    }
    println(await fs::exists_async(path));
    match await fs::remove_file_async(path) {
        Ok(v) => println("removed"),
        Err(e) => println("remove-error"),
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "written\npool\ntrue\nremoved\n");
}

#[test]
fn fs_26_awaiting_sync_api_has_async_migration_diagnostic() {
    assert_compile_error_contains(
        "import std::fs;\nasync fn main() { let value = await fs::read_to_string(\"x\"); }",
        &[
            "error[E0803]",
            "synchronous filesystem operations cannot be awaited",
            "await fs::read_to_string_async(...)",
            "blocking pool",
        ],
    );
}

#[test]
fn fs_27_awaiting_sync_api_through_alias_preserves_alias_in_help() {
    assert_compile_error_contains(
        "import std::fs as files;\nasync fn main() { let value = await files::exists(\"x\"); }",
        &["error[E0803]", "await files::exists_async(...)"],
    );
}

#[test]
fn fs_28_sync_compatibility_and_async_api_coexist() {
    let (out, ok) = compile_and_run(
        r#"
import std::fs;

async fn main() {
    let path = fs::temp_path("willow_t28_missing");
    println(fs::exists(path));
    println(await fs::exists_async(path));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "false\nfalse\n");
}

#[test]
fn fs_29_async_burst_over_blocking_queue_capacity_all_complete() {
    // Overload policy (willow-9tls.6): a bounded pool (1 thread, 2 queued
    // jobs) receives 40 `*_async` reads of a 128 KiB file at once, so the
    // queue is full long before the burst is spawned. None fails or is
    // dropped: over-capacity Tasks wait for a slot and every one completes
    // with its real result, and a cancelled waiter is simply cancelled.
    // Admission order is covered by the runtime unit tests; this asserts
    // only results. (Traced with WILLOW_SCHED_TRACE=1 this run records
    // dozens of `blocking_queue_full` events; a 1-byte `exists_async` burst
    // never filled the queue because each job finished before the next
    // spawn.)
    let (out, ok) = compile_and_run_with_env(
        r#"
import std::fs;
import std::collections::Array;

async fn main() {
    let path = fs::temp_path("willow_t29_big");
    let mut payload = "x";
    let mut d = 0;
    while d < 17 { payload = payload + payload; d = d + 1; }
    fs::write_string(path, payload);
    let tasks: Array<Task<Result<String, IoError>>> = [];
    let mut i = 0;
    while i < 40 { tasks.push(fs::read_to_string_async(path)); i = i + 1; }
    let token = CancellationToken::new();
    let doomed = token.attach(fs::read_to_string_async(path));
    token.cancel();
    let mut mismatches = 0;
    let mut k = 0;
    while k < tasks.len() {
        match await tasks[k] {
            Ok(text) => { if text != payload { mismatches = mismatches + 1; } }
            Err(e) => { mismatches = mismatches + 1; }
        }
        k = k + 1;
    }
    println(mismatches);
    match await doomed.result() { Ok(v) => println("completed"), Err(Cancelled) => println("cancelled"), }
    fs::remove_file(path);
}
"#,
        &[
            ("WILLOW_BLOCKING_THREADS", "1"),
            ("WILLOW_BLOCKING_QUEUE", "2"),
        ],
    );
    assert!(ok, "{out}");
    assert!(
        out == "0\ncancelled\n" || out == "0\ncompleted\n",
        "every burst Task must complete with its own result: {out}"
    );
}
