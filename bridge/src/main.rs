mod client;
mod gateway;
mod manager;
mod spawn;
mod stats;

use anyhow::Result;
use clap::Parser;
use futures_util::StreamExt;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncBufReadExt;
use tokio::sync::Mutex;

#[derive(Parser)]
#[command(name = "agent-hive-landlord", about = "Agent Hive Landlord")]
struct Cli {
    /// Broker URL (overrides HIVE_HOST env var)
    #[arg(long = "host")]
    host: Option<String>,
    /// Path to coworker binary (auto-detected if not set)
    #[arg(long = "coworker")]
    coworker: Option<String>,
}

fn home_dir() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
}

fn read_or_generate_id() -> String {
    let path = home_dir().join(".agent-hive-landlord-id");
    if path.exists() {
        if let Ok(id) = std::fs::read_to_string(&path) {
            let id = id.trim().to_string();
            if !id.is_empty() {
                return id;
            }
        }
    }
    let id = hex::encode(&uuid::Uuid::new_v4().as_bytes()[..4]);
    let _ = std::fs::write(&path, &id);
    id
}

fn read_landlord_key() -> Option<String> {
    let path = home_dir().join(".agent-hive-landlord.key");
    if path.exists() {
        if let Ok(key) = std::fs::read_to_string(&path) {
            let key = key.trim().to_string();
            if !key.is_empty() {
                return Some(key);
            }
        }
    }
    None
}

pub fn save_landlord_key(key: &str) {
    let path = home_dir().join(".agent-hive-landlord.key");
    let _ = std::fs::write(&path, key);
}

pub fn delete_landlord_files() {
    let _ = std::fs::remove_file(home_dir().join(".agent-hive-landlord-id"));
    let _ = std::fs::remove_file(home_dir().join(".agent-hive-landlord.key"));
}

/// Find the coworker binary by checking common locations relative to the landlord binary.
fn find_coworker_binary() -> Option<String> {
    // Check COWORKER_PATH env var first
    if let Ok(p) = std::env::var("COWORKER_PATH") {
        if std::path::Path::new(&p).exists() {
            return Some(p);
        }
    }

    // Get the directory of the current executable
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;

    // Try: <exe_dir>/coworker (same directory)
    let same_dir = exe_dir.join("coworker");
    if same_dir.exists() {
        return same_dir.to_str().map(|s| s.to_string());
    }
    // Try: <exe_dir>/coworker.exe
    let same_dir_exe = exe_dir.join("coworker.exe");
    if same_dir_exe.exists() {
        return same_dir_exe.to_str().map(|s| s.to_string());
    }

    // Try: <exe_dir>/../../coworker/target/release/coworker (repo layout)
    if let Some(grandparent) = exe_dir.parent().and_then(|p| p.parent()) {
        for name in &["coworker", "coworker.exe"] {
            let path = grandparent.join("coworker").join("target").join("release").join(name);
            if path.exists() {
                return path.to_str().map(|s| s.to_string());
            }
        }
    }

    None
}

