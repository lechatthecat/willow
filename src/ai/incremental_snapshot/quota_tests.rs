use super::*;
use serde_json::{Value, json};

// Independent oracle for the previous JSON-boundary accounting algorithm.
fn legacy_bound(value: &impl Serialize) -> usize {
    fn slack(value: &Value) -> usize {
        match value {
            Value::Object(fields) => fields
                .iter()
                .map(|(key, value)| {
                    if matches!(key.as_str(), "start" | "end") && value.is_u64() {
                        20usize.saturating_sub(value.to_string().len())
                    } else {
                        slack(value)
                    }
                })
                .sum(),
            Value::Array(values) => values.iter().map(slack).sum(),
            _ => 0,
        }
    }
    let value = serde_json::to_value(value).unwrap();
    serde_json::to_vec(&value).unwrap().len() + slack(&value)
}

fn fixture(n: usize, start: usize, end: usize) -> Snapshot {
    let location = Location {
        path: "test.wi".into(),
        start,
        end,
        display: None,
    };
    let span = Span::new(start, end, 1, 1);
    let function: Function = serde_json::from_value(json!({
        "identity":null,"id":"f","module":"test","name":"f",
        "locations":[location],"synthetic":false,"fingerprint":"f",
        "body_fingerprint":"body","callees":[],"runtime_effects":1,
        "unknown":false,"unresolved":[]
    }))
    .unwrap();
    let mut semantic = SemanticFacts::default();
    let symbol = serde_json::from_value(json!({
        "id":"f","name":"f","kind":"function","location":location,"ty":null
    }))
    .unwrap();
    semantic.symbols = vec![symbol; n];
    let reference = serde_json::from_value(json!({
        "target":"f","location":location,"role":"call"
    }))
    .unwrap();
    semantic.references = vec![reference; n];
    let expression = serde_json::from_value(json!({
        "function":"f","location":location,"ty":null,"target":null,
        "operation":"call:f","fingerprint":"expression","flow":0,"node":0,
        "occurrences":[]
    }))
    .unwrap();
    semantic.expressions = vec![expression; n];
    let flow = serde_json::from_value(json!({
        "function":"f","nodes":[{"location":location,"successors":[],"loop_header":false}],
        "entry":0,"complete":true
    }))
    .unwrap();
    semantic.flows = vec![flow; n];
    let lock_type: Type = Type::I64;
    let located = serde_json::from_value(json!({
        "effect":1,"witness":{"kind":"wait","cause":{
            "location":location,"operation":"await",
            "lock":{"mode":"lock read","target":location,"type":lock_type}
        }},"paths":{"normal":{"may_allocate":"none"},
            "panic":{"may_allocate":"may"},"scope":"operation","location":location}
    }))
    .unwrap();
    let captured = serde_json::from_value(json!({
        "effect":1,"witness":{"kind":"wait","cause":{
            "span":span,"operation":"await",
            "lock":{"mode":"lock","target":span}
        }}
    }))
    .unwrap();
    semantic.witnesses.insert(
        "f".into(),
        [located, captured]
            .into_iter()
            .cycle()
            .take(n * 2)
            .collect(),
    );
    semantic.effect_calls.insert(
        "f".into(),
        vec![
            semantic::EffectCall {
                target: "f".into(),
                location: location.clone()
            };
            n
        ],
    );
    semantic.unresolved_calls.insert(
        "f".into(),
        vec![
            semantic::UnresolvedCall {
                operation: super::super::operation::Operation::IndirectCall,
                location: location.clone()
            };
            n
        ],
    );
    let mut bridge = crate::rust_bridge_ir::RustBridgeSymbol::new(
        "foreign".into(),
        vec![],
        crate::rust_bridge_ir::Scalar::Void,
    );
    bridge.assumption_span = Some(span);
    semantic.rust_bridges = vec![bridge.clone(); n];
    semantic.interop.bridges = vec![
        super::super::interop::Bridge {
            function: "foreign".into(),
            identity: None,
            declaration: vec![location.clone()],
            declaration_text: String::new(),
            symbol: bridge,
        };
        n
    ];
    semantic.interop.crates = vec![
        super::super::interop::RustCrateIdentity {
            name: "opaque".into(),
            version: None,
            source: super::super::interop::RustCrateSource::Other(json!({
                "start":start,"end":end,"nested":[{"start":-1,"end":true},
                    {"start":{"end":999},"end":[{"start":0},{"end":null}]}],
                "unrecognized":"retained"
            })),
        };
        n
    ];
    // These live-only fields must not reserve bytes absent from serialization.
    semantic
        .rename_scopes
        .insert("test.wi".into(), vec![(start, end); n]);
    let mut snapshot = Snapshot {
        edit_context: None,
        version: 1,
        compiler: String::new(),
        compatibility: String::new(),
        workspace: String::new(),
        revision: String::new(),
        sources: BTreeMap::new(),
        functions: vec![function; n],
        semantic,
    };
    for function in &mut snapshot.functions {
        function.body_location = Some(location.clone());
        function.rename_calls = vec![location.clone()];
        function.rename_imports = vec![location.clone()];
    }
    snapshot
}

