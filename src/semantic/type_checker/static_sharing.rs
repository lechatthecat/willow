//! Deferred static sharing obligations, evaluated on the build-wide task graph.
use super::*;

impl TypeChecker {
    /// Publish deferred reads alongside the existing closed-build task inventory.
    /// Standalone semantic clients use the same graph, without changing their
    /// independent preemption-diagnostic policy.
    pub(super) fn record_task_context(
        &mut self,
        program: &Program,
        graph: &crate::semantic::call_graph::CallGraph,
        effects: &std::sync::Arc<crate::compiler_db::effects::UnitEffects>,
        imports: &HashMap<String, String>,
    ) {
        if let Some((queries, unit)) = &self.effect_queries {
            queries.static_initializers.record_preemption(
                *unit,
                program,
                effects,
                &self.indirect_lock,
                std::mem::take(&mut self.unsafe_static_accesses),
            );
        } else if !self.unsafe_static_accesses.is_empty() {
            // Standalone semantic clients use the same closed-build reachability.
            let queries = crate::compiler_db::effects::EffectQueries::default();
            let unit = crate::module::UnitId::ENTRY;
            queries.static_initializers.record(
                unit,
                program,
                graph,
                &self.static_reads,
                imports,
                None,
            );
            queries.static_initializers.record_preemption(
                unit,
                program,
                effects,
                &self.indirect_lock,
                std::mem::take(&mut self.unsafe_static_accesses),
            );
            for diagnostic in queries.static_initializers.check_preemption(
                &[unit],
                &queries,
                self.task_sync_preemption,
            ) {
                if diagnostic.code == ErrorCode::E2402 {
                    self.push(diagnostic);
                }
            }
        }
    }

    pub(super) fn record_static_sharing(
        &mut self,
        owner: &str,
        field: &str,
        info: &StaticPropInfo,
        access: Span,
    ) {
        // Replaceable slots retain the independent single-worker runtime policy.
        if !self.enforce_send_sync || info.is_mut {
            return;
        }
        let Some(caller) = self.local.current_effect_callable else {
            return;
        };
        // Specializations share source spans, but can have different field types.
        let key = (TypeId::from_source_name(owner), info.declaration_span);
        let sync = if let Some(sync) = self.static_sync.get(&key) {
            *sync
        } else {
            let mut markers = std::mem::take(&mut self.static_markers);
            let sync = self.is_sync_cached(&info.ty, &mut markers);
            self.static_markers = markers;
            self.static_sync.insert(key, sync);
            sync
        };
        if sync {
            return;
        }
        self.unsafe_static_accesses.push((
            caller,
            Diagnostic::new(
                Severity::Error,
                ErrorCode::E2402,
                format!(
                    "cannot share static `{}::{field}` with a task: `{}` is not `Sync`",
                    self.diagnostic_class_name(owner),
                    self.diagnostic_type_name(&info.ty),
                ),
            )
            .with_label(Label::primary(access, "task-reachable access to shared static storage"))
            .with_label(Label::secondary(info.declaration_span, "non-Sync static declared here"))
            .with_note("an immutable static slot does not make the object it refers to immutable; synchronous helpers and callbacks can also run inside tasks")
            .with_help("use an appropriate Mutex/RwLock, atomic, frozen collection, or structurally immutable value"),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_task_sharing_standalone_matches_closed_build() {
        let source = "import std::collections::Array; class G { pub static xs: Array<i64> = [1]; } fn helper() { G::xs.push(2); } async fn main() { helper(); }";
        let (program, errors) =
            crate::parser::Parser::new(crate::lexer::Lexer::new(source).tokenize().unwrap())
                .parse();
        assert!(errors.is_empty(), "{errors:?}");
        let mut checker = TypeChecker::new();
        checker.set_enforce_send_sync(true);
        checker.check_program(&program);
        assert!(
            checker
                .errors
                .iter()
                .any(|d| d.code == ErrorCode::E2402 && d.message.contains("cannot share static")),
            "{:?}",
            checker.errors
        );
    }
}
