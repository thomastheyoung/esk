pub mod cloud_file;
pub mod onepassword;

use anyhow::Result;
use std::collections::BTreeMap;

use crate::adapters::CommandRunner;
use crate::config::Config;
use crate::store::StorePayload;

/// A storage plugin that stores/retrieves the full secret state.
///
/// Unlike sync adapters (which deploy secrets to targets), plugins
/// store or backup the entire secret list as a source of truth.
pub trait StoragePlugin {
    fn name(&self) -> &str;

    /// Validate that external dependencies are available before push/pull.
    fn preflight(&self) -> Result<()> {
        Ok(())
    }

    /// Push store state to this plugin for a given environment.
    fn push(&self, payload: &StorePayload, config: &Config, env: &str) -> Result<()>;

    /// Pull store state from this plugin for a given environment.
    /// Returns (composite_key_secrets, version), or None if nothing stored.
    fn pull(&self, config: &Config, env: &str) -> Result<Option<(BTreeMap<String, String>, u64)>>;
}

/// Health status of a configured plugin.
pub struct PluginHealth {
    pub name: String,
    pub ok: bool,
    pub message: String,
}

/// Check the health of all configured plugins without filtering.
/// Returns one entry per configured plugin with preflight pass/fail.
pub fn check_plugin_health(config: &Config, runner: &dyn CommandRunner) -> Vec<PluginHealth> {
    let mut results = Vec::new();

    if let Some(op_config) = config.onepassword_plugin_config() {
        let plugin = onepassword::OnePasswordPlugin::new(config, op_config, runner);
        match plugin.preflight() {
            Ok(()) => results.push(PluginHealth {
                name: "onepassword".to_string(),
                ok: true,
                message: "op authenticated".to_string(),
            }),
            Err(e) => results.push(PluginHealth {
                name: "onepassword".to_string(),
                ok: false,
                message: e.to_string(),
            }),
        }
    }

    for (name, cf_config) in config.cloud_file_plugin_configs() {
        let plugin = cloud_file::CloudFilePlugin::new(name.clone(), config.project.clone(), cf_config);
        match plugin.preflight() {
            Ok(()) => results.push(PluginHealth {
                name,
                ok: true,
                message: "directory accessible".to_string(),
            }),
            Err(e) => results.push(PluginHealth {
                name,
                ok: false,
                message: e.to_string(),
            }),
        }
    }

    results
}

