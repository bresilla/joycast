use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::info;

pub struct ServiceManager {
    is_user: bool,
}

impl ServiceManager {
    pub fn new(user_mode: bool) -> Self {
        let is_user = if user_mode { true } else { !Self::is_root() };
        Self { is_user }
    }

    fn is_root() -> bool {
        std::env::var("USER").unwrap_or_default() == "root"
            || std::env::var("SUDO_USER").is_ok()
            || std::env::var("JOURNAL_STREAM").is_ok()
    }

    /// Return path to systemd unit file
    pub fn unit_path(&self) -> Result<PathBuf> {
        if self.is_user {
            let home = dirs_next::home_dir().context("Could not determine home directory")?;
            Ok(home.join(".config/systemd/user/joycast.service"))
        } else {
            Ok(PathBuf::from("/etc/systemd/system/joycast.service"))
        }
    }

    /// Generate systemd unit file content
    fn generate_unit_content(&self, exec_path: &Path) -> String {
        let exec_str = exec_path.to_string_lossy();
        if self.is_user {
            format!(
                r#"[Unit]
Description=Joycast Gamepad Server Daemon
After=network.target

[Service]
Type=simple
ExecStart={} server
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
"#,
                exec_str
            )
        } else {
            format!(
                r#"[Unit]
Description=Joycast Gamepad Server Daemon
After=network.target

[Service]
Type=simple
ExecStart={} server
Restart=on-failure
RestartSec=5
User=root
SupplementaryGroups=input

[Install]
WantedBy=multi-user.target
"#,
                exec_str
            )
        }
    }

    /// Helper to execute systemctl command
    fn run_systemctl(&self, args: &[&str]) -> Result<()> {
        let mut cmd = Command::new("systemctl");
        if self.is_user {
            cmd.arg("--user");
        }
        cmd.args(args);

        let status = cmd
            .status()
            .with_context(|| format!("Failed to execute 'systemctl {}'", args.join(" ")))?;

        if !status.success() {
            bail!("Command 'systemctl {}' failed", args.join(" "));
        }
        Ok(())
    }

    /// Install the systemd service
    pub fn install(&self) -> Result<()> {
        let exec_path =
            std::env::current_exe().context("Failed to determine current executable path")?;
        let unit_path = self.unit_path()?;
        let unit_content = self.generate_unit_content(&exec_path);

        if let Some(parent) = unit_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory {}", parent.display()))?;
        }

        fs::write(&unit_path, unit_content)
            .with_context(|| format!("Failed to write service file to {}", unit_path.display()))?;

        info!("Installed systemd unit file at {}", unit_path.display());

        let _ = self.run_systemctl(&["daemon-reload"]);
        let _ = self.run_systemctl(&["enable", "joycast"]);

        println!(
            "Joycast systemd service installed successfully!\nUnit file: {}\n\nTo start the service now, run:\n  joycast service start",
            unit_path.display()
        );

        Ok(())
    }

    /// Uninstall the systemd service
    pub fn uninstall(&self) -> Result<()> {
        let unit_path = self.unit_path()?;

        let _ = self.run_systemctl(&["stop", "joycast"]);
        let _ = self.run_systemctl(&["disable", "joycast"]);

        if unit_path.exists() {
            fs::remove_file(&unit_path)
                .with_context(|| format!("Failed to remove {}", unit_path.display()))?;
            info!("Removed systemd unit file at {}", unit_path.display());
        }

        let _ = self.run_systemctl(&["daemon-reload"]);

        println!("Joycast systemd service uninstalled successfully.");
        Ok(())
    }

    /// Start the systemd service
    pub fn start(&self) -> Result<()> {
        self.run_systemctl(&["start", "joycast"])?;
        println!("Joycast service started.");
        Ok(())
    }

    /// Stop the systemd service
    pub fn stop(&self) -> Result<()> {
        self.run_systemctl(&["stop", "joycast"])?;
        println!("Joycast service stopped.");
        Ok(())
    }

    /// Restart the systemd service
    pub fn restart(&self) -> Result<()> {
        self.run_systemctl(&["restart", "joycast"])?;
        println!("Joycast service restarted.");
        Ok(())
    }

    /// Check status of the systemd service
    pub fn status(&self) -> Result<()> {
        let mut cmd = Command::new("systemctl");
        if self.is_user {
            cmd.arg("--user");
        }
        cmd.arg("status").arg("joycast");
        let _ = cmd.status();
        Ok(())
    }

    /// Tail service logs
    pub fn logs(&self) -> Result<()> {
        let mut cmd = Command::new("journalctl");
        if self.is_user {
            cmd.arg("--user");
        }
        cmd.arg("-u").arg("joycast").arg("-f");
        let _ = cmd.status();
        Ok(())
    }
}
