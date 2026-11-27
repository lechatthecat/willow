use super::*;

#[test]
fn deep_module_signature_resolution_includes_closures() {
    std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(|| {
            let mut ty = Type::Named("Item".into());
            for i in 0..50_000 {
                ty = if i % 2 == 0 {
                    Type::Array(Box::new(ty))
                } else {
                    Type::Closure(vec![], Box::new(ty))
                };
            }
            let qualified = qualify_local_type(&ty, "local", &HashSet::from(["Item".into()]));
            let renamed = rename_module_prefix(
                &qualified,
                &HashMap::from([("local".into(), "canonical".into())]),
            );
            let imported = rename_imported_type(
                &renamed,
                &HashMap::from([("canonical::Item".into(), "Resolved".into())]),
            );
            assert_eq!(imported, ty.map_names(|_| "Resolved".to_string()));
        })
        .unwrap()
        .join()
        .unwrap();
}
