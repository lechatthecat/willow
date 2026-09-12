//! Shared construction policy for calls into the statically linked runtime.

use std::ops::{Deref, DerefMut};

use cranelift_codegen::ir::{FuncRef, Function};
use cranelift_module::{FuncId, Module};
use cranelift_object::{ObjectBuilder, ObjectModule, ObjectProduct};
use target_lexicon::{Architecture, BinaryFormat, OperatingSystem, Triple};

/// Runtime imports are resolved into the same native executable as generated
/// functions. On supported native targets they use the same small-code-model
/// reachability assumption as direct calls between generated functions.
fn runtime_calls_are_colocated(triple: &Triple) -> bool {
    matches!(
        (
            triple.operating_system,
            triple.binary_format,
            triple.architecture
        ),
        (
            OperatingSystem::Linux,
            BinaryFormat::Elf,
            Architecture::X86_64 | Architecture::Aarch64(_)
        ) | (
            OperatingSystem::Darwin(_),
            BinaryFormat::Macho,
            Architecture::X86_64 | Architecture::Aarch64(_)
        ) | (
            OperatingSystem::Windows,
            BinaryFormat::Coff,
            Architecture::X86_64
        )
    )
}

pub(super) struct RuntimeObjectModule {
    inner: ObjectModule,
    colocate_runtime: bool,
}

impl RuntimeObjectModule {
    pub(super) fn new(builder: ObjectBuilder) -> Self {
        let inner = ObjectModule::new(builder);
        let colocate_runtime = runtime_calls_are_colocated(inner.isa().triple());
        Self {
            inner,
            colocate_runtime,
        }
    }

    pub(super) fn declare_func_in_func(&mut self, id: FuncId, function: &mut Function) -> FuncRef {
        let reference = self.inner.declare_func_in_func(id, function);
        if self.colocate_runtime
            && self
                .inner
                .declarations()
                .get_function_decl(id)
                .name
                .as_deref()
                .is_some_and(|name| crate::backend::abi::runtime_symbol(name).is_some())
        {
            function.dfg.ext_funcs[reference].colocated = true;
        }
        reference
    }

    pub(super) fn finish(self) -> ObjectProduct {
        self.inner.finish()
    }
}

impl Deref for RuntimeObjectModule {
    type Target = ObjectModule;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for RuntimeObjectModule {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cranelift_codegen::settings;
    use cranelift_module::Linkage;

    #[test]
    fn native_target_policy_preserves_unknown_target_fallback() {
        for triple in [
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
            "x86_64-pc-windows-msvc",
        ] {
            assert!(
                runtime_calls_are_colocated(&triple.parse().unwrap()),
                "{triple}"
            );
        }
        for triple in [
            "aarch64-pc-windows-msvc",
            "x86_64-unknown-freebsd",
            "wasm32-unknown-unknown",
        ] {
            assert!(
                !runtime_calls_are_colocated(&triple.parse().unwrap()),
                "{triple}"
            );
        }
    }

    #[test]
    fn exact_runtime_imports_use_shared_policy_without_changing_linkage() {
        let isa = cranelift_native::builder()
            .unwrap()
            .finish(settings::Flags::new(settings::builder()))
            .unwrap();
        let builder = ObjectBuilder::new(
            isa,
            "runtime-call-test",
            cranelift_module::default_libcall_names(),
        )
        .unwrap();
        let mut module = RuntimeObjectModule::new(builder);
        let signature = module.make_signature();
        let runtime = module
            .declare_function("willow_panic_depth", Linkage::Import, &signature)
            .unwrap();
        let unrelated = module
            .declare_function("willow_unknown_extension", Linkage::Import, &signature)
            .unwrap();
        let mut function = Function::new();
        module.colocate_runtime = true;
        let reference = module.declare_func_in_func(runtime, &mut function);
        assert!(function.dfg.ext_funcs[reference].colocated);
        assert_eq!(
            module.declarations().get_function_decl(runtime).linkage,
            Linkage::Import
        );
        let reference = module.declare_func_in_func(unrelated, &mut function);
        assert!(!function.dfg.ext_funcs[reference].colocated);
        module.colocate_runtime = false;
        let mut fallback = Function::new();
        let reference = module.declare_func_in_func(runtime, &mut fallback);
        assert!(!fallback.dfg.ext_funcs[reference].colocated);
        // Imported runtime functions do not need definitions in this object.
        let _ = module.finish();
    }
}