#[test]
fn typed_snapshot_quota_matches_wire_bound_and_scales() {
    for (start, end) in [(0, 0), (9, 10), (99, 100), (usize::MAX - 1, usize::MAX)] {
        let single_slack = snapshot_slack(&fixture(1, start, end));
        for n in [1, 8, 64] {
            let snapshot = fixture(n, start, end);
            assert_eq!(snapshot_slack(&snapshot), single_slack * n);
            assert_eq!(
                bounded_encoded(&snapshot, snapshot_slack(&snapshot)).unwrap(),
                legacy_bound(&snapshot)
            );
            let projection = Projection {
                anchor: None,
                key: None,
                root: None,
                order: 0,
                order_path: vec![],
                function: snapshot.functions[0].clone(),
                semantic: snapshot.semantic,
            };
            assert_eq!(
                encoded(&projection).unwrap(),
                legacy_bound(&(&projection.function, &projection.semantic))
            );
        }
    }
}

#[test]
fn typed_snapshot_relocation_preserves_all_witness_coordinates() {
    let snapshot = fixture(1, 2, 3);
    let mut projection = Projection {
        anchor: None,
        key: None,
        root: None,
        order: 0,
        order_path: vec![],
        function: snapshot.functions[0].clone(),
        semantic: snapshot.semantic,
    };
    let prior_bound = encoded(&projection).unwrap();
    relocate_projection(
        &mut projection,
        &RelocationBatch::new(&[VerificationRelocation {
            path: "test.wi".into(),
            old_start: 0,
            old_end: 10,
            new_start: 100,
            new_end: 110,
        }]),
    );
    let fact = &projection.semantic.witnesses["f"][0];
    let cause = fact.witness.cause.as_ref().unwrap();
    assert_eq!(cause.location().map(|l| (l.start, l.end)), Some((102, 103)));
    let target = cause.lock.as_ref().unwrap().target.as_ref().unwrap();
    let super::super::evidence::SourcePoint::Located(target) = target else {
        panic!("located lock target");
    };
    assert_eq!((target.start, target.end), (102, 103));
    let path = fact
        .paths
        .as_ref()
        .unwrap()
        .location
        .as_ref()
        .unwrap()
        .as_ref()
        .unwrap();
    assert_eq!((path.start, path.end), (102, 103));
    // Captured spans have no path and were never relocated by the wire visitor.
    let captured = &projection.semantic.witnesses["f"][1];
    let super::super::evidence::CauseSource::Captured { span } =
        captured.witness.cause.as_ref().unwrap().source
    else {
        panic!("captured span");
    };
    assert_eq!((span.start, span.end), (2, 3));
    assert_eq!(encoded(&projection).unwrap(), prior_bound);
    assert_eq!(
        encoded(&projection).unwrap(),
        legacy_bound(&(&projection.function, &projection.semantic))
    );
}
