use std::{collections::BTreeSet, path::Path};

#[test]
fn native_relay_fixture_manifest_lists_every_adapter_gate_fixture() {
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/native-relay/manifest.json"))
            .expect("fixture manifest should parse");

    assert_eq!(
        manifest["schema"], "switchback/native-relay-fixtures@1",
        "schema should stay stable for relay audit tooling"
    );
    assert_eq!(manifest["status"], "partial_capture");
    assert_eq!(
        manifest["gate"]["adapter_registry_fixture_backed"],
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
        let captures = item["captures"].as_object().expect("captures map");
        for (client, path) in captures {
            assert!(
                clients.iter().any(|known| known.as_str() == Some(client)),
                "capture client must be listed in clients: {item}"
            );
            let path = path.as_str().expect("capture path");
            assert!(
                !path.contains("..") && path.ends_with(".json"),
                "capture path should stay inside the fixture directory: {path}"
            );
            let full_path = Path::new("tests/fixtures/native-relay").join(path);
            let fixture = std::fs::read_to_string(full_path).expect("capture fixture exists");
            let fixture: serde_json::Value =
                serde_json::from_str(&fixture).expect("capture fixture parses");
            assert_eq!(
                fixture["schema"],
                "switchback/native-relay-sanitized-fixture@1"
            );
            assert_eq!(
                fixture["client"],
                serde_json::Value::String(client.to_string())
            );
            assert_eq!(fixture["fixture"], item["id"]);
            assert!(
                fixture["capture"].is_object(),
                "fixture should carry sanitized capture metadata: {fixture}"
            );
        }
    }

    let non_stream = manifest["required"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == "non_stream_request_response")
        .expect("non-stream fixture entry");
    assert_eq!(
        non_stream["captures"]["codex"],
        "codex/non_stream_request_response.json"
    );
    assert_eq!(
        non_stream["captures"]["claude-code"],
        "claude-code/non_stream_request_response.json"
    );

    let tool_result = manifest["required"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == "tool_call_and_tool_result")
        .expect("tool-call fixture entry");
    assert_eq!(
        tool_result["captures"]["codex"],
        "codex/tool_call_and_tool_result.json"
    );

    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/native-relay/codex/tool_call_and_tool_result.json"
    ))
    .expect("codex tool-call fixture parses");
    let requests = fixture["capture"]["json"]["http"]
        .as_array()
        .expect("http capture array");
    let second_request = requests
        .iter()
        .filter(|entry| entry["kind"] == "request")
        .nth(1)
        .expect("second request carries the tool result");
    let input = second_request["body"]["input"]
        .as_array()
        .expect("request input array");
    let call = input
        .iter()
        .find(|item| item["type"] == "function_call")
        .expect("function_call item");
    let result = input
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .expect("function_call_output item");
    assert_eq!(call["call_id"], result["call_id"]);
    assert!(
        result["output"]
            .as_str()
            .expect("text tool output")
            .contains("SWITCHBACK_TOOL_RESULT_OK"),
        "tool result should carry the captured command output: {result}"
    );
}
