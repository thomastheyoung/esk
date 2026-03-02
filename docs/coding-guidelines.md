# Coding guidelines

Principles for writing idiomatic, elegant Rust in this project. A decision-making framework, not a style guide.

## Core philosophy

1. **Straightforward to read** — any developer infers what code does at a glance
2. **Let the type system work** — encode invariants at compile time, not runtime
3. **Put behavior where it belongs** — methods on the type, not free functions that take the type
4. **Use the standard library** — prefer idiomatic Rust over custom helpers
5. **No abstraction that hides what's happening** — if a reader has to jump to another file to understand the flow, the abstraction is wrong
6. **Every adapter is self-sufficient** — one file, no cross-adapter dependencies

## Use the type system

### Enums over booleans

A `bool` communicates nothing at the call site. When a function takes two booleans, every call site is a puzzle. Use enums when the meaning isn't obvious from context:

```rust
// Bad: what does (true, false) mean?
deploy_secret(key, value, true, false);

// Good: self-documenting
deploy_secret(key, value, Force::Yes, DryRun::No);
```

For options structs with named fields (`DeployOptions { force: true, dry_run: false }`), booleans are fine — the field name provides the context a bare `true` lacks.

### Make invalid states unrepresentable

When two fields are coupled — one only meaningful when the other has a specific value — that's an enum:

```rust
// Bad: error only meaningful when !success
struct DeployResult { success: bool, error: Option<String> }

// Good: each variant carries exactly what it needs
enum DeployOutcome { Success, Failed(String) }
```

### Newtypes for domain distinctions

When two `String` parameters could be swapped silently, a newtype catches the bug at compile time with zero runtime cost:

```rust
struct SecretKey(String);
struct Environment(String);

// Compiler prevents: store.get(env, key) — wrong order caught at compile time
fn get(&self, key: &SecretKey, env: &Environment) -> Option<&str>;
```

Only worth it when confusion is plausible. `deploy_secret(key: &str, value: &str)` is clear enough — `key` and `value` aren't easily confused.

## Pattern matching

### `let-else` for early returns

Cleaner than `match` or `if let` + `else` when the else branch diverges:

```rust
let Some(env) = opts.env else {
    bail!("--env is required for deploy");
};
```

### `matches!` for boolean checks

```rust
// Instead of: match mode { Mode::Debug | Mode::Verbose => true, _ => false }
if matches!(mode, Mode::Debug | Mode::Verbose) {
    enable_logging();
}
```

### Slice patterns

```rust
match users.as_slice() {
    [] => bail!("no users found"),
    [user] => Ok(user),
    [first, ..] => bail!("expected one user, found {}", users.len()),
}
```

### Or-patterns reduce duplication

```rust
match format {
    Format::Integer | Format::Number => validate_numeric(value),
    Format::Url | Format::Email => validate_uri(value),
    _ => Ok(()),
}
```

### Destructure where it helps

```rust
// Unpack options struct at the top of a function
let DeployOptions { env, force, dry_run, verbose, .. } = *opts;
```

## Iterators

### When to use iterator chains

Prefer chains when each step is a single, clear operation:

```rust
let deployed: Vec<_> = results.iter()
    .filter(|r| r.success)
    .map(|r| &r.key)
    .collect();
```

### When to use loops

Prefer explicit loops when the body is complex, has side effects, or the chain would exceed ~5 adapters:

```rust
for result in &results {
    if !result.success {
        log::error(result.key, result.error);
        if result.is_critical() { break; }
    }
}
```

### Key patterns

**Collecting Results** — short-circuits on first error:

```rust
let parsed: Vec<Config> = files.iter()
    .map(|f| Config::load(f))
    .collect::<Result<Vec<_>>>()?;
```

**`filter_map` for Option unwrapping:**

```rust
let values: Vec<_> = keys.iter()
    .filter_map(|k| store.get(k))
    .collect();
```

**Use terminal methods** — `sum()`, `any()`, `all()`, `find()`, `count()`, `min()`, `max()` — instead of manual accumulation loops.

## Ownership and borrowing

### Accept the most general reference

```rust
// Good: accepts &str, &String, &Cow<str> via deref coercion
fn process(name: &str) { ... }

// Good: accepts &[T], &Vec<T>, &[T; N]
fn process_all(items: &[SecretValue]) { ... }

// Good: accepts &Path, &PathBuf
fn load(path: &Path) -> Result<Config> { ... }
```

### Cow for conditional allocation

When a function usually returns the input unchanged:

```rust
fn normalize_key(key: &str) -> Cow<'_, str> {
    if key.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
        Cow::Borrowed(key)
    } else {
        Cow::Owned(key.to_ascii_uppercase().replace('-', "_"))
    }
}
```

### Clone is fine in non-hot paths

This is a CLI that shells out to external processes. The subprocess spawn costs orders of magnitude more than a `.clone()`. Don't twist code into knots to avoid an allocation in code that's about to fork a process. Use `.clone()` freely when it simplifies the code — question each clone, but don't agonize.

### Temporary mutability scoping

```rust
let sorted = {
    let mut v = get_items();
    v.sort();
    v  // becomes immutable in outer scope
};
```

## Behavior belongs on the type

When multiple call sites repeat the same logic involving a type's data, that logic is a method:

```rust
// Wrong: free function operating on the type's data
fn format_target(target: &ResolvedTarget) -> String { ... }

// Right: method on the type
impl ResolvedTarget {
    pub fn label(&self) -> String { ... }
}
```

The test: if you need the type's fields to compute the result, it's a method. If it appears in 2+ places, it definitely is.

### Return references when callers don't need ownership

```rust
// Wrong: clones on every call
pub fn find_secret(&self, key: &str) -> Option<(String, &SecretDef)>

// Right: borrow from self
pub fn find_secret(&self, key: &str) -> Option<(&str, &SecretDef)>
```

## Use the standard library

Before writing a helper, check if Rust already has it:

| Instead of                                          | Use                                                           |
| --------------------------------------------------- | ------------------------------------------------------------- | --- | ----- |
| Custom `append_env_flags` fn wrapping a for loop    | `args.extend(flag_parts.iter().map(String::as_str))`          |
| 8-line `center()` with manual padding               | `format!("{s:^width$}")`                                      |
| `vec!["a".to_string(), format!(...)]` in flat_map   | `["a".into(), format!(...)]` — array literal, stack-allocated |
| Manual `Default` with all `BTreeMap::new()`         | `#[derive(Default)]`                                          |
| Manual `Display` + `impl Error`                     | `#[derive(thiserror::Error)]`                                 |
| `match opt { Some(v) => v, None => return Ok(()) }` | `let Some(v) = opt else { return Ok(()); };`                  |
| Manual loop + push on fallible operations           | `.map(\|v\| ...).collect::<Result<Vec<_>>>()`                 |
| `if x.is_some() { x.unwrap() }`                     | `if let Some(v) = x { ... }`                                  |
| Manual `HashMap` entry check + insert               | `map.entry(key).or_insert_with(                               |     | ...)` |

## Avoid redundant code

**Tail-return instead of `?; Ok(())`**. When the last expression already returns `Result<()>`, don't unwrap it just to rewrap it:

```rust
// Wrong
output.check("cmd", key)?;
Ok(())

// Right
output.check("cmd", key)
```

**Chain where there's no intermediate logic:**

```rust
// Right: no logic between run and check
self.runner.run("cmd", &args, opts)
    .with_context(|| format!("failed to run cmd for {key}"))?
    .check("cmd", key)

// Right: keep separate when there IS intermediate logic
let output = self.runner.run("cmd", &args, opts)?;
if output.stderr_contains("not found") { return Ok(()); }
output.check("cmd", key)
```

**Avoid allocations for comparisons:**

```rust
// Wrong: allocates a String just to check membership
self.environments.contains(&env.to_string())

// Right: compare by reference
self.environments.iter().any(|e| e == env)
```

## Data-driven over repetitive if-chains

When the same structural pattern repeats N times with only data changing:

```rust
// Wrong: 14 identical if-blocks
if self.targets.dotenv.is_some() { names.push(".env"); }
if self.targets.cloudflare.is_some() { names.push("cloudflare"); }

// Right: declarative, scannable, hard to get wrong
[
    (".env", self.targets.dotenv.is_some()),
    ("cloudflare", self.targets.cloudflare.is_some()),
    // ...
]
.into_iter()
.filter(|(_, present)| *present)
.map(|(name, _)| name)
.collect()
```

This is not the same as a macro — the logic is still visible inline. It just removes the structural repetition.

## Conversion traits

### Implement `From`, not `Into`

The blanket impl gives you `Into` for free. Only use `Into` in generic bounds:

```rust
impl From<serde_yaml::Error> for ConfigError {
    fn from(e: serde_yaml::Error) -> Self { ConfigError::Parse(e) }
}
// or: #[derive(thiserror::Error)] with #[from]

// In bounds, use Into for ergonomic APIs:
fn set(&mut self, key: impl Into<String>, value: impl Into<String>);
```

