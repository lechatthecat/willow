//! Shared runtime import policy for the statically linked executable backend.
use cranelift_codegen::ir::{FuncRef, Function, Signature};
use cranelift_module::{FuncId, Linkage, Module, ModuleResult};
use cranelift_object::{ObjectBuilder, ObjectModule, ObjectProduct};
use std::ops::{Deref, DerefMut};
use target_lexicon::{
    Aarch64Architecture, Architecture, BinaryFormat, Environment, OperatingSystem, Triple,
};

/// Keep the inner module private: all lowering paths must use the same FuncRef
/// construction policy. Runtime membership is indexed by FuncId, not searched
/// by symbol name at every call site.
pub(super) struct RuntimeObjectModule {
    inner: ObjectModule,
    runtime: Vec<bool>,
    direct: bool,
}

/// These native executable link paths statically link the runtime archive.
/// x86-64 uses signed rel32; AArch64 uses CALL26 with linker veneers where
/// supported. The final linker must diagnose an unrepresentable displacement;
/// this is the same bounded-image contract as direct generated-function calls.
/// Do not extend this allowlist to arbitrary dynamic/import-library targets.
pub fn supports_direct_runtime_calls(triple: &Triple) -> bool {
    let architecture = matches!(
        triple.architecture,
        Architecture::X86_64 | Architecture::Aarch64(Aarch64Architecture::Aarch64)
    );
    architecture
        && matches!(
            (
                triple.operating_system,
                triple.binary_format,
                triple.environment
            ),
            (
                OperatingSystem::Linux,
                BinaryFormat::Elf,
                Environment::Gnu | Environment::Musl
            ) | (
                OperatingSystem::Darwin(_) | OperatingSystem::MacOSX(_),
                BinaryFormat::Macho,
                _
            ) | (
                OperatingSystem::Windows,
                BinaryFormat::Coff,
                Environment::Msvc
            )
        )
}

impl RuntimeObjectModule {
    pub(super) fn new(builder: ObjectBuilder, static_runtime: bool) -> Self {
        let inner = ObjectModule::new(builder);
        let direct = static_runtime && supports_direct_runtime_calls(inner.isa().triple());
        Self {
            inner,
            runtime: Vec::new(),
            direct,
        }
    }

    /// Only the ABI-table declaration loop registers runtime imports here.
    pub(super) fn declare_runtime_function(
        &mut self,
        name: &str,
        signature: &Signature,
    ) -> ModuleResult<FuncId> {
        let id = self
            .inner
            .declare_function(name, Linkage::Import, signature)?;
        let index = id.as_u32() as usize;
        if self.runtime.len() <= index {
            self.runtime.resize(index + 1, false);
        }
        self.runtime[index] = true;
        Ok(id)
    }

