use anyhow::{Context, Result};
use console::style;
use std::path::Path;

use crate::deploy_tracker::DeployIndex;
use crate::store::SecretStore;
use crate::sync_tracker::SyncIndex;

const ESK_GITIGNORE_COMMENT: &str = "# esk (store.enc is safe to commit)";
const ESK_GITIGNORE_ENTRIES: &[&str] = &[
    ".esk/store.key",
    ".esk/deploy-index.json",
    ".esk/sync-index.json",
];

pub fn run(cwd: &Path) -> Result<()> {
    let config_path = cwd.join("esk.yaml");
    let esk_dir = cwd.join(".esk");
    let store_path = esk_dir.join("store.enc");
    let key_path = esk_dir.join("store.key");
    let deploy_index_path = esk_dir.join("deploy-index.json");

    cliclack::intro(style("esk init").bold())?;

    // Scaffold esk.yaml if it doesn't exist
    if config_path.is_file() {
            cliclack::log::remark(format!("Exists  {}", style(config_path.display()).dim()))?;
        } else {
            let scaffold = r#"project: myapp

    environments: [dev, prod]

    apps:
      web:
        path: apps/web

    targets:
      env:
        pattern: "{app_path}/.env{env_suffix}.local"
        env_suffix:
          dev: ""
          prod: ".production"

    secrets:
      General:
        # EXAMPLE_SECRET:
        #   description: An example secret
        #   targets:
        #     env: [web:dev, web:prod]
    "#;
            std::fs::write(&config_path, scaffold).context("failed to write esk.yaml")?;
            cliclack::log::success(format!("Created {}", style(config_path.display()).dim()))?;
        }

    // Create store (generates key + empty encrypted store)
    if !key_path.is_file() || !store_path.is_file() {
        let _store = SecretStore::load_or_create(cwd)?;
        if key_path.is_file() {
            cliclack::log::success(format!("Created {}", style(key_path.display()).dim()))?;
        }
        if store_path.is_file() {
            cliclack::log::success(format!("Created {}", style(store_path.display()).dim()))?;
        }
    } else {
        cliclack::log::remark(format!("Exists  {}", style(key_path.display()).dim()))?;
        cliclack::log::remark(format!("Exists  {}", style(store_path.display()).dim()))?;
    }

    // Create empty deploy index
    if deploy_index_path.is_file() {
        cliclack::log::remark(format!(
            "Exists  {}",
            style(deploy_index_path.display()).dim()
        ))?;
    } else {
        let index = DeployIndex::new(&deploy_index_path);
        index.save()?;
        cliclack::log::success(format!(
            "Created {}",
            style(deploy_index_path.display()).dim()
        ))?;
    }

    // Create empty sync index
    let sync_index_path = esk_dir.join("sync-index.json");
    if sync_index_path.is_file() {
        cliclack::log::remark(format!(
            "Exists  {}",
            style(sync_index_path.display()).dim()
        ))?;
    } else {
        let index = SyncIndex::new(&sync_index_path);
        index.save()?;
        cliclack::log::success(format!(
            "Created {}",
            style(sync_index_path.display()).dim()
        ))?;
    }

    let gitignore_path = cwd.join(".gitignore");
    if ensure_esk_gitignore_entries(&gitignore_path)? {
        cliclack::log::success(format!("Updated {}", style(gitignore_path.display()).dim(),))?;
    }

    cliclack::outro(format!(
        "Run {} to add secrets",
        style("esk set <KEY> --env <ENV>").cyan()
    ))?;
    Ok(())
}

fn ensure_esk_gitignore_entries(gitignore_path: &Path) -> Result<bool> {
    let mut contents = if gitignore_path.is_file() {
        std::fs::read_to_string(gitignore_path)?
    } else {
        String::new()
    };

    let missing: Vec<&str> = ESK_GITIGNORE_ENTRIES
        .iter()
        .filter(|entry| !contents.lines().any(|line| line.trim() == **entry))
        .copied()
        .collect();

    if missing.is_empty() {
        return Ok(false);
    }

    // If no esk entries exist yet, add the comment header
    let has_any = ESK_GITIGNORE_ENTRIES
        .iter()
        .any(|entry| contents.lines().any(|line| line.trim() == *entry));

    if !has_any {
        if !contents.is_empty() {
            if !contents.ends_with('\n') {
                contents.push('\n');
            }
            contents.push('\n');
        }
        contents.push_str(ESK_GITIGNORE_COMMENT);
        contents.push('\n');
    }

    for entry in &missing {
        contents.push_str(entry);
        contents.push('\n');
    }

    std::fs::write(gitignore_path, contents)
        .with_context(|| format!("failed to update {}", gitignore_path.display()))?;
    Ok(true)
}
