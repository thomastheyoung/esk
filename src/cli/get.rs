use anyhow::{bail, Result};
use std::fmt::Write;

use crate::config::Config;
use crate::store::SecretStore;

pub fn run(config: &Config, key: &str, env: &str) -> Result<()> {
    config.validate_env(env)?;

    let store = SecretStore::open(&config.root)?;
    match store.get(key, env)? {
        Some(value) => println!("{value}"),
        None if config.find_secret(key).is_none() => {
            bail!("{}", unknown_secret_message(config, key))
        }
        None => bail!("no value for {key}:{env}"),
    }
    Ok(())
}

pub(crate) fn unknown_secret_message(config: &Config, key: &str) -> String {
    let candidates: Vec<String> = config
        .secrets
        .values()
        .flat_map(|secrets| secrets.keys().cloned())
        .collect();
    let mut message = format!("secret '{key}' is not defined in esk.yaml");
    if let Some(suggestion) = crate::suggest::closest(key, &candidates) {
        let _ = write!(message, " Did you mean '{suggestion}'?");
    }
    message
}
