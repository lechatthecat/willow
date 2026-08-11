//! Capability probes for Cranelift's `stack_switch` instruction.
//!
//! These back the decision record at
//! `docs/decisions/0001-task-stack-switch-capability.md` (willow-38w.2.2, A2),
//! which is the input to A3's task-owned synchronous stack. The record makes
//! claims about what the pinned Cranelift can and cannot do; without probes,
//! those claims silently rot the next time the dependency is bumped.
//!
//! Each probe below states the claim it pins. If one starts failing after a
//! Cranelift upgrade, that is the signal to revisit the record — quite possibly
//! because a platform became newly supported, which is good news, not a
//! regression.
//!
//! Perspectives covered:
//!
//!  1. the `stack_switch_model` setting exists and defaults to `none`
//!  2. `basic` and `update_windows_tib` are both accepted as setting values
//!  3. an unknown model value is rejected
//!  4. under `basic`, `stack_switch` compiles on an x64 host and is refused on
//!     every other host — the record's central portability claim
//!  5. `stack_switch` does NOT compile under the default `none`, on any host
//!  6. `stack_switch` does NOT compile under `update_windows_tib`, on any host
//!  7. the instruction passes the IR verifier regardless of the model, so the
//!     model is purely a lowering-time gate
//!  8. `stack_switch` is absent from every non-x64 backend in the tree
//!  9. the x64 lowering is gated on the `Basic` model alone
//! 10. `UpdateWindowsTib` exists only as an ISLE type variant, with no rule
//! 11. the ControlContext layout the record documents is the one in the source
//! 12. the payload register is a System V argument register, not a Win64 one
//!
//! Plus an ignored reporter that regenerates the exact refusal text the record
//! quotes.

use std::path::{Path, PathBuf};

use cranelift_codegen::ir::{AbiParam, Function, InstBuilder, Signature, UserFuncName, types};
use cranelift_codegen::isa::TargetIsa;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::{Context, verifier};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

/// Build `fn(i64, i64, i64) -> i64` whose body is a single `stack_switch`.
///
/// The three parameters are the instruction's `store_context_ptr`,
/// `load_context_ptr`, and `in_payload0`; the result is `out_payload0`.
fn stack_switch_function(isa: &dyn TargetIsa) -> Function {
    let mut signature = Signature::new(isa.default_call_conv());
    for _ in 0..3 {
        signature.params.push(AbiParam::new(types::I64));
    }
    signature.returns.push(AbiParam::new(types::I64));

    let mut function = Function::with_name_signature(UserFuncName::default(), signature);
    let mut builder_context = FunctionBuilderContext::new();
    let mut builder = FunctionBuilder::new(&mut function, &mut builder_context);

    let block = builder.create_block();
    builder.append_block_params_for_function_params(block);
    builder.switch_to_block(block);
    builder.seal_block(block);

    let store_context_ptr = builder.block_params(block)[0];
    let load_context_ptr = builder.block_params(block)[1];
    let in_payload0 = builder.block_params(block)[2];
    let out_payload0 = builder
        .ins()
        .stack_switch(store_context_ptr, load_context_ptr, in_payload0);
    builder.ins().return_(&[out_payload0]);

    builder.finalize(isa.frontend_config());
    function
}

/// Outcome of asking the host backend to compile [`stack_switch_function`]
/// under a given `stack_switch_model`.
enum Lowering {
    /// Machine code was produced, with this many bytes in it.
    Compiled(usize),
    /// Cranelift refused, by `Err` or by panic. The string is its complaint.
    Refused(String),
}

fn lower_on_host(model: &str) -> Lowering {
    // Cranelift panics rather than returning `Err` when no lowering rule
    // matches, so the probe has to be prepared for either shape of refusal.
    let result = std::panic::catch_unwind(|| {
        let isa_builder = cranelift_native::builder().expect("host ISA");
        let mut flags = settings::builder();
        flags.set("stack_switch_model", model).expect("known model");
        let isa = isa_builder
            .finish(settings::Flags::new(flags))
            .expect("host ISA finish");

        let mut context = Context::for_function(stack_switch_function(isa.as_ref()));
        context
            .compile(isa.as_ref(), &mut Default::default())
            .map(|compiled| compiled.code_buffer().len())
            .map_err(|error| format!("{error:?}"))
    });

    match result {
        Ok(Ok(bytes)) => Lowering::Compiled(bytes),
        Ok(Err(message)) => Lowering::Refused(message),
        Err(payload) => {
            let message = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
                .unwrap_or_else(|| "<non-string panic payload>".to_string());
            Lowering::Refused(message)
        }
    }
}

