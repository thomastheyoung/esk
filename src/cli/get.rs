use anyhow::{bail, Result};

use crate::config::Config;
use crate::store::SecretStore;

pub fn run(config: &Config, key: &str, env: &str) -> Result<()> {
    if !config.environments.contains(&env.to_string()) {
        bail!("unknown environment '{env}'. Valid: {}", config.environments.join(", "));
    }

    let store = SecretStore::open(&config.root)?;
    match store.get(key, env)? {
        Some(value) => println!("{value}"),
        None => bail!("no value for {key}:{env}"),
    }
    Ok(())
}
