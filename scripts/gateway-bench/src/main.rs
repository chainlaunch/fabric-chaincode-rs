// Persistent-connection benchmark client for fabric-chaincode-rs's 3-way
// (Go/Rust/TypeScript) comparison. Unlike scripts/benchmark.py (which spawns
// `peer chaincode query/invoke` as a fresh process per call, paying a full
// TLS handshake + CLI startup every time), this connects ONCE via the
// fabric-gateway Rust client and reuses that connection for every call --
// isolating peer + chaincode latency from CLI/TLS setup cost, and adding a
// genuine concurrent-throughput measurement the CLI-based benchmark cannot
// do at all (one CLI process = one in-flight call).
//
// Usage:
//   gateway-bench --endpoint localhost:7051 --override peer0.org1.example.com \
//     --tls-ca <path> --msp Org1MSP --cert <path> --key <path> \
//     --channel mychannel --chaincodes basic-go,basic-rust,basic-ts \
//     --num-calls 30 --concurrency 20 --run-id bench123

use clap::Parser;
use fabric_gateway::identity::EcdsaP256Signer;
use fabric_gateway::{Gateway, Identity};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tonic::transport::{Certificate, Channel, ClientTlsConfig};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    endpoint: String,
    #[arg(long)]
    r#override: String,
    #[arg(long)]
    tls_ca: String,
    #[arg(long)]
    msp: String,
    #[arg(long)]
    cert: String,
    #[arg(long)]
    key: String,
    #[arg(long)]
    channel: String,
    /// Comma-separated chaincode names, all installed on the same channel.
    #[arg(long, value_delimiter = ',')]
    chaincodes: Vec<String>,
    #[arg(long, default_value_t = 30)]
    num_calls: usize,
    #[arg(long, default_value_t = 20)]
    concurrency: usize,
    #[arg(long, default_value = "gwbench")]
    run_id: String,
}

struct Stats {
    n: usize,
    min_ms: f64,
    mean_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    max_ms: f64,
    ops_per_s: f64,
}

fn stats(mut latencies_ms: Vec<f64>) -> Stats {
    let n = latencies_ms.len();
    if n == 0 {
        return Stats { n: 0, min_ms: 0.0, mean_ms: 0.0, p50_ms: 0.0, p95_ms: 0.0, max_ms: 0.0, ops_per_s: 0.0 };
    }
    latencies_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let sum: f64 = latencies_ms.iter().sum();
    let mean = sum / n as f64;
    let p50 = latencies_ms[n / 2];
    let p95 = latencies_ms[((n as f64 * 0.95) as usize).min(n - 1)];
    Stats {
        n,
        min_ms: latencies_ms[0],
        mean_ms: mean,
        p50_ms: p50,
        p95_ms: p95,
        max_ms: latencies_ms[n - 1],
        // ops/s from mean single-caller latency -- NOT the same thing as
        // concurrent throughput; see run_concurrent below for that.
        ops_per_s: 1000.0 / mean,
    }
}

