use std::{collections::BTreeSet, fs, path::PathBuf};

use serde_json::{json, Value};
use txwatch_config::{AlertRule, AppConfig, Network};

fn schema() -> Value {
    serde_json::to_value(AppConfig::json_schema()).expect("schema should serialize to JSON")
}

fn required_fields(value: &Value) -> BTreeSet<&str> {
    value["required"]
        .as_array()
        .expect("schema should declare required fields")
        .iter()
        .map(|field| field.as_str().expect("required field should be a string"))
        .collect()
}

#[test]
fn schema_describes_required_config_fields_and_nested_contracts() {
    let schema = schema();
    assert_eq!(schema["$schema"], "http://json-schema.org/draft-07/schema#");
    assert_eq!(schema["title"], "AppConfig");
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(
        required_fields(&schema),
        BTreeSet::from(["contracts", "poll_interval_seconds"])
    );
    assert_eq!(
        schema["properties"]["poll_interval_seconds"]["type"],
        "integer"
    );
    assert_eq!(schema["properties"]["contracts"]["type"], "array");
    assert_eq!(
        schema["properties"]["contracts"]["items"]["$ref"],
        "#/definitions/WatchedContract"
    );

    let contract = &schema["definitions"]["WatchedContract"];
    assert_eq!(contract["additionalProperties"], false);
    assert_eq!(
        required_fields(contract),
        BTreeSet::from(["contract_id", "label", "network", "rules", "webhook_url"])
    );
    assert_eq!(contract["properties"]["rules"]["type"], "array");
    assert_eq!(
        contract["properties"]["rules"]["items"]["$ref"],
        "#/definitions/AlertRule"
    );
}

#[test]
fn schema_preserves_optional_fields_defaults_and_skipped_fields() {
    let schema = schema();
    assert!(!required_fields(&schema).contains("cursor_file"));
    assert!(!required_fields(&schema).contains("webhook_timeout_seconds"));
    assert_eq!(
        schema["properties"]["webhook_timeout_seconds"]["default"],
        15
    );
    assert_eq!(
        schema["properties"]["horizon_startup_check"]["default"],
        false
    );

    let contract = &schema["definitions"]["WatchedContract"];
    let properties = contract["properties"]
        .as_object()
        .expect("contract should have properties");
    assert!(!properties.contains_key("test_horizon_base_url"));
    assert!(properties.contains_key("horizon_url"));
    assert!(properties.contains_key("webhook_secret"));
    assert!(!required_fields(contract).contains("horizon_url"));
    assert!(!required_fields(contract).contains("webhook_secret"));
    assert_eq!(properties["enabled"]["default"], true);
    assert_eq!(properties["webhook_compress"]["default"], false);
    assert_eq!(properties["webhook_rule_filter"]["default"], json!([]));
}

#[test]
fn schema_uses_serde_network_names() {
    let schema = schema();
    let values = schema["definitions"]["Network"]["enum"]
        .as_array()
        .expect("network should be an enum");
    assert_eq!(
        values,
        &vec![json!("mainnet"), json!("testnet"), json!("futurenet")]
    );
    for value in values {
        serde_json::from_value::<Network>(value.clone())
            .expect("schema network values should deserialize");
    }
}

#[test]
fn schema_describes_tagged_rule_variants_and_their_required_parameters() {
    let schema = schema();
    let rule = &schema["definitions"]["AlertRule"];
    let variants = rule
        .get("oneOf")
        .or_else(|| rule.get("anyOf"))
        .and_then(Value::as_array)
        .expect("rules should be described by tagged alternatives");
    let cases = [
        (json!({"type": "AnyTransaction"}), vec!["type"]),
        (json!({"type": "TransactionFailed"}), vec!["type"]),
        (json!({"type": "InsufficientFunds"}), vec!["type"]),
        (json!({"type": "SorobanInvocation"}), vec!["type"]),
        (
            json!({"type": "LargeTransfer", "threshold_xlm": 100}),
            vec!["type"],
        ),
        (
            json!({"type": "FunctionCalled", "function_name": "withdraw"}),
            vec!["type", "function_name"],
        ),
        (
            json!({"type": "FunctionCallCount", "function_name": "withdraw", "min_count": 2}),
            vec!["type", "function_name", "min_count"],
        ),
        (
            json!({"type": "AdminFunctionCalled", "function_names": ["upgrade"]}),
            vec!["type", "function_names"],
        ),
        (
            json!({"type": "HighFee", "threshold_stroops": 100}),
            vec!["type"],
        ),
        (
            json!({"type": "SourceAccountMatch", "source_account": "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}),
            vec!["type", "source_account"],
        ),
        (
            json!({"type": "TimeWindow", "hours_utc": "09:00-17:00"}),
            vec!["type", "hours_utc"],
        ),
        (
            json!({"type": "LedgerSequenceRange", "from": 1, "to": 100}),
            vec!["type", "from", "to"],
        ),
    ];
    assert_eq!(variants.len(), cases.len());

    for (input, required) in cases {
        let tag = input["type"].as_str().expect("rule should have a type");
        let variant = variants
            .iter()
            .find(|variant| variant["properties"]["type"]["enum"] == json!([tag]))
            .unwrap_or_else(|| panic!("missing schema for {tag}"));
        assert_eq!(required_fields(variant), required.into_iter().collect());
        serde_json::from_value::<AlertRule>(input)
            .expect("schema rule examples should deserialize");
    }
}

#[test]
fn schema_describes_tls_and_string_maps() {
    let schema = schema();
    let tls = &schema["definitions"]["TlsConfig"];
    assert_eq!(tls["additionalProperties"], false);
    assert_eq!(
        required_fields(tls),
        BTreeSet::from(["client_cert", "client_key"])
    );
    assert_eq!(tls["properties"]["client_cert"]["type"], "string");
    assert!(!required_fields(tls).contains("ca_cert"));

    let properties = &schema["definitions"]["WatchedContract"]["properties"];
    assert_eq!(
        properties["webhook_headers"]["additionalProperties"]["type"],
        "string"
    );
    assert_eq!(properties["tags"]["additionalProperties"]["type"], "string");
}

#[test]
fn schema_generation_is_deterministic() {
    let first =
        serde_json::to_string_pretty(&AppConfig::json_schema()).expect("schema should serialize");
    let second =
        serde_json::to_string_pretty(&AppConfig::json_schema()).expect("schema should serialize");
    assert_eq!(first, second);
}

#[test]
fn schema_matches_committed_snapshot() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/schema.json");
    let raw = fs::read_to_string(path).expect("config/schema.json should exist");
    let snapshot: Value =
        serde_json::from_str(&raw).expect("config/schema.json should contain a valid JSON Schema");
    assert_eq!(
        schema(),
        snapshot,
        "Configuration schema drift detected. Regenerate config/schema.json with: \
         cargo run --locked --quiet -p txwatch-config --example export_schema > config/schema.json"
    );
}
