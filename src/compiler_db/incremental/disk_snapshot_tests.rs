use super::*;

#[test]
fn disk_json_buffers_preserve_exact_bytes_without_numeric_arrays() {
    for text in [
        "null",
        "\"日本語 🦀\"",
        " {\"escaped\":\"\\n\\\"\\\\\",\"x\":[1,2]} ",
    ] {
        let original = CompactJson::from_bytes(text.as_bytes().to_vec());
        let encoded = serde_json::to_vec(&original).unwrap();
        let shape: Value = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(shape["bytes"].as_str(), Some(text));
        let decoded: CompactJson = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.bytes, original.bytes);
        assert_eq!(
            ResultFingerprint::bytes(&decoded.bytes),
            ResultFingerprint::bytes(&original.bytes)
        );
    }
    assert!(serde_json::from_str::<CompactJson>(r#"{"bytes":[0,255]}"#).is_err());
}

fn fixture() -> (SyntaxQueries, BodyId) {
    let mut queries = SyntaxQueries::default().candidate().unwrap();
    let path = Path::new("disk-entry.wi");
    queries
        .parse(path, FileId::ENTRY, "fn f() -> i64 { return 1; }")
        .unwrap();
    let body = BodyId::fresh();
    queries.register_body(UnitId::ENTRY, body, path, "Function:f");
    queries
        .publish_body(UnitId::ENTRY, body, Value::Null, vec![], vec![])
        .unwrap();
    (queries, body)
}

#[test]
fn disk_snapshot_typed_closure_reuses_and_invalidates() {
    let (queries, body) = fixture();
    let bytes = serde_json::to_vec(&queries.disk_snapshot().unwrap()).unwrap();
    for (source, expected) in [
        ("fn f() -> i64 { return 1; }", true),
        ("fn f() -> i64 { return 2; }", false),
    ] {
        let snapshot = serde_json::from_slice(&bytes).unwrap();
        let restored = SyntaxQueries::from_disk(snapshot).unwrap();
        let mut next = restored.candidate().unwrap();
        let path = Path::new("disk-entry.wi");
        next.parse(path, FileId::ENTRY, source).unwrap();
        assert_eq!(
            next.validate_body(UnitId::ENTRY, body, path, "Function:f", vec![])
                .unwrap(),
            expected
        );
    }
}

#[test]
fn disk_snapshot_retains_body_analysis_and_invalidates_changed_dependencies() {
    let (mut queries, body) = fixture();
    let nodes = [
        QueryNode::NormalizedBody(body),
        QueryNode::DirectEffects(body),
        QueryNode::DefiniteAssignment(body),
        QueryNode::AsyncBorrowReport(body),
        QueryNode::ResolvedReferences(body),
    ];
    let dependencies = vec![QueryNode::BodySyntax(
        PathBuf::from("disk-entry.wi"),
        "Function:f".into(),
    )];
    for node in &nodes {
        queries
            .publish_derived(node.clone(), serde_json::json!([42]), dependencies.clone())
            .unwrap();
    }
    let bytes = serde_json::to_vec(&queries.disk_snapshot().unwrap()).unwrap();
    for (source, retained) in [
        ("fn f() -> i64 { return 1; }", true),
        ("fn f() -> i64 { return 2; }", false),
        ("fn other() -> i64 { return 1; }", false),
    ] {
        let mut next = SyntaxQueries::from_disk(serde_json::from_slice(&bytes).unwrap())
            .unwrap()
            .candidate()
            .unwrap();
        next.parse(Path::new("disk-entry.wi"), FileId::ENTRY, source)
            .unwrap();
        for node in &nodes {
            let result = next
                .validate_derived_typed::<Vec<i32>>(node.clone(), dependencies.clone())
                .unwrap();
            assert_eq!(result, retained.then_some(vec![42]), "{node:?}: {source}");
        }
    }
}

#[test]
fn disk_snapshot_rejects_missing_cycle_timestamp_and_wrong_payload() {
    let (queries, _) = fixture();
    let pristine = serde_json::to_value(queries.disk_snapshot().unwrap()).unwrap();
    let mut missing = pristine.clone();
    missing["table"]["inputs"] = serde_json::json!([]);
    let mut timestamp = pristine.clone();
    timestamp["table"]["revision"] = serde_json::json!(0);
    let mut wrong = pristine.clone();
    wrong["table"]["memos"][0]["value"]["kind"] = serde_json::json!("Children");
    let mut cycle = pristine.clone();
    let node = cycle["table"]["memos"][0]["node"].clone();
    let revision = cycle["table"]["memos"][0]["changed_at"].clone();
    cycle["table"]["memos"][0]["dependencies"] = serde_json::json!([
        {"node":{"Query":node}, "observed_changed_at": revision}
    ]);
    for invalid in [missing, timestamp, wrong, cycle] {
        let snapshot = serde_json::from_value(invalid).unwrap();
        assert!(SyntaxQueries::from_disk(snapshot).is_err());
    }
}