### `TryFrom` for validated construction

```rust
impl TryFrom<&str> for SecretKey {
    type Error = anyhow::Error;
    fn try_from(s: &str) -> Result<Self> {
        validate_key_format(s)?;
        Ok(SecretKey(s.to_string()))
    }
}
```

### Naming conventions for conversions

| Prefix  | Cost      | Ownership                | Example                |
| ------- | --------- | ------------------------ | ---------------------- |
| `as_`   | Free      | Borrowed &rarr; borrowed | `str::as_bytes()`      |
| `to_`   | Allocates | Borrowed &rarr; owned    | `str::to_lowercase()`  |
| `into_` | Variable  | Owned &rarr; owned       | `String::into_bytes()` |
| `from_` | Variable  | Constructor              | `String::from_utf8()`  |

## Error handling

Three patterns, each with a specific purpose:

| Pattern                                         | When to use                                                                                                                      |
| ----------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| `bail!("message")`                              | The code itself generates the error — validation failures, user aborts. Include remediation: `"Use --skip-validation to bypass"` |
| `.context("message")?`                          | Wrapping a library/IO error with static context                                                                                  |
| `.with_context(\|\| format!("message {var}"))?` | Wrapping a library/IO error with dynamic context                                                                                 |

For typed errors that callers need to `downcast_ref` and inspect, use `thiserror`. Everywhere else, use `anyhow`.

**Error messages should tell the user what to do next:**

```rust
// Wrong: states the problem, leaves user stuck
bail!("wrangler is not authenticated");

// Right: states the problem and the fix
bail!("wrangler is not authenticated. Run: wrangler login");
```

### `unwrap` and `expect`

Never `unwrap()` on fallible operations that can fail at runtime (user input, file I/O, network). Use `?` and propagate.

`unwrap()` is acceptable when panic signals a bug — a runtime invariant violation where failure means the program logic is wrong:

```rust
// OK: static regex can't fail to compile
let re = Regex::new(r"^\d+$").unwrap();

// OK: we just verified the condition
assert!(items.len() == 1);
let item = items.pop().unwrap();
```

Prefer `expect("reason")` when the invariant isn't obvious from surrounding code.

## Serde patterns

### `skip_serializing_if` to keep output clean

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub error: Option<String>,

#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
pub tombstones: BTreeMap<String, u64>,
```

### `rename_all` for wire format consistency

```rust
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeployStatus { Success, PartialFailure, Failed }
```

### Untagged enums for polymorphic YAML

```rust
#[derive(Deserialize)]
#[serde(untagged)]
pub enum Required {
    Bool(bool),          // required: true
    Envs(Vec<String>),   // required: [prod, staging]
}
```

### `flatten` for shared config

```rust
#[derive(Deserialize)]
pub struct CloudflareTargetConfig {
    pub mode: String,
    #[serde(flatten)]
    pub common: CommonTargetConfig,
}
```

## Function signatures

**Use an options struct at 4+ parameters**, especially when booleans are involved. Borrow with a lifetime when the struct doesn't need to own its data:

```rust
pub struct DeployOptions<'a> {
    pub env: Option<&'a str>,
    pub force: bool,
    pub dry_run: bool,
}
```

Use `Default::default()` and struct update syntax for partial initialization:

```rust
CommandOpts {
    stdin: Some(value.as_bytes().to_vec()),
    ..Default::default()
}
```

**The `run` / `run_with_runner` pattern** — every CLI command that touches external services has both:

```rust
pub fn run(config: &Config, opts: &DeployOptions<'_>) -> Result<()> {
    run_with_runner(config, opts, &RealCommandRunner)
}

