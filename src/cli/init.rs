use anyhow::{Context, Result};
use console::style;
use std::path::Path;

use crate::deploy_tracker::DeployIndex;
use crate::store::SecretStore;
use crate::sync_tracker::SyncIndex;

enum FileStatus {
    Created,
    Existed,
}

struct InitReport {
    config: FileStatus,
    key: FileStatus,
    store: FileStatus,
    deploy_index: FileStatus,
    sync_index: FileStatus,
    gitignore_updated: bool,
}

impl InitReport {
    fn render(&self, cwd: &Path) -> Result<()> {
        let config_path = cwd.join("esk.yaml");
        let esk_dir = cwd.join(".esk");
        let key_path = esk_dir.join("store.key");
        let store_path = esk_dir.join("store.enc");
        let deploy_index_path = esk_dir.join("deploy-index.json");
        let sync_index_path = esk_dir.join("sync-index.json");
        let gitignore_path = cwd.join(".gitignore");

        cliclack::intro(style("esk init").bold())?;

        Self::render_file(&self.config, &config_path)?;
        Self::render_file(&self.key, &key_path)?;
        Self::render_file(&self.store, &store_path)?;
        Self::render_file(&self.deploy_index, &deploy_index_path)?;
        Self::render_file(&self.sync_index, &sync_index_path)?;

        if self.gitignore_updated {
            cliclack::log::success(format!(
                "Updated {}",
                style(gitignore_path.display()).dim(),
            ))?;
        }

        cliclack::outro(format!(
            "Run {} to add secrets",
            style("esk set <KEY> --env <ENV>").cyan()
        ))?;
        Ok(())
    }

    fn render_file(status: &FileStatus, path: &Path) -> Result<()> {
        match status {
            FileStatus::Created => {
                cliclack::log::success(format!("Created {}", style(path.display()).dim()))?;
            }
            FileStatus::Existed => {
                cliclack::log::remark(format!("Exists  {}", style(path.display()).dim()))?;
            }
        }
        Ok(())
    }
}

const ESK_GITIGNORE_COMMENT: &str = "# esk (store.enc is safe to commit)";
const ESK_GITIGNORE_ENTRIES: &[&str] = &[
    ".esk/store.key",
    ".esk/deploy-index.json",
    ".esk/sync-index.json",
];

pub fn run(cwd: &Path) -> Result<()> {
    let report = ensure_project(cwd)?;
    report.render(cwd)
}

fn ensure_project(cwd: &Path) -> Result<InitReport> {
    let config_path = cwd.join("esk.yaml");
    let esk_dir = cwd.join(".esk");
    let store_path = esk_dir.join("store.enc");
    let key_path = esk_dir.join("store.key");
    let deploy_index_path = esk_dir.join("deploy-index.json");
    let sync_index_path = esk_dir.join("sync-index.json");
    let gitignore_path = cwd.join(".gitignore");

    // Scaffold esk.yaml if it doesn't exist
    let config_status = if config_path.is_file() {
        FileStatus::Existed
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
        FileStatus::Created
    };

    // Create store (generates key + empty encrypted store)
    let (key_status, store_status) = if key_path.is_file() && store_path.is_file() {
        (FileStatus::Existed, FileStatus::Existed)
    } else {
        let _store = SecretStore::load_or_create(cwd)?;
        (FileStatus::Created, FileStatus::Created)
    };

    // Create empty deploy index
    let deploy_index_status = if deploy_index_path.is_file() {
        FileStatus::Existed
    } else {
        let index = DeployIndex::new(&deploy_index_path);
        index.save()?;
        FileStatus::Created
    };

    // Create empty sync index
    let sync_index_status = if sync_index_path.is_file() {
        FileStatus::Existed
    } else {
        let index = SyncIndex::new(&sync_index_path);
        index.save()?;
        FileStatus::Created
    };

    let gitignore_updated = ensure_esk_gitignore_entries(&gitignore_path)?;

    Ok(InitReport {
        config: config_status,
        key: key_status,
        store: store_status,
        deploy_index: deploy_index_status,
        sync_index: sync_index_status,
        gitignore_updated,
    })
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