/// x64 is the one architecture with a `stack_switch` implementation, so it is
/// the one architecture where lowering is expected to succeed.
fn host_is_x64() -> bool {
    cfg!(target_arch = "x86_64")
}

/// Opt-out for the probes that read the Cranelift source checkout, for builds
/// where no registry checkout exists (vendored sources, offline packaging).
const SKIP_SOURCE_PROBES: &str = "WILLOW_SKIP_CRANELIFT_SOURCE_PROBES";

fn source_probes_skipped() -> bool {
    std::env::var_os(SKIP_SOURCE_PROBES).is_some_and(|value| value == "1")
}

/// Locate the pinned `cranelift-codegen` source in the Cargo registry, so the
/// source-shape probes read the exact version this workspace builds against.
///
/// `None` means "not found", which callers treat as a failure rather than a
/// reason to skip — see [`cranelift_source_root`].
fn cranelift_codegen_src() -> Option<PathBuf> {
    let version = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.lock"))
        .ok()?
        .split("name = \"cranelift-codegen\"")
        .nth(1)?
        .lines()
        .find_map(|line| line.trim().strip_prefix("version = ").map(str::to_string))?
        .trim_matches('"')
        .to_string();

    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))?;

    let registry = home.join("registry").join("src");
    // One unreadable entry must not abort the search: registry roots routinely
    // hold directories from other toolchains and permission-restricted leftovers.
    for entry in std::fs::read_dir(registry).ok()? {
        let Ok(entry) = entry else {
            continue;
        };
        let candidate = entry.path().join(format!("cranelift-codegen-{version}"));
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    None
}

/// The pinned Cranelift source root, or `None` **only** when the source probes
/// are explicitly opted out of.
///
/// A missing checkout is a failure, not a skip. A probe that quietly returns
/// when it cannot find its input stops pinning the record, and a decision record
/// that nothing pins is exactly the failure mode these probes exist to prevent:
/// the suite would stay green through a Cranelift bump that invalidated it.
fn cranelift_source_root() -> Option<PathBuf> {
    if source_probes_skipped() {
        return None;
    }
    Some(cranelift_codegen_src().unwrap_or_else(|| {
        panic!(
            "could not locate the pinned cranelift-codegen source under the Cargo registry, so \
             the source-shape probes backing \
             docs/decisions/0001-task-stack-switch-capability.md cannot run. Run `cargo fetch`, \
             or set {SKIP_SOURCE_PROBES}=1 to skip them deliberately."
        )
    }))
}

/// Read one file out of the pinned Cranelift source. `None` only under the
/// opt-out; an unreadable file is a failure.
fn cranelift_source(relative: &str) -> Option<String> {
    let path = cranelift_source_root()?.join(relative);
    Some(
        std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display())),
    )
}

/// Perspective 1: the setting exists, and its default is `none` — so nothing in
/// Willow gets stack switching by accident. A3 has to opt in explicitly.
#[test]
fn stack_switch_model_defaults_to_none() {
    let flags = settings::Flags::new(settings::builder());
    assert!(
        flags.to_string().contains("stack_switch_model = \"none\""),
        "expected the default model to be `none`: {flags}"
    );
}

/// Perspective 2: both non-`none` models are accepted as *settings*. Accepting
/// the value says nothing about whether a backend implements it — see
/// perspectives 6 and 10.
#[test]
fn stack_switch_model_accepts_basic_and_update_windows_tib() {
    for model in ["none", "basic", "update_windows_tib"] {
        let mut flags = settings::builder();
        flags
            .set("stack_switch_model", model)
            .unwrap_or_else(|error| panic!("`{model}` should be a known model: {error}"));
    }
}