pub fn run_with_runner(
    config: &Config,
    opts: &DeployOptions<'_>,
    runner: &dyn CommandRunner,
) -> Result<()> {
    // actual implementation
}
```

`run()` is the public API. `run_with_runner()` is the testable seam. Use `&dyn CommandRunner` (dynamic dispatch) intentionally — the vtable lookup is negligible compared to process spawn cost, and it keeps the API clean.

## When not to abstract

Resist the urge to DRY when:

- **Only 2 call sites** — wait for a third before extracting
- **The abstraction needs as many params as the code it replaces** — the cure is worse than the disease
- **The implementations will likely diverge** — premature unification creates coupling
- **The inline version is already clear** — `format!("{key}:{env}")` doesn't need a `composite_key()` function
- **A helper is too trivial to justify** — a `plural(n)` function for `if n == 1 { "" } else { "s" }` adds a hop without clarity
- **It creates mixed styles** — if only 4 of 8 match arms can use a pattern, keep all 8 consistent

### When simplicity beats cleverness

- Don't over-generalize signatures. Accept `&str` unless you genuinely need `impl AsRef<str>`.
- Don't add traits for single implementations. `CommandRunner` is justified because tests inject `MockCommandRunner`. A trait for "any YAML parser" when you only use `serde_yaml` is premature.
- Don't add deep trait hierarchies. Flat, focused traits (`DeployTarget`, `SyncRemote`, `CommandRunner`) are a strength.
- Don't use typestate builders for things constructed once with 3-4 fields. Plain structs are fine.
- The test: can a new contributor understand this in 30 seconds?

## Adapter self-sufficiency

Each target and remote file is a self-contained unit. It imports from its parent `mod.rs` (traits, `CommandRunner`, `check_command`, `resolve_env_flags`) but never from sibling adapters.

**Allowed shared infrastructure** (in `targets/mod.rs` or `remotes/mod.rs`):

- Trait definitions (`DeployTarget`, `SyncRemote`)
- `CommandRunner`, `CommandOutput`, `CommandOpts`
- `check_command`, `resolve_env_flags`, `validate_stdin_kv_value`
- Builder functions (`build_targets`, `build_remotes`)

**Not allowed:**

- Shared helpers only 2-3 adapters use — inline them
- Cross-adapter imports (`use super::fly::resolve_app`)
- Macros that generate adapter boilerplate — hides what's happening

The test: if removing one adapter file requires editing another adapter file, the dependency is wrong.

## Collections

Use `BTreeMap` over `HashMap` for deterministic ordering in serialized output (config, store, deploy index). This matters for diffs, tests, and debugging.

Use `Vec::with_capacity(n)` when the size is known. `collect()` into `Vec` preallocates when the iterator has a `size_hint()`, so it's usually free.

## Clippy

Enable pedantic lints selectively. In `lib.rs`:

```rust
#![warn(clippy::pedantic)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::struct_excessive_bools)]
```

Key pedantic lints to keep enabled:

- `redundant_closure_for_method_calls` — simplifies closures
- `manual_let_else` — encourages `let ... else` syntax
- `needless_pass_by_value` — catches unnecessary moves
- `cast_possible_truncation` — safer numeric casts

## Testing

- Inline `#[cfg(test)] mod tests` at the bottom of each file
- `tempfile::tempdir()` for filesystem isolation — no real external services
- `MockCommandRunner` with pre-canned `CommandOutput` for target/remote tests
- Assert on both the result AND the recorded CLI calls (program, args, stdin, cwd)
- Test function names: `{module}_{scenario}` — no `test_` prefix
- Never remove or weaken existing tests

## Naming

| Kind           | Convention                                        | Example                                             |
| -------------- | ------------------------------------------------- | --------------------------------------------------- |
| Functions      | `snake_case`, descriptive verbs                   | `resolve_app`, `check_command`, `deploy_secret`     |
| Structs        | `PascalCase`, domain noun                         | `DeployOptions`, `CloudflareTarget`, `StorePayload` |
| Enums          | `PascalCase` with `PascalCase` variants           | `DeployMode::Individual`, `Required::Envs(vec)`     |
| Constants      | `UPPER_SNAKE_CASE`                                | `ESK_VERSION_KEY`, `MAX_VERSION_JUMP`               |
| Modules/files  | `snake_case`, matching domain                     | `aws_ssm.rs`, `cloud_file.rs`                       |
| Config structs | `{Service}TargetConfig` / `{Service}RemoteConfig` | `FlyTargetConfig`, `DopplerRemoteConfig`            |
| Test functions | `{module}_{scenario}`                             | `fly_deploy_uses_stdin`                             |
| Getters        | No `get_` prefix                                  | `fn name(&self) -> &str`, not `fn get_name`         |
| Predicates     | `is_` or `has_` prefix, returns `bool`            | `is_empty()`, `has_key()`                           |
| Acronyms       | One word in PascalCase                            | `Uuid`, not `UUID`; `HttpClient`, not `HTTPClient`  |

No module stuttering: items within a module should not repeat the module name. `targets::EnvFile`, not `targets::EnvFileTarget` (unless `Target` is meaningful disambiguation).

## Import ordering

1. External crates (`anyhow`, `console`, `serde`)
2. Standard library (`std::collections`, `std::path`)
3. Blank line
4. Crate-internal (`crate::config`, `crate::store`)

Alphabetical within each group. Multi-item imports grouped with `{}`. Never glob-import except `use super::*` in test modules.