/// Ensure agent-hive MCP is configured globally in ~/.freecc.json
fn ensure_mcp_config(coworker_path: &str) {
    let home = home_dir();
    let config_path = home.join(".freecc.json");

    let mut config: serde_json::Value = if config_path.exists() {
        std::fs::read_to_string(&config_path)
            .ok()
            .and_then(|c| serde_json::from_str(&c).ok())
            .unwrap_or(serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    // Check if agent-hive is already configured
    if config.get("mcpServers").and_then(|s| s.get("agent-hive")).is_some() {
        return; // already configured
    }

    // Resolve to absolute path so freecc can find the binary from any cwd
    let abs_path = std::path::absolute(coworker_path)
        .unwrap_or_else(|_| std::path::PathBuf::from(coworker_path));

    if !abs_path.exists() {
        eprintln!("coworker binary not found: {}", abs_path.display());
        return;
    }

    // Add agent-hive entry
    if config.get_mut("mcpServers").and_then(|v| v.as_object_mut()).is_none() {
        config["mcpServers"] = serde_json::json!({});
    }
    if let Some(servers) = config.get_mut("mcpServers").and_then(|v| v.as_object_mut()) {
        servers.insert("agent-hive".to_string(), serde_json::json!({"command": abs_path.to_string_lossy()}));
    }

    let _ = std::fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap_or_default());
    println!("Configured agent-hive MCP in {}", config_path.display());
}

const ALLOWED_COMMANDS: &[&str] = &[
    "freecc", "claude", "claude-code", "opencode", "codex", "cursor", "bun", "node",
];

fn is_harness_command(cmd: &str) -> bool {
    let name = std::path::Path::new(cmd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(cmd);
    ALLOWED_COMMANDS.contains(&name)
}

fn validate_spawn_command(cmd: &str) -> anyhow::Result<()> {
    // Block path separators — only bare binary names allowed
    if cmd.contains('/') || cmd.contains('\\') {
        anyhow::bail!("Path separators not allowed in command: {}", cmd);
    }
    let base = cmd.split_whitespace().next().unwrap_or("");
    if !is_harness_command(base) {
        anyhow::bail!("Command not in whitelist: {} (allowed: {:?})", base, ALLOWED_COMMANDS);
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let broker_url = cli.host
        .or_else(|| std::env::var("HIVE_HOST").ok())
        .map(|h| {
            h.replace("http://", "ws://").replace("https://", "wss://")
        })
        .unwrap_or_else(|| "ws://127.0.0.1:7899".to_string());

    let local_port: u16 = std::env::var("BRIDGE_LOCAL_PORT")
        .unwrap_or_else(|_| "17900".to_string())
        .parse()
        .unwrap_or(17900);

    let bridge_id = read_or_generate_id();
    let hostname = hostname::get()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_default();

    println!("Agent Hive Landlord {}", bridge_id);
    println!("Hostname: {}", hostname);
    println!("Broker: {}", broker_url);

    // Detect coworker binary
    let coworker_path = cli.coworker
        .or_else(|| find_coworker_binary())
        .or_else(|| {
            println!("Coworker binary not found. Set --coworker or COWORKER_PATH to enable auto MCP setup.");
            None
        });
    if let Some(ref p) = coworker_path {
        println!("Coworker: {}", p);
    }

    // Keep the HTTP version of the broker URL for passing to spawned agents
    let broker_url_http = broker_url.replace("ws://", "http://").replace("wss://", "https://");

    // Shared sender reference — PTY reader threads read through this indirection.
    // On reconnect, we swap the inner Arc so all PTY threads pick up the new sender.
    let shared_sender: manager::SharedSender = Arc::new(Mutex::new(Arc::new(Mutex::new(
        client::BrokerSender::new_placeholder()
    ))));

    let mgr = Arc::new(Mutex::new(manager::AgentManager::new(
        bridge_id.clone(),
        broker_url_http,
        coworker_path,
        shared_sender.clone(),
    )));

    // Start local gateway for coworkers — gateway also uses shared_sender
    let mgr_gateway = mgr.clone();
    let gateway_sender = shared_sender.clone();
    let gateway_port = local_port;
    tokio::spawn(async move {
        let _ = gateway::start(gateway_port, mgr_gateway, gateway_sender).await;
    });

    // Reconnect loop
    let mut backoff_secs: u64 = 1;
    let max_backoff: u64 = 30;

    loop {
        let mutual_key = read_landlord_key();
        let key_desc = if mutual_key.is_some() { "with mutual key" } else { "new registration" };
        println!("Connecting to broker ({})...", key_desc);

        match client::connect(&broker_url, &bridge_id, mutual_key.as_deref(), &hostname).await {
            Ok(ws_stream) => {
                backoff_secs = 1; // reset on success
                println!("Connected to broker");

                let (ws_sink, ws_stream) = ws_stream.split();
                let broker_tx = Arc::new(Mutex::new(client::BrokerSender::new(ws_sink)));
                let broker_rx = Arc::new(Mutex::new(ws_stream));

                // Swap the shared sender — all PTY reader threads will now use this one
                {
                    let new_sender = broker_tx.clone();
                    *shared_sender.lock().await = new_sender;
                }

                // Re-register running agents after reconnect
                {
                    let mut m = mgr.lock().await;
                    m.re_register_all().await;
                }

                // Start broker message reader
                let mgr_clone = mgr.clone();
                let broker_tx_reader = broker_tx.clone();
                let bridge_id_clone = bridge_id.clone();

                // Interactive shell
                println!("Commands: spawn <cmd>, kill <id>, list, status, quit");
                let stdin = tokio::io::BufReader::new(tokio::io::stdin());
                let mut lines = stdin.lines();

                let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let done_clone = done.clone();

                // Read broker messages in background
                let reader_done = done.clone();
                tokio::spawn(async move {
                    client::read_broker_messages(broker_rx, mgr_clone, broker_tx_reader, &bridge_id_clone).await;
                    reader_done.store(true, std::sync::atomic::Ordering::Relaxed);
                });

                // System stats reporter (every 5s) — also reports live agent IDs for zombie detection
                let stats_tx = broker_tx.clone();
                let stats_mgr = mgr.clone();
                tokio::spawn(async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
                    loop {
                        interval.tick().await;
                        let mut m = stats_mgr.lock().await;
                        // Clean up dead agents first
                        m.cleanup_dead();
                        let agent_ids: Vec<String> = m.agents.keys().cloned().collect();
                        let stats = stats::collect_with_agents(&agent_ids);
                        drop(m);
                        let mut tx = stats_tx.lock().await;
                        tx.send(&stats).await;
                    }
                });

                // Interactive command loop (stdin commands + wait for broker disconnect)
                loop {
                    tokio::select! {
                        line = lines.next_line() => {
                            match line {
                                Ok(Some(input)) => {
                                    let parts: Vec<String> = input.split_whitespace().map(String::from).collect();
                                    if parts.is_empty() { continue; }

                                    match parts[0].as_str() {
                                        "spawn" => {
                                            if parts.len() < 2 {
                                                println!("Usage: spawn <command> [args...]");
                                                continue;
                                            }
                                            let cmd = parts[1].clone();
                                            let args: Vec<String> = parts[2..].to_vec();
                                            let mut m = mgr.lock().await;
                                            match m.spawn_agent(cmd, args, None).await {
                                                Ok(id) => println!("Spawned agent: {}", id),
                                                Err(e) => eprintln!("Spawn failed: {}", e),
                                            }
                                        }
                                        "kill" => {
                                            if parts.len() < 2 {
                                                println!("Usage: kill <agent-id>");
                                                continue;
                                            }
                                            let mut m = mgr.lock().await;
                                            match m.kill_agent(&parts[1]).await {
                                                Ok(()) => println!("Killed agent: {}", parts[1]),
                                                Err(e) => eprintln!("Kill failed: {}", e),
                                            }
                                        }
                                        "list" => {
                                            let m = mgr.lock().await;
                                            m.list_agents();
                                        }
                                        "status" => {
                                            let m = mgr.lock().await;
                                            println!("Bridge: {}", m.bridge_id);
                                            println!("Agents: {}", m.agents.len());
                                        }
                                        "quit" | "exit" => {
                                            println!("Shutting down...");
                                            let mut m = mgr.lock().await;
                                            m.shutdown().await;
                                            return Ok(());
                                        }
                                        _ => println!("Unknown command: {}", parts[0]),
                                    }
                                }
                                // stdin EOF (e.g. running as service) — don't break, just wait for broker
                                Ok(None) => {
                                    // No more stdin — just wait for broker disconnect
                                    while !done_clone.load(std::sync::atomic::Ordering::Relaxed) {
                                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                                    }
                                    break;
                                }
                                Err(_) => {
                                    // stdin error — same, wait for broker
                                    while !done_clone.load(std::sync::atomic::Ordering::Relaxed) {
                                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                                    }
                                    break;
                                }
                            }
                        }
                        _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                            if done_clone.load(std::sync::atomic::Ordering::Relaxed) {
                                println!("Broker disconnected");
                                break;
                            }
                        }
                    }
                }

                println!("Reconnecting in {}s...", backoff_secs);
                tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(max_backoff);
            }
            Err(e) => {
                eprintln!("Connection failed: {}", e);
                println!("Retrying in {}s...", backoff_secs);
                tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(max_backoff);
            }
        }
    }
}
