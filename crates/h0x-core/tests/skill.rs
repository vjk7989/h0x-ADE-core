// The installable skill is the repo's stub; it points agents to `h0x-core --skill`.
#[test]
fn the_installable_stub_is_a_valid_skill() {
    let stub = include_str!("../../../skills/h0x-core/SKILL.md");
    let mut lines = stub.lines();
    assert_eq!(lines.next(), Some("---"));
    assert_eq!(lines.next(), Some("name: h0x-core"));
    let description = lines.next().unwrap().strip_prefix("description: ").unwrap();
    let description: String = serde_json::from_str(description).expect("quoted YAML description");
    assert!(!description.is_empty() && description.chars().count() < 1024);
    assert_eq!(lines.next(), Some("---"));
    assert!(stub.contains("h0x-core --skill"), "the stub points to the manual");
}
