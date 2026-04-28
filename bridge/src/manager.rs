use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::client::BrokerSender;
use crate::spawn::AgentProcess;

pub type SharedSender = Arc<Mutex<Arc<Mutex<BrokerSender>>>>;

pub struct AgentManager {
    pub bridge_id: String,
    pub broker_url: String,
    pub coworker_path: Option<String>,
    pub agents: HashMap<String, AgentProcess>,
    sender_ref: SharedSender,
}

impl AgentManager {
    pub fn new(bridge_id: String, broker_url: String, coworker_path: Option<String>, sender_ref: SharedSender) -> Self {
        Self {
            bridge_id,
            broker_url,
            coworker_path,
            agents: HashMap::new(),
            sender_ref,
        }
    }

    pub async fn spawn_agent(
        &mut self,
        cmd: String,
        mut args: Vec<String>,
        cwd: Option<String>,
    ) -> Result<String> {
        crate::validate_spawn_command(&cmd)?;

        // Auto-setup MCP config for harness commands
        if let Some(ref coworker) = self.coworker_path {
            if crate::is_harness_command(&cmd) {
                crate::ensure_mcp_config(coworker);
                // Auto-append channel flag if not present
                if !args.iter().any(|a| a.contains("dangerously-load-development-channels")) {
                    args.push("--dangerously-load-development-channels".to_string());
                    args.push("server:agent-hive".to_string());
                }
            }
        }

        let mut agent = AgentProcess::spawn(cmd, args, &self.broker_url, cwd)?;
        let id = agent.id.clone();

        // Register with broker
        let msg = serde_json::json!({
            "type": "register",
            "id": id,
            "name": format!("agent-{}", id),
            "pid": agent.pid,
            "bridge_id": self.bridge_id,
            "harness": "claude-code",
            "hostname": hostname::get().map(|h| h.to_string_lossy().to_string()).unwrap_or_default(),
        });
        let inner = self.sender_ref.lock().await.clone();
        let mut tx = inner.lock().await;
        if !tx.send(&msg).await {
            eprintln!("Warning: failed to register agent {} — broker connection may be down", id);
        }
        drop(tx);

        // Start forwarding PTY output to broker in a background thread
        if let Some((reader, exited)) = agent.take_reader() {
            let session_id = id.clone();
            let sender_ref = self.sender_ref.clone();
            let rt = tokio::runtime::Handle::current();
            std::thread::spawn(move || {
                use std::io::Read;
                let mut reader: Box<dyn std::io::Read + Send> = reader;
                let mut buf = [0u8; 4096];
                let mut consecutive_failures: u32 = 0;
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let data = hex::encode(&buf[..n]);
                            let msg = serde_json::json!({
                                "type": "terminal_output",
                                "session_id": session_id,
                                "data": data,
                            });
                            let sent = rt.block_on(async {
                                let inner = sender_ref.lock().await.clone();
                                let mut tx = inner.lock().await;
                                tx.send(&msg).await
                            });
                            if sent {
                                consecutive_failures = 0;
                            } else {
                                consecutive_failures += 1;
                                if consecutive_failures >= 10 {
                                    eprintln!("Agent {} PTY reader: 10 consecutive send failures, stopping", session_id);
                                    break;
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
                // Mark as exited
                exited.store(true, std::sync::atomic::Ordering::Relaxed);
                // Notify broker that agent exited
                let exit_msg = serde_json::json!({
                    "type": "agent_exited",
                    "session_id": session_id,
                });
                rt.block_on(async {
                    let inner = sender_ref.lock().await.clone();
                    let mut tx = inner.lock().await;
                    tx.send(&exit_msg).await;
                });
                println!("Agent {} PTY closed", session_id);
            });
        }

        self.agents.insert(id.clone(), agent);
        Ok(id)
    }

    /// Re-register all running agents with the broker after reconnect.
    /// Skips agents whose process has exited.
    pub async fn re_register_all(&mut self) {
        if self.agents.is_empty() { return; }
        // Clean up dead agents first
        self.cleanup_dead();
        if self.agents.is_empty() { return; }
        println!("Re-registering {} running agent(s)...", self.agents.len());
        for (id, agent) in &self.agents {
            let msg = serde_json::json!({
                "type": "register",
                "id": id,
                "name": format!("agent-{}", id),
                "pid": agent.pid,
                "bridge_id": self.bridge_id,
                "harness": "claude-code",
                "hostname": hostname::get().map(|h| h.to_string_lossy().to_string()).unwrap_or_default(),
            });
            let inner = self.sender_ref.lock().await.clone();
            let mut tx = inner.lock().await;
            if !tx.send(&msg).await {
                eprintln!("Warning: failed to re-register agent {}", id);
            }
        }
    }

    pub async fn kill_agent(
        &mut self,
        id: &str,
    ) -> Result<()> {
        if let Some(mut agent) = self.agents.remove(id) {
            agent.kill()?;
            println!("Agent {} terminated", id);
        }
        Ok(())
    }

    pub fn list_agents(&self) {
        if self.agents.is_empty() {
            println!("No agents running.");
            return;
        }
        for (id, agent) in &self.agents {
            println!("  {}  pid={}  cmd={}", id, agent.pid, agent.cmd);
        }
    }

    /// Remove agents whose PTY reader threads have reported exit.
    pub fn cleanup_dead(&mut self) -> Vec<String> {
        let dead: Vec<String> = self.agents.iter()
            .filter(|(_, agent)| agent.is_exited())
            .map(|(id, _)| id.clone())
            .collect();
        for id in &dead {
            self.agents.remove(id);
            println!("Cleaned up dead agent: {}", id);
        }
        dead
    }

    /// Check if an agent is still running.
    pub fn is_agent_alive(&self, id: &str) -> bool {
        self.agents.get(id).map_or(false, |a| !a.is_exited())
    }

    pub async fn shutdown(&mut self) {
        let ids: Vec<String> = self.agents.keys().cloned().collect();
        for id in ids {
            let _ = self.kill_agent(&id).await;
        }
    }

    pub fn write_to_agent(&mut self, id: &str, data: &[u8]) -> Result<()> {
        if let Some(agent) = self.agents.get_mut(id) {
            agent.write(data)?;
        }
        Ok(())
    }

    pub fn resize_agent(&mut self, id: &str, cols: u16, rows: u16) -> Result<()> {
        if let Some(agent) = self.agents.get_mut(id) {
            agent.resize(cols, rows)?;
        }
        Ok(())
    }
}