/// One request at a time, over the one already-open connection. Isolates
/// peer + chaincode latency from CLI/TLS setup cost (which the CLI-driven
/// benchmark pays fresh on every single call).
async fn run_sequential_query(contract: &fabric_gateway::Contract, n: usize) -> Stats {
    let mut latencies = Vec::with_capacity(n);
    for _ in 0..n {
        let start = Instant::now();
        if let Err(e) = contract.evaluate_transaction("ReadAsset", &["asset1"]).await {
            eprintln!("  query failed: {e}");
            continue;
        }
        latencies.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    stats(latencies)
}

async fn run_sequential_invoke(contract: &fabric_gateway::Contract, n: usize, run_id: &str, cc: &str) -> Stats {
    let mut latencies = Vec::with_capacity(n);
    for i in 0..n {
        let id = format!("{run_id}-{cc}-seq-{i}");
        let start = Instant::now();
        if let Err(e) = contract
            .submit_transaction("CreateAsset", &[&id, "purple", "1", "bench", "1"])
            .await
        {
            eprintln!("  invoke failed: {e}");
            continue;
        }
        latencies.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    stats(latencies)
}

/// Many requests in flight at once over the SAME connection -- this is the
/// number the CLI-per-call benchmark structurally cannot produce (one CLI
/// process can only have one call in flight). Reports wall-clock throughput,
/// not per-call latency.
async fn run_concurrent_query(contract: Arc<fabric_gateway::Contract>, total: usize, concurrency: usize) -> (f64, usize) {
    let per_task = total.div_ceil(concurrency);
    let start = Instant::now();
    let mut handles = Vec::with_capacity(concurrency);
    for _ in 0..concurrency {
        let c = Arc::clone(&contract);
        handles.push(tokio::spawn(async move {
            let mut failures = 0usize;
            for _ in 0..per_task {
                if c.evaluate_transaction("ReadAsset", &["asset1"]).await.is_err() {
                    failures += 1;
                }
            }
            failures
        }));
    }
    let mut failures = 0usize;
    for h in handles {
        failures += h.await.unwrap_or(per_task);
    }
    let elapsed = start.elapsed().as_secs_f64();
    let completed = concurrency * per_task - failures;
    (completed as f64 / elapsed, failures)
}

async fn run_concurrent_invoke(
    contract: Arc<fabric_gateway::Contract>,
    total: usize,
    concurrency: usize,
    run_id: &str,
    cc: &str,
) -> (f64, usize) {
    let per_task = total.div_ceil(concurrency);
    let start = Instant::now();
    let mut handles = Vec::with_capacity(concurrency);
    for t in 0..concurrency {
        let c = Arc::clone(&contract);
        let run_id = run_id.to_string();
        let cc = cc.to_string();
        handles.push(tokio::spawn(async move {
            let mut failures = 0usize;
            for i in 0..per_task {
                let id = format!("{run_id}-{cc}-c{t}-{i}");
                if c.submit_transaction("CreateAsset", &[&id, "purple", "1", "bench", "1"]).await.is_err() {
                    failures += 1;
                }
            }
            failures
        }));
    }
    let mut failures = 0usize;
    for h in handles {
        failures += h.await.unwrap_or(per_task);
    }
    let elapsed = start.elapsed().as_secs_f64();
    let completed = concurrency * per_task - failures;
    (completed as f64 / elapsed, failures)
}

fn print_stats_row(cc: &str, op: &str, s: &Stats) {
    println!(
        "{cc:<12} {op:<8} {:>4} {:>7.1} {:>7.1} {:>7.1} {:>7.1} {:>7.1} {:>7.1}",
        s.n, s.min_ms, s.mean_ms, s.p50_ms, s.p95_ms, s.max_ms, s.ops_per_s
    );
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(std::fs::read(&args.tls_ca)?))
        .domain_name(&args.r#override);
    let channel = Channel::from_shared(format!("https://{}", args.endpoint))?
        .tls_config(tls)?
        .connect()
        .await?;

    let identity = Identity::from_cert_file(&args.msp, &args.cert)?;
    let signer = Arc::new(EcdsaP256Signer::from_file(&args.key)?);
    let gateway = Gateway::builder(identity, channel)
        .with_evaluate_timeout(Duration::from_secs(15))
        .with_endorse_timeout(Duration::from_secs(15))
        .with_submit_timeout(Duration::from_secs(15))
        .with_commit_status_timeout(Duration::from_secs(60))
        .with_sign(signer)
        .connect()?;

    let network = gateway.get_network(&args.channel);

    println!(
        "{:<12} {:<8} {:>4} {:>7} {:>7} {:>7} {:>7} {:>7} {:>7}",
        "Chaincode", "Op", "n", "min", "mean", "p50", "p95", "max", "ops/s"
    );
    let mut concurrent_results = Vec::new();

    for cc in &args.chaincodes {
        let contract = network.get_contract(cc);

        // Warm up (first call on a fresh connection pays connection-level
        // setup this tool is specifically trying to amortize out of the
        // measurement).
        let _ = contract.evaluate_transaction("ReadAsset", &["asset1"]).await;

        let q = run_sequential_query(&contract, args.num_calls).await;
        print_stats_row(cc, "query", &q);
        let inv = run_sequential_invoke(&contract, args.num_calls, &args.run_id, cc).await;
        print_stats_row(cc, "invoke", &inv);

        let contract = Arc::new(contract);
        let (q_ops, q_fail) = run_concurrent_query(Arc::clone(&contract), args.num_calls * args.concurrency, args.concurrency).await;
        let (i_ops, i_fail) = run_concurrent_invoke(contract, args.num_calls * args.concurrency, args.concurrency, &args.run_id, cc).await;
        concurrent_results.push((cc.clone(), q_ops, q_fail, i_ops, i_fail));
    }

    println!();
    println!(
        "{:<12} {:>14} {:>10} {:>14} {:>10}",
        "Chaincode", "query ops/s", "failures", "invoke ops/s", "failures"
    );
    println!("  (concurrency={}, {} total calls per op per chaincode)", args.concurrency, args.num_calls * args.concurrency);
    for (cc, q_ops, q_fail, i_ops, i_fail) in concurrent_results {
        println!("{cc:<12} {q_ops:>14.1} {q_fail:>10} {i_ops:>14.1} {i_fail:>10}");
    }

    Ok(())
}
