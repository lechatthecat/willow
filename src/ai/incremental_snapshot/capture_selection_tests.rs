use super::*;

#[test]
fn constructor_capture_visits_each_sibling_once() {
    for count in [16, 64, 256] {
        let key = (UnitId::ENTRY, FunctionId::free("constructor_group"));
        let members: HashSet<_> = (0..count).map(|_| BodyId::fresh()).collect();
        let groups = HashMap::from([(key, members.clone())]);
        let roots: Vec<_> = (0..count)
            .map(|member| Root {
                item: 0,
                kind: RootKind::Constructor(member),
                span: Span::dummy(),
                key,
            })
            .collect();
        let visited: Vec<_> = constructor_capture_siblings(roots.iter(), &groups).collect();
        assert_eq!(visited.len(), count);
        assert_eq!(visited.into_iter().collect::<HashSet<_>>(), members);
        assert_eq!(
            constructor_capture_siblings(roots[..1].iter(), &groups).count(),
            count
        );
        assert_eq!(
            constructor_capture_siblings(roots[..0].iter(), &groups).count(),
            0
        );
    }
}
