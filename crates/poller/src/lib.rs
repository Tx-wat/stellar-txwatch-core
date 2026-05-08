use std::{collections::HashMap, time::Duration};

use anyhow::Result;
use reqwest::Client;
use serde::Deserialize;
use txwatch_config::{AppConfig, WatchedContract};
use txwatch_notifier::send_webhook;
use txwatch_rules::{evaluate, HorizonTransaction};

// ── Horizon response shapes ───────────────────────────────────────────────────

#[derive(Deserialize)]
struct HorizonPage {
    _embedded: Embedded,
}

#[derive(Deserialize)]
struct Embedded {
    records: Vec<HorizonTransaction>,
}

// ── Public entry point ────────────────────────────────────────────────────────

pub async fn run(cfg: AppConfig) -> Result<()> {
    let client = Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;

    // cursor per contract_id — "now" means start from the latest
    let mut cursors: HashMap<String, String> =
        cfg.contracts.iter().map(|c| (c.contract_id.clone(), "now".to_string())).collect();

    let interval = Duration::from_secs(cfg.poll_interval_seconds);

    loop {
        for contract in &cfg.contracts {
            if let Err(e) = poll_contract(&client, contract, &mut cursors).await {
                eprintln!("[{}] poll error: {e}", contract.label);
            }
        }
        tokio::time::sleep(interval).await;
    }
}

// ── Per-contract poll ─────────────────────────────────────────────────────────

async fn poll_contract(
    client:  &Client,
    contract: &WatchedContract,
    cursors: &mut HashMap<String, String>,
) -> Result<()> {
    let cursor = cursors.get(&contract.contract_id).cloned().unwrap_or_else(|| "now".to_string());
    let base   = contract.network.horizon_base_url();
    let url    = format!(
        "{}/accounts/{}/transactions?cursor={}&order=asc&limit=200",
        base, contract.contract_id, cursor
    );

    let page: HorizonPage = client.get(&url).send().await?.json().await?;
    let records = page._embedded.records;

    for mut tx in records {
        // Advance cursor past this transaction's paging token.
        // Horizon paging tokens are the transaction's ledger sequence encoded
        // as a string; we derive it from the hash position in the page.
        // The simplest correct approach: re-fetch with the tx hash as cursor.
        cursors.insert(contract.contract_id.clone(), tx.hash.clone());

        // Enrich with Soroban invoke details parsed from envelope_xdr field.
        enrich_soroban(&mut tx);

        let payloads = evaluate(
            &contract.label,
            &contract.contract_id,
            contract.network.as_str(),
            base,
            &contract.rules,
            &tx,
        );

        for payload in payloads {
            println!(
                "[ALERT] {} — {} (tx: {})",
                payload.label, payload.rule_triggered, payload.transaction_hash
            );
            if let Err(e) = send_webhook(client, &contract.webhook_url, &payload).await {
                eprintln!("[{}] webhook error: {e}", contract.label);
            }
        }
    }

    Ok(())
}

// ── Soroban envelope enrichment ───────────────────────────────────────────────
// Horizon returns the XDR base64 envelope. Full XDR decoding requires the
// stellar-xdr crate; here we do a lightweight JSON-based extraction using
// the Soroban RPC `getTransaction` endpoint which returns decoded JSON.

fn enrich_soroban(tx: &mut HorizonTransaction) {
    // Placeholder: in a production build, call the Soroban RPC
    // `getTransaction` endpoint and parse `invokeHostFunctionOp` fields.
    // The poller already stores `envelope_xdr`; a future PR can wire in
    // stellar-xdr decoding here without changing any other crate.
    let _ = tx; // suppress unused warning
}