/// Perspective 3: the setting is a closed enum, so a typo in A3's flag plumbing
/// fails loudly at configuration time rather than silently disabling switching.
#[test]
fn stack_switch_model_rejects_unknown_values() {
    let mut flags = settings::builder();
    assert!(
        flags.set("stack_switch_model", "segmented").is_err(),
        "an unknown model must be rejected"
    );
}

/// Perspective 4: under `basic`, lowering succeeds on x64 and **fails
/// everywhere else**. Both halves are assertions; neither host skips.
///
/// The negative half is the more valuable one, and is the record's central
/// portability claim: on an Apple Silicon runner this test is what proves
/// Cranelift cannot express the transition on aarch64. Skipping on non-x64 would
/// mean the exact machine that most needs to hear the answer never asks.
#[test]
fn basic_model_lowers_on_x64_and_is_refused_on_every_other_architecture() {
    match (lower_on_host("basic"), host_is_x64()) {
        (Lowering::Compiled(bytes), true) => assert!(
            bytes > 0,
            "the basic model must emit real machine code, got an empty buffer"
        ),
        (Lowering::Refused(message), true) => {
            panic!("x64 must lower `stack_switch` under the basic model: {message}")
        }
        (Lowering::Compiled(bytes), false) => panic!(
            "a non-x64 host lowered `stack_switch` into {bytes} bytes — a backend gained \
             stack-switch support, so revisit \
             docs/decisions/0001-task-stack-switch-capability.md, which says x64 is the only one"
        ),
        (Lowering::Refused(_), false) => {}
    }
}

/// Perspective 5: under the default model there is no lowering rule on any
/// architecture, so the instruction cannot be reached by accident. On x64 that
/// is the `Basic` gate; elsewhere there is no rule at all. Either way: refused.
#[test]
fn stack_switch_does_not_lower_under_the_default_model() {
    if let Lowering::Compiled(bytes) = lower_on_host("none") {
        panic!("`stack_switch` must not lower under the default `none` model, got {bytes} bytes");
    }
}

/// Perspective 6: `update_windows_tib` is a *declared but unimplemented* model.
/// This is the single most load-bearing fact in the decision record: Windows
/// needs the TIB stack bounds updated for guard pages and `__chkstk` to behave,
/// the setting name exists, and selecting it still does not lower — on x64,
/// where the other model does lower, or anywhere else.
#[test]
fn stack_switch_does_not_lower_under_update_windows_tib() {
    if let Lowering::Compiled(bytes) = lower_on_host("update_windows_tib") {
        panic!(
            "`update_windows_tib` now lowers into {bytes} bytes — Cranelift gained Windows \
             stack-switch support, so revisit \
             docs/decisions/0001-task-stack-switch-capability.md"
        );
    }
}

/// Perspective 7: the IR verifier accepts `stack_switch` no matter the model, so
/// the model is purely a lowering-time gate. Nothing catches a misconfiguration
/// at IR-construction time; A3 must check the flag itself.
#[test]
fn stack_switch_passes_the_ir_verifier_regardless_of_model() {
    for model in ["none", "basic", "update_windows_tib"] {
        let isa_builder = cranelift_native::builder().expect("host ISA");
        let mut flags = settings::builder();
        flags.set("stack_switch_model", model).expect("known model");
        let isa = isa_builder
            .finish(settings::Flags::new(flags))
            .expect("host ISA finish");
        let function = stack_switch_function(isa.as_ref());
        assert!(
            verifier::verify_function(&function, isa.as_ref()).is_ok(),
            "`stack_switch` must verify under model `{model}`"
        );
    }
}