/// Build all configured plugins from the config.
/// Runs preflight checks and filters out plugins that fail, printing warnings.
pub fn build_plugins<'a>(
    config: &'a Config,
    runner: &'a dyn CommandRunner,
) -> Vec<Box<dyn StoragePlugin + 'a>> {
    let mut plugins: Vec<Box<dyn StoragePlugin + 'a>> = Vec::new();

    let mut candidates: Vec<Box<dyn StoragePlugin + 'a>> = Vec::new();

    if let Some(op_config) = config.onepassword_plugin_config() {
        candidates.push(Box::new(onepassword::OnePasswordPlugin::new(
            config, op_config, runner,
        )));
    }

    for (name, cf_config) in config.cloud_file_plugin_configs() {
        candidates.push(Box::new(cloud_file::CloudFilePlugin::new(
            name,
            config.project.clone(),
            cf_config,
        )));
    }

    for plugin in candidates {
        match plugin.preflight() {
            Ok(()) => plugins.push(plugin),
            Err(e) => {
                let _ = cliclack::log::warning(format!(
                    "Skipping {} plugin: {}",
                    plugin.name(),
                    e
                ));
            }
        }
    }

    plugins
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::{CommandOpts, CommandOutput};


    struct DummyRunner;
    impl CommandRunner for DummyRunner {
        fn run(&self, _program: &str, _args: &[&str], _opts: CommandOpts) -> Result<CommandOutput> {
            Ok(CommandOutput {
                success: true,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    #[test]
    fn build_plugins_empty_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lockbox.yaml");
        std::fs::write(&path, "project: x\nenvironments: [dev]").unwrap();
        let config = Config::load(&path).unwrap();
        let runner = DummyRunner;
        let plugins = build_plugins(&config, &runner);
        assert!(plugins.is_empty());
    }

    #[test]
    fn build_plugins_with_onepassword() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
plugins:
  onepassword:
    vault: V
    item_pattern: test
"#;
        let path = dir.path().join("lockbox.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let runner = DummyRunner;
        let plugins = build_plugins(&config, &runner);
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name(), "onepassword");
    }

    #[test]
    fn build_plugins_with_cloud_file() {
        let dir = tempfile::tempdir().unwrap();
        let cloud_dir = tempfile::tempdir().unwrap();
        let yaml = format!(
            r#"
project: x
environments: [dev]
plugins:
  dropbox:
    type: cloud_file
    path: {}
    format: encrypted
"#,
            cloud_dir.path().display()
        );
        let path = dir.path().join("lockbox.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let runner = DummyRunner;
        let plugins = build_plugins(&config, &runner);
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name(), "dropbox");
    }

    #[test]
    fn build_plugins_filters_failing_preflight() {
        let dir = tempfile::tempdir().unwrap();
        // onepassword will fail (runner fails), cloud_file with existing dir will pass
        let cloud_dir = tempfile::tempdir().unwrap();
        let yaml = format!(
            r#"
project: x
environments: [dev]
plugins:
  onepassword:
    vault: V
    item_pattern: test
  testcloud:
    type: cloud_file
    path: {}
    format: cleartext
"#,
            cloud_dir.path().display()
        );
        let path = dir.path().join("lockbox.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();

        struct FailRunner;
        impl CommandRunner for FailRunner {
            fn run(&self, _: &str, _: &[&str], _: CommandOpts) -> Result<CommandOutput> {
                anyhow::bail!("not found")
            }
        }

        let plugins = build_plugins(&config, &FailRunner);
        // onepassword fails preflight, cloud_file with existing dir passes
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name(), "testcloud");
    }

    #[test]
    fn build_plugins_filters_cloud_file_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
plugins:
  testcloud:
    type: cloud_file
    path: /nonexistent/path/nowhere
    format: cleartext
"#;
        let path = dir.path().join("lockbox.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let runner = DummyRunner;
        let plugins = build_plugins(&config, &runner);
        assert!(plugins.is_empty());
    }

    #[test]
    fn check_plugin_health_op_ok() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
plugins:
  onepassword:
    vault: V
    item_pattern: test
"#;
        let path = dir.path().join("lockbox.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();

        let health = check_plugin_health(&config, &DummyRunner);
        assert_eq!(health.len(), 1);
        assert!(health[0].ok);
        assert_eq!(health[0].name, "onepassword");
    }

    #[test]
    fn check_plugin_health_op_fails() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
plugins:
  onepassword:
    vault: V
    item_pattern: test
"#;
        let path = dir.path().join("lockbox.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();

        struct FailRunner;
        impl CommandRunner for FailRunner {
            fn run(&self, _: &str, _: &[&str], _: CommandOpts) -> Result<CommandOutput> {
                anyhow::bail!("op not found")
            }
        }

        let health = check_plugin_health(&config, &FailRunner);
        assert_eq!(health.len(), 1);
        assert!(!health[0].ok);
        assert!(health[0].message.contains("op) is not installed"));
    }

    #[test]
    fn check_plugin_health_no_plugins() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = "project: x\nenvironments: [dev]";
        let path = dir.path().join("lockbox.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();

        let health = check_plugin_health(&config, &DummyRunner);
        assert!(health.is_empty());
    }

    #[test]
    fn build_plugins_mixed() {
        let dir = tempfile::tempdir().unwrap();
        let cloud_dir1 = tempfile::tempdir().unwrap();
        let cloud_dir2 = tempfile::tempdir().unwrap();
        let yaml = format!(
            r#"
project: x
environments: [dev]
plugins:
  onepassword:
    vault: V
    item_pattern: test
  dropbox:
    type: cloud_file
    path: {}
    format: encrypted
  gdrive:
    type: cloud_file
    path: {}
    format: cleartext
"#,
            cloud_dir1.path().display(),
            cloud_dir2.path().display()
        );
        let path = dir.path().join("lockbox.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let runner = DummyRunner;
        let plugins = build_plugins(&config, &runner);
        assert_eq!(plugins.len(), 3);
    }
}
