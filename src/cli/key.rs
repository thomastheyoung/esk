use anyhow::Result;

use crate::config::Config;
use crate::store::SecretStore;

pub fn rotate(config: &Config) -> Result<()> {
    let store = SecretStore::open(&config.root)?;
    store.rotate_key()?;
    cliclack::log::success("Encryption key rotated; store re-encrypted")?;
    Ok(())
}
