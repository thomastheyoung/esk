# Idiomatic cleanup spec

Targeted cleanup to improve code elegance using idiomatic Rust patterns. No new abstractions — only "put behavior where it belongs" and "use the standard library."

## Guiding principles

- Code should be straightforward to read — any dev infers what it does at a glance
- Every target/remote adapter is self-sufficient (one file, no cross-adapter dependencies)
- Prefer idiomatic Rust over custom helpers
- No abstraction that hides what's happening

## Changes

### 1. `CommandOutput::check()` — method on the type

**27 call sites** across all targets repeat this 4-line block:

```rust
if !output.success {
    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow::bail!("wrangler secret put failed for {key}: {stderr}");
}
```

Add a method to `CommandOutput` in `src/targets/mod.rs`:

```rust
impl CommandOutput {
    pub fn check(&self, command: &str, key: &str) -> Result<()> {
        if !self.success {
            let stderr = String::from_utf8_lossy(&self.stderr);
            anyhow::bail!("{command} failed for {key}: {stderr}");
        }
        Ok(())
    }
}
```

Each call site becomes:

```rust
let output = self.runner.run("wrangler", &args, opts)
    .with_context(|| format!("failed to run wrangler for {key}"))?;
output.check("wrangler secret put", key)?;
```

Adapters still own their commands, args, and flow. The method just removes the identical error-handling stutter.

### 2. Delete `append_env_flags`, use `.extend()`

**24 call sites** use this two-function ceremony:

```rust
let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
let mut args: Vec<&str> = vec!["secrets", "import", "-a", fly_app];
append_env_flags(&mut args, &flag_parts);
```

`append_env_flags` is a function that wraps a for loop. Replace with stdlib:

```rust
args.extend(flag_parts.iter().map(String::as_str));
```

Delete `append_env_flags` from `src/targets/mod.rs`. Remove it from all `use` imports across target files.

### 3. Delete `StatusTheme` and `ListTheme`, use `EskTheme`

`StatusTheme` (`src/cli/status.rs:17-27`) and `ListTheme` (if present in `src/cli/list.rs`) are character-for-character identical to `EskTheme` in `src/ui.rs`. The comment even says "same as list.rs."

`EskTheme` is already set globally via `cliclack::set_theme(EskTheme)` in `main.rs:15`. Verify whether these per-module themes are actually applied (via a second `set_theme` call) or are just dead code. Either way, delete them.

### 4. Unify `relative_time` into `ui::format_relative_time`

Two implementations of relative time formatting:

- `ui::format_relative_time` (`src/ui.rs:26-43`) — falls back to absolute date after 24h
- `status::relative_time` (`src/cli/status.rs:791-810`) — returns `Nd ago` for days, empty string on parse failure

Extend `ui::format_relative_time` to handle days (like the status.rs version does), then delete the status.rs copy. Use one function everywhere.

Target behavior:

```rust
pub fn format_relative_time(ts: &str) -> String {
    let Ok(dt) = DateTime::parse_from_rfc3339(ts) else {
        return ts.to_string();
    };
    let delta = Utc::now().signed_duration_since(dt.with_timezone(&Utc));

    if delta.num_seconds() < 60 { "just now".to_string() }
    else if delta.num_minutes() < 60 { format!("{}m ago", delta.num_minutes()) }
    else if delta.num_hours() < 24 { format!("{}h ago", delta.num_hours()) }
    else if delta.num_days() < 30 { format!("{}d ago", delta.num_days()) }
    else { dt.format("%Y-%m-%d %H:%M").to_string() }
}
```

### 5. `Config::find_and_load()` — method on the type

**8 call sites** in `main.rs` repeat:

```rust
let cwd = std::env::current_dir()?;
let config_path = Config::find(&cwd)?;
let config = Config::load(&config_path)?;
```

Add to `src/config.rs`:

```rust
impl Config {
    pub fn find_and_load() -> Result<Config> {
        let cwd = std::env::current_dir()?;
        let config_path = Self::find(&cwd)?;
        Self::load(&config_path)
    }
}
```

Each call site in main.rs becomes `let config = Config::find_and_load()?;`

### 6. `Config::validate_env()` — method on the type

**4 call sites** (set, delete, get, sync) repeat:

```rust
if !config.environments.contains(&env.to_string()) {
    bail!("{}", suggest::unknown_env(env, &config.environments));
}
```

Add to `src/config.rs`:

```rust
impl Config {
    pub fn validate_env(&self, env: &str) -> Result<()> {
        if !self.environments.contains(&env.to_string()) {
            anyhow::bail!("{}", crate::suggest::unknown_env(env, &self.environments));
        }
        Ok(())
    }
}
```

Each call site becomes `config.validate_env(env)?;`

### 7. `sync::run_with_runner` accepts `SyncOptions`

Currently has 8 positional parameters with `#[allow(clippy::too_many_arguments)]`:

```rust
pub fn run_with_runner(config, env, only, dry_run, bail, force, auto_deploy, prefer, runner)
```

Change to accept the struct that already exists:

```rust
pub fn run_with_runner(config: &Config, opts: &SyncOptions, runner: &dyn CommandRunner)
```

Matches the pattern `deploy.rs` already uses with `DeployOptions`.

### 8. `set::run` / `set::run_with_runner` gets `SetOptions`

Same problem — 8 positional params with `#[allow(clippy::too_many_arguments)]`. Introduce:

```rust
pub struct SetOptions<'a> {
    pub key: &'a str,
    pub env: &'a str,
    pub value: Option<&'a str>,
    pub group: Option<&'a str>,
    pub no_sync: bool,
    pub bail: bool,
    pub skip_validation: bool,
}
```

## Out of scope

These were considered and intentionally excluded:

- **Breaking `deploy.rs::run_with_runner` (665 lines) into sub-functions** — worth doing but is a standalone refactor, not an elegance fix
- **Extracting auto-sync+deploy from set/delete** — the two blocks differ enough (skip_validation, context) that a shared function would need too many params
- **Shared preflight helpers across adapters** — violates adapter self-sufficiency
- **Shared test helpers across adapters** — violates adapter self-sufficiency
- **Macros for target_candidates() or struct definitions** — hides what's happening
- **Generic JsonIndex for deploy/sync trackers** — marginal gain, types will likely diverge
