#[test]
fn test_parse_target() {
    // group:123456
    let (kind, id) = qq::parse_target("group:123456").unwrap();
    assert!(matches!(kind, qq::TargetKind::Group));
    assert_eq!(id, 123456);

    // private:789012
    let (kind, id) = qq::parse_target("private:789012").unwrap();
    assert!(matches!(kind, qq::TargetKind::Private));
    assert_eq!(id, 789012);

    // shorthand
    let (kind, id) = qq::parse_target("g:100").unwrap();
    assert!(matches!(kind, qq::TargetKind::Group));
    assert_eq!(id, 100);

    let (kind, id) = qq::parse_target("p:200").unwrap();
    assert!(matches!(kind, qq::TargetKind::Private));
    assert_eq!(id, 200);

    // invalid
    assert!(qq::parse_target("invalid").is_err());
    assert!(qq::parse_target("group:abc").is_err());
    assert!(qq::parse_target("unknown:123").is_err());
}
