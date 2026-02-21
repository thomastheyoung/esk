use anyhow::{Context, Result};
use console::style;
use std::path::Path;

use crate::store::SecretStore;
use crate::tracker::SyncIndex;

pub fn run(cwd: &Path) -> Result<()> {
    let config_path = cwd.join("lockbox.yaml");
    let store_path = cwd.join(".secrets.enc");
    let key_path = cwd.join(".secrets.key");
    let sync_index_path = cwd.join(".sync-index.json");

    // Scaffold lockbox.yaml if it doesn't exist
    if !config_path.is_file() {
        let scaffold = r#"project: myapp

environments: [dev, prod]

apps:
  web:
    path: apps/web

adapters:
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
        std::fs::write(&config_path, scaffold).context("failed to write lockbox.yaml")?;
        println!("  {} {}", style("created").green(), config_path.display());
    } else {
        println!("  {} {}", style("exists").dim(), config_path.display());
    }

    // Create store (generates key + empty encrypted store)
    if !key_path.is_file() || !store_path.is_file() {
        let _store = SecretStore::load_or_create(cwd)?;
        if key_path.is_file() {
            println!("  {} {}", style("created").green(), key_path.display());
        }
        if store_path.is_file() {
            println!("  {} {}", style("created").green(), store_path.display());
        }
    } else {
        println!("  {} {}", style("exists").dim(), key_path.display());
        println!("  {} {}", style("exists").dim(), store_path.display());
    }

    // Create empty sync index
    if !sync_index_path.is_file() {
        let index = SyncIndex::new(&sync_index_path);
        index.save()?;
        println!(
            "  {} {}",
            style("created").green(),
            sync_index_path.display()
        );
    } else {
        println!("  {} {}", style("exists").dim(), sync_index_path.display());
    }

    // Remind about .gitignore
    let gitignore_path = cwd.join(".gitignore");
    if gitignore_path.is_file() {
        let contents = std::fs::read_to_string(&gitignore_path)?;
        if !contents.contains(".secrets.key") {
            println!(
                "\n  {} add {} to your .gitignore",
                style("reminder:").yellow(),
                style(".secrets.key").bold()
            );
        }
    }

    println!(
        "\n  {} run `lockbox set <KEY> --env <ENV>` to add secrets",
        style("next:").cyan()
    );
    Ok(())
}
