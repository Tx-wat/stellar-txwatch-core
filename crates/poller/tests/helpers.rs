#![allow(dead_code)]
use txwatch_config::{AlertRule, Network, WatchedContract};

/// A shutdown receiver that never fires. The sender is leaked on purpose so the
/// channel stays open for the lifetime of the test process.
pub fn shutdown_rx() -> tokio::sync::oneshot::Receiver<()> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::mem::forget(tx);
    rx
}

pub fn contract(webhook_url: &str, rules: Vec<AlertRule>) -> WatchedContract {
    WatchedContract {
        label: "Integration Test Contract".into(),
        contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
        network: Network::Testnet,
        rules,
        webhook_url: webhook_url.to_string(),
        webhook_secret: None,
        webhook_headers: Default::default(),
        max_alerts_per_minute: None,
        cursor_alias: None,
        webhook_compress: false,
        enabled: true,
        webhook_rule_filter: Vec::new(),
        webhook_timeout_seconds: None,
        allow_internal_webhook_urls: Some(false),
        horizon_url: None,
        test_horizon_base_url: None,
        max_alerts_per_cycle: None,
        tags: Vec::new(),
        tls: None,
        webhook_tls_cert_pem: None,
    }
}

/// Builds a Horizon error response body (e.g. a 503 Service Unavailable).
pub fn horizon_error(status: u16, title: &str) -> serde_json::Value {
    serde_json::json!({
        "type":   "https://stellar.org/horizon-errors/error",
        "title":  title,
        "status": status,
        "detail": title,
    })
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
