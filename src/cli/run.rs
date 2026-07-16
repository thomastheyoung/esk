use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::process::Command;

use crate::config::{Config, ResolvedSecret};
use crate::store::{SecretStore, StorePayload};

pub struct RunOptions<'a> {
    pub env: &'a str,
    pub app: Option<&'a str>,
    pub command: &'a [String],
}

pub fn run(config: &Config, opts: &RunOptions<'_>) -> Result<i32> {
    config.validate_env(opts.env)?;
    if let Some(app) = opts.app {
        crate::store::validate_app(app)?;
        if !config.apps.contains_key(app) {
            bail!("unknown app '{app}'");
        }
    }
    let Some(program) = opts.command.first() else {
        bail!("a command is required after `--`");
    };

    let store = SecretStore::open(&config.root)?;
    let payload = store.payload()?;
    let resolved = config.resolve_secrets()?;
    let secrets = collect_env_secrets(&resolved, &payload, opts.env, opts.app);

    let mut command = Command::new(program);
    command.args(&opts.command[1..]);
    command.envs(secrets);
    let status = command
        .status()
        .with_context(|| format!("failed to start command '{program}'"))?;
    Ok(child_exit_code(status))
}

fn child_exit_code(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;

        status.signal().map_or(1, |signal| 128 + signal)
    }

    #[cfg(not(unix))]
    {
        1
    }
}

fn collect_env_secrets(
    resolved: &[ResolvedSecret],
    payload: &StorePayload,
    env: &str,
    app: Option<&str>,
) -> BTreeMap<String, String> {
    let mut secrets = BTreeMap::new();
    for secret in resolved {
        let targeted = secret.targets.iter().any(|target| {
            target.environment == env
                && app.is_none_or(|requested| target.app.as_deref() == Some(requested))
        });
        if targeted {
            let composite = format!("{}:{env}", secret.key);
            if let Some(value) = payload.secrets.get(&composite) {
                secrets.insert(secret.key.clone(), value.clone());
            }
        }
    }
    secrets
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ResolvedTarget;

    fn secret(key: &str, app: Option<&str>, env: &str) -> ResolvedSecret {
        ResolvedSecret {
            key: key.to_string(),
            group: "Test".to_string(),
            description: None,
            targets: vec![ResolvedTarget {
                service: ".env".to_string(),
                app: app.map(str::to_string),
                environment: env.to_string(),
            }],
            validate: None,
            required: crate::config::Required::default(),
            allow_empty: false,
        }
    }

    #[test]
    fn filters_values_by_environment_and_app() {
        let resolved = vec![
            secret("WEB_KEY", Some("web"), "dev"),
            secret("API_KEY", Some("api"), "dev"),
            secret("PROD_KEY", Some("web"), "prod"),
        ];
        let payload = StorePayload {
            secrets: BTreeMap::from([
                ("WEB_KEY:dev".to_string(), "web-value".to_string()),
                ("API_KEY:dev".to_string(), "api-value".to_string()),
                ("PROD_KEY:prod".to_string(), "prod-value".to_string()),
            ]),
            ..Default::default()
        };

        let web = collect_env_secrets(&resolved, &payload, "dev", Some("web"));
        assert_eq!(web.get("WEB_KEY").map(String::as_str), Some("web-value"));
        assert!(!web.contains_key("API_KEY"));
        assert!(!web.contains_key("PROD_KEY"));

        let all_dev = collect_env_secrets(&resolved, &payload, "dev", None);
        assert_eq!(all_dev.len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn preserves_signal_termination_as_shell_exit_code() {
        let status = Command::new("sh")
            .args(["-c", "kill -TERM $$"])
            .status()
            .unwrap();

        assert_eq!(child_exit_code(status), 128 + 15);
    }
}
