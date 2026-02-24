use anyhow::{Context, Result};
use console::style;
use std::path::Path;

use crate::adapter_tracker::SyncIndex;
use crate::plugin_tracker::PluginIndex;
use crate::store::SecretStore;

const ESK_GITIGNORE_COMMENT: &str = "# esk";
const ESK_GITIGNORE_ENTRY: &str = ".esk/";

pub fn run(cwd: &Path) -> Result<()> {
    let config_path = cwd.join("esk.yaml");
    let esk_dir = cwd.join(".esk");
    let store_path = esk_dir.join("store.enc");
    let key_path = esk_dir.join("store.key");
    let sync_index_path = esk_dir.join("sync-index.json");

    cliclack::intro(style("esk init").bold())?;

    // Scaffold esk.yaml if it doesn't exist
    if !config_path.is_file() {
        let scaffold = r#"project: myapp

environments: [dev, prod]

apps:
  web:
    path: apps/web

adapters:
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
    } else {
        cliclack::log::remark(format!("Exists  {}", style(config_path.display()).dim()))?;
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

    // Create empty sync index
    if !sync_index_path.is_file() {
        let index = SyncIndex::new(&sync_index_path);
        index.save()?;
        cliclack::log::success(format!(
            "Created {}",
            style(sync_index_path.display()).dim()
        ))?;
    } else {
        cliclack::log::remark(format!(
            "Exists  {}",
            style(sync_index_path.display()).dim()
        ))?;
    }

    // Create empty plugin index
    let plugin_index_path = esk_dir.join("plugin-index.json");
    if !plugin_index_path.is_file() {
        let index = PluginIndex::new(&plugin_index_path);
        index.save()?;
        cliclack::log::success(format!(
            "Created {}",
            style(plugin_index_path.display()).dim()
        ))?;
    } else {
        cliclack::log::remark(format!(
            "Exists  {}",
            style(plugin_index_path.display()).dim()
        ))?;
    }

    let gitignore_path = cwd.join(".gitignore");
    if ensure_esk_gitignore_entry(&gitignore_path)? {
        cliclack::log::success(format!(
            "Updated {} with {}",
            style(gitignore_path.display()).dim(),
            style(ESK_GITIGNORE_ENTRY).bold()
        ))?;
    }

    cliclack::outro(format!(
        "Run {} to add secrets",
        style("esk set <KEY> --env <ENV>").cyan()
    ))?;
    Ok(())
}

fn ensure_esk_gitignore_entry(gitignore_path: &Path) -> Result<bool> {
    let mut contents = if gitignore_path.is_file() {
        std::fs::read_to_string(gitignore_path)?
    } else {
        String::new()
    };

    if contents.lines().any(|line| line.trim() == ESK_GITIGNORE_ENTRY) {
        return Ok(false);
    }

    if !contents.is_empty() {
        if !contents.ends_with('\n') {
            contents.push('\n');
        }
        contents.push('\n');
    }
    contents.push_str(ESK_GITIGNORE_COMMENT);
    contents.push('\n');
    contents.push_str(ESK_GITIGNORE_ENTRY);
    contents.push('\n');

    std::fs::write(gitignore_path, contents)
        .with_context(|| format!("failed to update {}", gitignore_path.display()))?;
    Ok(true)
}
