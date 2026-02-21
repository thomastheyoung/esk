use anyhow::{Context, Result};
use std::io::Write;
use std::process::{Command, Stdio};

use crate::adapters::SyncAdapter;
use crate::config::{CloudflareAdapterConfig, Config, ResolvedTarget};

pub struct CloudflareAdapter<'a> {
    pub config: &'a Config,
    pub adapter_config: &'a CloudflareAdapterConfig,
}

impl<'a> SyncAdapter for CloudflareAdapter<'a> {
    fn name(&self) -> &str {
        "cloudflare"
    }

    fn sync_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let app = target
            .app
            .as_deref()
            .context("cloudflare adapter requires an app")?;
        let app_config = self
            .config
            .apps
            .get(app)
            .with_context(|| format!("unknown app '{app}'"))?;
        let app_path = self.config.root.join(&app_config.path);

        let env_flags = self
            .adapter_config
            .env_flags
            .get(&target.environment)
            .cloned()
            .unwrap_or_default();

        let mut cmd = Command::new("wrangler");
        cmd.args(["secret", "put", key]);
        if !env_flags.is_empty() {
            cmd.args(env_flags.split_whitespace());
        }
        cmd.current_dir(&app_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn wrangler for {key}"))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(value.as_bytes())
                .with_context(|| format!("failed to write value to wrangler stdin for {key}"))?;
        }

        let output = child
            .wait_with_output()
            .with_context(|| format!("failed to wait for wrangler for {key}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("wrangler secret put failed for {key}: {stderr}");
        }

        Ok(())
    }
}
