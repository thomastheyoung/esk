#[cfg(not(feature = "keychain"))]
use anyhow::bail;
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

enum KeyStatus {
    File(FileStatus),
    #[cfg(feature = "keychain")]
    Keychain(FileStatus),
}

struct InitReport {
    config: FileStatus,
    key: KeyStatus,
    store: FileStatus,
    deploy_index: FileStatus,
    sync_index: FileStatus,
    gitignore_updated: bool,
}

impl InitReport {
    fn render(&self, cwd: &Path) -> Result<()> {
        let config_path = cwd.join("esk.yaml");
        let esk_dir = cwd.join(".esk");
        let store_path = esk_dir.join("store.enc");
        let deploy_index_path = esk_dir.join("deploy-index.json");
        let sync_index_path = esk_dir.join("sync-index.json");
        let gitignore_path = cwd.join(".gitignore");

        cliclack::intro(style("esk init").bold())?;

        Self::render_file(&self.config, &config_path)?;
        match &self.key {
            KeyStatus::File(status) => {
                Self::render_file(status, &esk_dir.join("store.key"))?;
            }
            #[cfg(feature = "keychain")]
            KeyStatus::Keychain(status) => match status {
                FileStatus::Created => {
                    cliclack::log::success(format!(
                        "Stored  {}",
                        style("encryption key in OS keychain").dim()
                    ))?;
                }
                FileStatus::Existed => {
                    cliclack::log::remark(format!(
                        "Exists  {}",
                        style("encryption key in OS keychain").dim()
                    ))?;
                }
            },
        }
        Self::render_file(&self.store, &store_path)?;
        Self::render_file(&self.deploy_index, &deploy_index_path)?;
        Self::render_file(&self.sync_index, &sync_index_path)?;

        if self.gitignore_updated {
            cliclack::log::success(format!("Updated {}", style(gitignore_path.display()).dim(),))?;
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
    ".esk/lock",
    ".esk/key-provider",
];

pub fn run(cwd: &Path, keychain: bool) -> Result<()> {
    let report = ensure_project(cwd, keychain)?;
    report.render(cwd)
}

fn ensure_project(cwd: &Path, keychain: bool) -> Result<InitReport> {
    let config_path = cwd.join("esk.yaml");
    let esk_dir = cwd.join(".esk");
    let store_path = esk_dir.join("store.enc");
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

    // Determine key provider and create store
    let (key_status, store_status) = if keychain {
        #[cfg(not(feature = "keychain"))]
        {
            bail!("keychain support requires the 'keychain' feature. Rebuild with: cargo install esk --features keychain");
        }
        #[cfg(feature = "keychain")]
        {
            ensure_keychain_store(cwd, &esk_dir, &store_path)?
        }
    } else {
        let key_path = esk_dir.join("store.key");
        if key_path.is_file() && store_path.is_file() {
            (KeyStatus::File(FileStatus::Existed), FileStatus::Existed)
        } else {
            let _store = SecretStore::load_or_create(cwd)?;
            (KeyStatus::File(FileStatus::Created), FileStatus::Created)
        }
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

#[cfg(feature = "keychain")]
fn ensure_keychain_store(
    cwd: &Path,
    esk_dir: &Path,
    store_path: &Path,
) -> Result<(KeyStatus, FileStatus)> {
    use crate::store::SecretStore;

    let marker_path = esk_dir.join("key-provider");
    let already_keychain = marker_path.is_file()
        && std::fs::read_to_string(&marker_path)
            .unwrap_or_default()
            .trim()
            == "keychain";

    let file_key_path = esk_dir.join("store.key");
    let has_file_key = file_key_path.is_file();

    if already_keychain && store_path.is_file() {
        // Already set up with keychain
        return Ok((
            KeyStatus::Keychain(FileStatus::Existed),
            FileStatus::Existed,
        ));
    }

    if has_file_key && store_path.is_file() {
        // Migration: read existing file key, store in keychain, keep file as backup
        let hex_str = std::fs::read_to_string(&file_key_path)
            .context("failed to read existing key file for migration")?;
        let key = hex::decode(hex_str.trim()).context("invalid key hex in existing key file")?;

        // Write marker first, then create provider from it to store the key
        crate::store::KeyProvider::write_marker(esk_dir, "keychain")?;
        let provider = crate::store::KeyProvider::from_marker(esk_dir)?;
        provider.store(&key)?;

        return Ok((
            KeyStatus::Keychain(FileStatus::Created),
            FileStatus::Existed,
        ));
    }

    // Fresh keychain setup
    let _store = SecretStore::load_or_create_with_provider(cwd, Some("keychain"))?;

    Ok((
        KeyStatus::Keychain(FileStatus::Created),
        FileStatus::Created,
    ))
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
