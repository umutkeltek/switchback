use std::collections::BTreeSet;

#[test]
fn native_relay_fixture_manifest_lists_every_adapter_gate_fixture() {
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/native-relay/manifest.json"))
            .expect("fixture manifest should parse");

    assert_eq!(
        manifest["schema"], "switchback/native-relay-fixtures@1",
        "schema should stay stable for relay audit tooling"
    );
    assert_eq!(manifest["status"], "pending_capture");
    assert_eq!(
        manifest["gate"]["adapter_registry_fail_closed"],
        serde_json::json!(true)
    );
    assert_eq!(manifest["gate"]["no_token_values"], serde_json::json!(true));

    let required = manifest["required"]
        .as_array()
        .expect("required fixture list")
        .iter()
        .map(|item| item["id"].as_str().expect("fixture id"))
        .collect::<BTreeSet<_>>();
    let expected = BTreeSet::from([
        "model_list",
        "non_stream_request_response",
        "stream_request_first_byte_and_finish",
        "tool_call_and_tool_result",
        "token_count",
        "expired_token_or_refresh_failure",
        "client_abort_before_first_byte",
        "client_abort_after_first_byte",
    ]);
    assert_eq!(required, expected);

    for item in manifest["required"].as_array().unwrap() {
        assert!(
            item["sanitized_fixture"].is_null(),
            "fixtures must start null until captured and scrubbed: {item}"
        );
        let clients = item["clients"].as_array().expect("clients");
        assert!(
            clients
                .iter()
                .any(|client| client.as_str() == Some("codex")),
            "fixture should name Codex coverage: {item}"
        );
        assert!(
            clients
                .iter()
                .any(|client| client.as_str() == Some("claude-code")),
            "fixture should name Claude Code coverage: {item}"
        );
    }
}
