use anyhow::{Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

pub struct AgentProcess {
    pub id: String,
    pub pid: u32,
    pub cmd: String,
    pty: Box<dyn MasterPty + Send>,
    pty_reader: Option<Box<dyn Read + Send>>,
    pty_writer: Option<Box<dyn Write + Send>>,
    child: Arc<Mutex<Box<dyn Child + Send>>>,
    /// Set to true when the PTY reader thread detects EOF / exit
    exited: Arc<std::sync::atomic::AtomicBool>,
}

impl AgentProcess {
    pub fn spawn(cmd: String, args: Vec<String>, broker_url: &str, cwd: Option<String>) -> Result<Self> {
        let id = hex::encode(&uuid::Uuid::new_v4().as_bytes()[..4]);

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 30,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("Failed to create PTY")?;

        // Convert ws:// back to http:// for the agent's HIVE_HOST
        let hive_host = broker_url.replace("ws://", "http://").replace("wss://", "https://");

        // Resolve cwd: explicit > bridge cwd > fall back to current dir
        let resolved_cwd = match cwd {
            Some(ref p) if !p.is_empty() => {
                let path = std::path::PathBuf::from(p);
                if !path.is_absolute() {
                    anyhow::bail!("cwd must be an absolute path: {}", p);
                }
                if !path.is_dir() {
                    anyhow::bail!("cwd does not exist or is not a directory: {}", p);
                }
                path
            }
            _ => std::env::current_dir().context("Failed to get current directory")?,
        };

        // On Windows, wrap in cmd.exe /C to resolve .cmd/.sh launchers
        #[cfg(target_os = "windows")]
        let builder = {
            let mut all_args = vec![cmd.clone()];
            all_args.extend(args.iter().cloned());
            let mut b = CommandBuilder::new("cmd.exe");
            b.arg("/C");
            b.args(&all_args);
            b.cwd(&resolved_cwd);
            b.env("HIVE_HOST", &hive_host);
            b.env("AGENT_HIVE_PEER_ID", &id);
            b
        };

        #[cfg(not(target_os = "windows"))]
        let builder = {
            let mut b = CommandBuilder::new(&cmd);
            b.args(&args);
            b.cwd(&resolved_cwd);
            b.env("HIVE_HOST", &hive_host);
            b.env("AGENT_HIVE_PEER_ID", &id);
            b
        };

        let child = pair
            .slave
            .spawn_command(builder)
            .context("Failed to spawn child process")?;

        let pid = child.process_id().unwrap_or(0);

        let reader = pair.master.try_clone_reader().context("Failed to clone PTY reader")?;
        let writer = pair.master.take_writer().context("Failed to get PTY writer")?;

        Ok(Self {
            id,
            pid,
            cmd,
            pty: pair.master,
            pty_reader: Some(Box::new(reader)),
            pty_writer: Some(writer),
            child: Arc::new(Mutex::new(child)),
            exited: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        if let Some(ref mut writer) = self.pty_writer {
            writer.write_all(data)?;
            writer.flush()?;
        }
        Ok(())
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.pty.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        Ok(())
    }

    pub fn kill(&mut self) -> Result<()> {
        // Close PTY writer first to signal EOF
        self.pty_writer = None;

        // Kill the entire process tree via the OS
        #[cfg(target_os = "windows")]
        {
            // On Windows, use taskkill /T /F to kill the whole tree (cmd.exe → child → grandchild)
            let pid = self.pid;
            let _ = std::process::Command::new("taskkill")
                .args(["/T", "/F", "/PID", &pid.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }

        #[cfg(not(target_os = "windows"))]
        {
            // On Unix, kill the process group (negative PID sends to the whole group)
            let pid = self.pid as i32;
            if pid > 0 {
                let _ = std::process::Command::new("kill")
                    .arg(format!("-{}", pid))
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
        }

        // Also use portable-pty's kill as fallback
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }

        self.exited.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// Check if the child process has exited (read-only, uses atomic flag).
    pub fn is_exited(&self) -> bool {
        self.exited.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn take_reader(&mut self) -> Option<(Box<dyn Read + Send>, Arc<std::sync::atomic::AtomicBool>)> {
        self.pty_reader.take().map(|r| (r, self.exited.clone()))
    }
}
