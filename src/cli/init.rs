use anyhow::{Context, Result};
use console::style;
use std::io::IsTerminal;
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

        Self::render_file(&self.config, &config_path)?;
        match &self.key {
            KeyStatus::File(status) => {
                Self::render_file(status, &esk_dir.join("store.key"))?;
            }
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
pub(crate) const ESK_GITIGNORE_ENTRIES: &[&str] = &[
    ".esk/store.key",
    ".esk/deploy-index.json",
    ".esk/sync-index.json",
    ".esk/lock",
    ".esk/key-provider",
];

pub fn run(cwd: &Path, keychain: bool) -> Result<()> {
    cliclack::intro(style("esk init").bold())?;
    let use_keychain = resolve_key_provider(cwd, keychain)?;
    let report = ensure_project(cwd, use_keychain)?;
    report.render(cwd)
}

/// Determines whether to use keychain or file-based key storage.
///
/// Priority:
/// 1. `--keychain` flag → keychain (skip prompt)
/// 2. TTY + keychain available → interactive prompt (pre-selects current provider on re-init)
/// 3. Re-init, non-TTY → preserve current provider from marker
/// 4. Default (CI, headless, non-TTY) → file
fn resolve_key_provider(cwd: &Path, keychain_flag: bool) -> Result<bool> {
    if keychain_flag {
        return Ok(true);
    }

    let esk_dir = cwd.join(".esk");
    let store_path = esk_dir.join("store.enc");

    let current_is_keychain = if store_path.is_file() {
        let marker_path = esk_dir.join("key-provider");
        Some(
            marker_path.is_file()
                && std::fs::read_to_string(&marker_path)
                    .unwrap_or_default()
                    .trim()
                    == "keychain",
        )
    } else {
        None
    };

    // TTY + keychain available: always prompt (fresh or re-init)
    if std::io::stdin().is_terminal() && keychain_available() {
        return prompt_key_storage(current_is_keychain.unwrap_or(true));
    }

    // Non-TTY re-init: preserve current provider
    if let Some(is_keychain) = current_is_keychain {
        return Ok(is_keychain);
    }

    Ok(false)
}

/// Probes whether the OS keychain is functional at runtime.
fn keychain_available() -> bool {
    #[cfg(feature = "keychain")]
    {
        let Ok(entry) = keyring::Entry::new("esk", "probe") else {
            return false;
        };
        match entry.get_secret() {
            Ok(_) | Err(keyring::Error::NoEntry) => true,
            Err(_) => false,
        }
    }
    #[cfg(not(feature = "keychain"))]
    {
        false
    }
}

fn prompt_key_storage(default_keychain: bool) -> Result<bool> {
    let use_keychain: bool = cliclack::select("Where should esk store the encryption key?")
        .items(&[
            (
                true,
                "OS keychain (recommended)",
                "key never stored on disk",
            ),
            (
                false,
                "File on disk",
                "saved to .esk/store.key (gitignored)",
            ),
        ])
        .initial_value(default_keychain)
        .interact()?;
    Ok(use_keychain)
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
      .env:
        pattern: "{app_path}/.env{env_suffix}.local"
        env_suffix:
          dev: ""
          prod: ".production"

    secrets:
      General:
        # EXAMPLE_SECRET:
        #   description: An example secret
        #   targets:
        #     .env: [web:dev, web:prod]
    "#;
        std::fs::write(&config_path, scaffold).context("failed to write esk.yaml")?;
        FileStatus::Created
    };

    // Determine key provider and create store
    let (key_status, store_status) = if keychain {
        ensure_keychain_store(cwd, &esk_dir, &store_path)?
    } else {
        ensure_file_store(cwd, &esk_dir, &store_path)?
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

fn ensure_file_store(
    cwd: &Path,
    esk_dir: &Path,
    store_path: &Path,
) -> Result<(KeyStatus, FileStatus)> {
    let key_path = esk_dir.join("store.key");
    let marker_path = esk_dir.join("key-provider");
    let was_keychain = marker_path.is_file()
        && std::fs::read_to_string(&marker_path)
            .unwrap_or_default()
            .trim()
            == "keychain";

    if was_keychain && store_path.is_file() {
        // Migration: read key from keychain, write to file
        let provider = crate::store::KeyProvider::from_marker(esk_dir)?;
        let key = provider.load()?;
        crate::store::KeyProvider::write_marker(esk_dir, "file")?;
        let file_provider = crate::store::KeyProvider::from_marker(esk_dir)?;
        file_provider.store(&key)?;
        return Ok((KeyStatus::File(FileStatus::Created), FileStatus::Existed));
    }

    if key_path.is_file() && store_path.is_file() {
        return Ok((KeyStatus::File(FileStatus::Existed), FileStatus::Existed));
    }

    let _store = SecretStore::load_or_create(cwd)?;
    Ok((KeyStatus::File(FileStatus::Created), FileStatus::Created))
}

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
        let key = zeroize::Zeroizing::new(
            hex::decode(hex_str.trim()).context("invalid key hex in existing key file")?,
        );

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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn resolve_flag_true_returns_keychain() {
        let dir = TempDir::new().unwrap();
        let result = resolve_key_provider(dir.path(), true).unwrap();
        assert!(result);
    }

    #[test]
    fn resolve_existing_project_no_marker_returns_file() {
        let dir = TempDir::new().unwrap();
        let esk_dir = dir.path().join(".esk");
        std::fs::create_dir_all(&esk_dir).unwrap();
        std::fs::write(esk_dir.join("store.enc"), b"dummy").unwrap();
        let result = resolve_key_provider(dir.path(), false).unwrap();
        assert!(!result);
    }

    #[test]
    fn resolve_existing_project_file_marker_returns_file() {
        let dir = TempDir::new().unwrap();
        let esk_dir = dir.path().join(".esk");
        std::fs::create_dir_all(&esk_dir).unwrap();
        std::fs::write(esk_dir.join("store.enc"), b"dummy").unwrap();
        std::fs::write(esk_dir.join("key-provider"), "file\n").unwrap();
        let result = resolve_key_provider(dir.path(), false).unwrap();
        assert!(!result);
    }

    #[test]
    fn resolve_existing_project_keychain_marker_returns_keychain() {
        let dir = TempDir::new().unwrap();
        let esk_dir = dir.path().join(".esk");
        std::fs::create_dir_all(&esk_dir).unwrap();
        std::fs::write(esk_dir.join("store.enc"), b"dummy").unwrap();
        std::fs::write(esk_dir.join("key-provider"), "keychain\n").unwrap();
        let result = resolve_key_provider(dir.path(), false).unwrap();
        assert!(result);
    }

    #[test]
    fn resolve_fresh_project_non_tty_returns_file() {
        // CI / non-TTY: no store.enc, no flag → file
        let dir = TempDir::new().unwrap();
        let result = resolve_key_provider(dir.path(), false).unwrap();
        assert!(!result);
    }
}
