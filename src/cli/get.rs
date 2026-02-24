use anyhow::{bail, Result};

use crate::config::Config;
use crate::store::SecretStore;
use crate::suggest;

pub fn run(config: &Config, key: &str, env: &str) -> Result<()> {
    if !config.environments.contains(&env.to_string()) {
        bail!("{}", suggest::unknown_env(env, &config.environments));
    }

    let store = SecretStore::open(&config.root)?;
    match store.get(key, env)? {
        Some(value) => println!("{value}"),
        None => bail!("no value for {key}:{env}"),
    }
    Ok(())
}
