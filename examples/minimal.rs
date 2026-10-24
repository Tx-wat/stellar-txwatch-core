use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use txwatch_config::{AlertRule, AppConfig, Network, WatchedContract};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("Starting local webhook receiver...");
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    println!("Receiver listening on http://{}", addr);

    // Spawn a local webhook echo receiver
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(conn) => conn,
                Err(_) => break,
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                if let Ok(n) = socket.read(&mut buf).await {
                    let req = String::from_utf8_lossy(&buf[..n]);
                    if let Some(body_start) = req.find("\r\n\r\n") {
                        let body = &req[body_start + 4..];
                        println!("\n[Received Webhook Alert]:\n{}", body);
                    }
                }
                let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK").await;
            });
        }
    });

    // In-memory config for a testnet contract
    let config = AppConfig {
        poll_interval_seconds: 10,
        cursor_file: None,
        http_pool_max_idle_per_host: None,
        http_tcp_keepalive_secs: None,
        http_connection_verbose: None,
        max_alerts_per_cycle: None,
        contracts: vec![WatchedContract {
            label: "Testnet Escrow Quickstart".into(),
            contract_id: "CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC".into(),
            network: Network::Testnet,
            rules: vec![AlertRule::AnyTransaction],
            webhook_url: format!("http://{}/webhook", addr),
            webhook_secret: None,
            test_horizon_base_url: None,
        }],
    };

    println!("Running single poll cycle against Stellar Testnet...");
    let failed = txwatch_poller::run_once(config, false).await?;
    if failed == 0 {
        println!("Poll cycle completed successfully.");
    } else {
        println!("Poll cycle completed with {} contract failure(s).", failed);
    }

    Ok(())
}