/// Perspective 8: no backend other than x64 has any `stack_switch` support at
/// all. This is why the record cannot name a mechanism for macOS on Apple
/// Silicon: arm64 has no lowering to select.
#[test]
fn only_the_x64_backend_mentions_stack_switch() {
    let Some(source) = cranelift_source_root() else {
        return;
    };
    let isa = source.join("src").join("isa");
    let entries = std::fs::read_dir(&isa)
        .unwrap_or_else(|error| panic!("failed to list {}: {error}", isa.display()));

    let mut checked = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let backend = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if backend == "x64" {
            continue;
        }
        checked.push(backend.clone());
        let mentions = walk_files(&path)
            .into_iter()
            .filter(|file| {
                std::fs::read_to_string(file)
                    .map(|text| text.contains("stack_switch"))
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(
            mentions, 0,
            "backend `{backend}` now mentions stack_switch — a new architecture may be \
             supported, so revisit docs/decisions/0001-task-stack-switch-capability.md"
        );
    }

    // Without this the probe passes vacuously if the layout of the crate ever
    // moves the backends elsewhere: zero directories inspected, zero mentions
    // found, green. aarch64 is named explicitly because it is the architecture
    // the record's macOS answer turns on.
    assert!(
        checked.iter().any(|backend| backend == "aarch64"),
        "expected an aarch64 backend directory under {}, found {checked:?}",
        isa.display()
    );
}

fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files
}

/// Perspective 9: the x64 lowering rule matches `Basic` and nothing else,
/// which is the source-level counterpart of perspectives 5 and 6.
#[test]
fn x64_lowering_is_gated_on_the_basic_model_only() {
    let Some(lower) = cranelift_source("src/isa/x64/lower.isle") else {
        return;
    };
    assert!(
        lower.contains("(if-let (StackSwitchModel.Basic) (stack_switch_model))"),
        "expected the x64 stack_switch rule to be gated on StackSwitchModel.Basic"
    );
    assert!(
        !lower.contains("StackSwitchModel.UpdateWindowsTib"),
        "the x64 backend now has an UpdateWindowsTib rule — revisit the decision record"
    );
}

/// Perspective 10: `UpdateWindowsTib` exists as a type variant only. Confirms
/// perspective 6's runtime observation has the cause the record claims, rather
/// than failing for some unrelated reason.
#[test]
fn update_windows_tib_is_a_type_variant_with_no_lowering_rule() {
    let Some(prelude) = cranelift_source("src/prelude_lower.isle") else {
        return;
    };
    assert!(
        prelude.contains("(type StackSwitchModel extern (enum (None) (Basic) (UpdateWindowsTib)))"),
        "expected UpdateWindowsTib to be declared in the StackSwitchModel enum"
    );
}

/// Perspective 11: the record documents a three-word ControlContext as
/// `{ sp, fp, ip }` at offsets 0/8/16. A3's runtime struct must match this
/// exactly — Cranelift performs no checking on these pointers.
#[test]
fn control_context_layout_is_sp_fp_ip_at_zero_eight_sixteen() {
    let Some(source) = cranelift_source("src/isa/x64/inst/stack_switch.rs") else {
        return;
    };
    for (field, offset) in [
        ("stack_pointer_offset", "0"),
        ("frame_pointer_offset", "8"),
        ("ip_offset", "16"),
    ] {
        assert!(
            source.contains(&format!("{field}: {offset},")),
            "expected `{field}: {offset}` in the ControlContext layout"
        );
    }
}

/// Perspective 12: the payload register is hardcoded to `rdi`. That is System V
/// argument 0; the Win64 equivalent is `rcx`. A trampoline reached by switching
/// to a fresh stack therefore has to read its argument from a register that the
/// Windows ABI does not use for arguments at all — a second, independent reason
/// the record does not select this mechanism for Windows.
#[test]
fn payload_register_is_the_system_v_first_argument() {
    let Some(source) = cranelift_source("src/isa/x64/inst/stack_switch.rs") else {
        return;
    };
    assert!(
        source.contains("pub fn payload_register() -> Reg {") && source.contains("regs::rdi()"),
        "expected the payload register to be rdi (System V argument 0)"
    );
}

/// Not a check — a reporter. Run it to regenerate the exact refusal text quoted
/// in the decision record after a Cranelift upgrade:
///
/// ```text
/// cargo test --test integration stack_switch_capability_report -- --ignored --nocapture
/// ```
#[test]
#[ignore = "reporting aid for the decision record, not a pass/fail check"]
fn stack_switch_capability_report() {
    for model in ["none", "basic", "update_windows_tib"] {
        match lower_on_host(model) {
            Lowering::Compiled(bytes) => println!("{model}: compiled, {bytes} bytes"),
            Lowering::Refused(message) => println!("{model}: refused -- {message}"),
        }
    }
}
