use anyhow::{bail, Result};

use crate::config::Config;
use crate::store::SecretStore;

pub fn run(config: &Config, key: &str, env: &str) -> Result<()> {
    config.validate_env(env)?;

    let store = SecretStore::open(&config.root)?;
    match store.get(key, env)? {
        Some(value) => println!("{value}"),
        None => bail!("no value for {key}:{env}"),
    }
    Ok(())
}
