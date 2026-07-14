use super::stale_selfci_export_bookmark;

#[test]
fn retains_bookmark_for_current_process() {
    let line = format!("selfci-export-base-{}-1: target", std::process::id());
    assert_eq!(stale_selfci_export_bookmark(&line), None);
}

#[test]
fn retains_malformed_or_unsafe_process_ids() {
    for pid in ["nonnumeric", "0", "-1", "2147483648"] {
        let line = format!("selfci-export-candidate-{pid}-1: target");
        assert_eq!(stale_selfci_export_bookmark(&line), None);
    }
}

#[test]
fn detects_bookmark_for_nonexistent_process() {
    let line = "selfci-export-base-2147483647-1: target";
    assert_eq!(
        stale_selfci_export_bookmark(line),
        Some("selfci-export-base-2147483647-1".to_string())
    );
}