    pub(super) fn declare_func_in_func(&mut self, id: FuncId, function: &mut Function) -> FuncRef {
        let reference = self.inner.declare_func_in_func(id, function);
        if self.direct
            && self
                .runtime
                .get(id.as_u32() as usize)
                .copied()
                .unwrap_or(false)
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

    #[test]
    fn runtime_target_policy() {
        for target in [
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "x86_64-unknown-linux-musl",
            "aarch64-unknown-linux-musl",
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
            "x86_64-pc-windows-msvc",
            "aarch64-pc-windows-msvc",
        ] {
            assert!(
                supports_direct_runtime_calls(&target.parse().unwrap()),
                "{target}"
            );
        }
        for target in [
            "x86_64-pc-windows-gnu",
            "x86_64-unknown-freebsd",
            "riscv64gc-unknown-linux-gnu",
            "s390x-unknown-linux-gnu",
            "i686-unknown-linux-gnu",
            "aarch64_be-unknown-linux-gnu",
            "aarch64-apple-ios",
            "x86_64-unknown-none",
        ] {
            assert!(
                !supports_direct_runtime_calls(&target.parse().unwrap()),
                "{target}"
            );
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn fallback_targets_emit_absolute_import_relocations() {
        use cranelift_codegen::ir::{InstBuilder, UserFuncName};
        use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
        use object::{Object, ObjectSection, ObjectSymbol, RelocationTarget};
        for (target, expected) in [
            (
                "x86_64-pc-windows-gnu",
                object::RelocationFlags::Coff {
                    typ: object::pe::IMAGE_REL_AMD64_ADDR64,
                },
            ),
            (
                "x86_64-unknown-freebsd",
                object::RelocationFlags::Elf {
                    r_type: object::elf::R_X86_64_64,
                },
            ),
        ] {
            let isa = cranelift_codegen::isa::lookup(target.parse().unwrap())
                .unwrap()
                .finish(settings::Flags::new(settings::builder()))
                .unwrap();
            let mut module = RuntimeObjectModule::new(
                ObjectBuilder::new(
                    isa,
                    "fallback_target",
                    cranelift_module::default_libcall_names(),
                )
                .unwrap(),
                true,
            );
            assert!(!module.direct, "{target}");
            let signature = module.make_signature();
            let runtime = module
                .declare_runtime_function("runtime_probe", &signature)
                .unwrap();
            let caller = module
                .declare_function("caller", Linkage::Export, &signature)
                .unwrap();
            let mut context = module.make_context();
            context.func.signature = signature;
            context.func.name = UserFuncName::user(0, caller.as_u32());
            let reference = module.declare_func_in_func(runtime, &mut context.func);
            assert!(!context.func.dfg.ext_funcs[reference].colocated);
            let mut builder_context = FunctionBuilderContext::new();
            let mut builder = FunctionBuilder::new(&mut context.func, &mut builder_context);
            let block = builder.create_block();
            builder.switch_to_block(block);
            builder.ins().call(reference, &[]);
            builder.ins().return_(&[]);
            builder.seal_all_blocks();
            builder.finalize(module.target_config());
            module.define_function(caller, &mut context).unwrap();
            let bytes = module.finish().emit().unwrap();
            let file = object::File::parse(bytes.as_slice()).unwrap();
            let mut count = 0;
            for section in file.sections() {
                for (_, relocation) in section.relocations() {
                    if let RelocationTarget::Symbol(index) = relocation.target()
                        && file.symbol_by_index(index).unwrap().name().unwrap() == "runtime_probe"
                    {
                        assert_eq!(relocation.flags(), expected, "{target}");
                        count += 1;
                    }
                }
            }
            assert_eq!(count, 1, "{target}");
        }
    }

    #[test]
    fn custom_runtime_link_model_disables_direct_calls() {
        let isa = cranelift_native::builder()
            .unwrap()
            .finish(settings::Flags::new(settings::builder()))
            .unwrap();
        let module = RuntimeObjectModule::new(
            ObjectBuilder::new(
                isa,
                "custom_runtime",
                cranelift_module::default_libcall_names(),
            )
            .unwrap(),
            false,
        );
        assert!(!module.direct);
    }

    #[test]
    fn runtime_membership_and_fallback() {
        let isa = cranelift_native::builder()
            .unwrap()
            .finish(settings::Flags::new(settings::builder()))
            .unwrap();
        let mut module = RuntimeObjectModule::new(
            ObjectBuilder::new(
                isa,
                "runtime_policy",
                cranelift_module::default_libcall_names(),
            )
            .unwrap(),
            true,
        );
        let signature = module.make_signature();
        let unknown = module
            .declare_function("willow_unknown", Linkage::Import, &signature)
            .unwrap();
        let runtime = module
            .declare_runtime_function("willow_print_int", &signature)
            .unwrap();
        let local = module
            .declare_function("local", Linkage::Local, &signature)
            .unwrap();
        for direct in [false, true] {
            module.direct = direct;
            for count in [1, 8, 64, 512] {
                let mut function = Function::new();
                for _ in 0..count {
                    for (id, expected) in [(runtime, direct), (unknown, false), (local, true)] {
                        let reference = module.declare_func_in_func(id, &mut function);
                        assert_eq!(function.dfg.ext_funcs[reference].colocated, expected);
                    }
                }
                assert_eq!(function.dfg.ext_funcs.len(), 3 * count);
            }
        }
        assert_eq!(
            module.declarations().get_function_decl(runtime).linkage,
            Linkage::Import
        );
    }
}
