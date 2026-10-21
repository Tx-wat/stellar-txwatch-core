#![allow(dead_code)]
use txwatch_config::{AlertRule, Network, WatchedContract};

pub fn contract(webhook_url: &str, rules: Vec<AlertRule>) -> WatchedContract {
    WatchedContract {
        label: "Integration Test Contract".into(),
        contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
        network: Network::Testnet,
        rules,
        webhook_url: webhook_url.to_string(),
        webhook_secret: None,
        tls: None,
        horizon_base_url_override: None,
    }
}

pub fn tx_page(hash: &str, paging_token: &str, successful: bool) -> serde_json::Value {
    serde_json::json!({
        "_embedded": {
            "records": [{
                "hash":         hash,
                "created_at":   "2024-06-01T10:00:00Z",
                "successful":   successful,
                "paging_token": paging_token,
                "fee_charged":  "100",
                "envelope_xdr": null,
                "result_xdr":   null
            }]
        }
    })
}

pub fn ops_page(function_name: &str) -> serde_json::Value {
    serde_json::json!({
        "_embedded": {
            "records": [{
                "type":     "invoke_host_function",
                "function": function_name
            }]
        }
    })
}

pub fn payment_ops_page(amount_str: &str) -> serde_json::Value {
    serde_json::json!({
        "_embedded": {
            "records": [{
                "type":   "payment",
                "amount": amount_str
            }]
        }
    })
}

pub fn empty_page() -> serde_json::Value {
    serde_json::json!({ "_embedded": { "records": [] } })
}

pub fn tx_page_3(hash1: &str, hash2: &str, hash3: &str) -> serde_json::Value {
    serde_json::json!({
        "_embedded": {
            "records": [
                {
                    "hash": hash1, "created_at": "2024-06-01T10:00:00Z",
                    "successful": true, "paging_token": "1",
                    "fee_charged": "100", "envelope_xdr": null, "result_xdr": null
                },
                {
                    "hash": hash2, "created_at": "2024-06-01T10:01:00Z",
                    "successful": true, "paging_token": "2",
                    "fee_charged": "100", "envelope_xdr": null, "result_xdr": null
                },
                {
                    "hash": hash3, "created_at": "2024-06-01T10:02:00Z",
                    "successful": true, "paging_token": "3",
                    "fee_charged": "100", "envelope_xdr": null, "result_xdr": null
                }
            ]
        }
    })
}
