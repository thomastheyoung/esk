use anyhow::{Context, Result};
use std::process::Command;

use crate::adapters::SyncAdapter;
use crate::config::{Config, ConvexAdapterConfig, ResolvedTarget};

pub struct ConvexAdapter<'a> {
    pub config: &'a Config,
    pub adapter_config: &'a ConvexAdapterConfig,
}

impl<'a> SyncAdapter for ConvexAdapter<'a> {
    fn name(&self) -> &str {
        "convex"
    }

    fn sync_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let convex_path = self.config.root.join(&self.adapter_config.path);

        let env_flags = self
            .adapter_config
            .env_flags
            .get(&target.environment)
            .cloned()
            .unwrap_or_default();

        // Read CONVEX_DEPLOYMENT from deployment_source if configured
        let mut env_vars: Vec<(String, String)> = Vec::new();
        if let Some(source) = &self.adapter_config.deployment_source {
            let source_path = self.config.root.join(source);
            if source_path.is_file() {
                let contents = std::fs::read_to_string(&source_path)
                    .with_context(|| format!("failed to read {}", source_path.display()))?;
                for line in contents.lines() {
                    if let Some(deployment) = line.strip_prefix("CONVEX_DEPLOYMENT=") {
                        let deployment = deployment.trim().trim_matches('"').trim_matches('\'');
                        env_vars.push(("CONVEX_DEPLOYMENT".to_string(), deployment.to_string()));
                        break;
                    }
                }
            }
        }

        let mut cmd = Command::new("npx");
        cmd.args(["convex", "env", "set", key, value]);
        if !env_flags.is_empty() {
            cmd.args(env_flags.split_whitespace());
        }
        cmd.current_dir(&convex_path);
        for (k, v) in &env_vars {
            cmd.env(k, v);
        }

        let output = cmd
            .output()
            .with_context(|| format!("failed to run convex for {key}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("convex env set failed for {key}: {stderr}");
        }

        Ok(())
    }
}
