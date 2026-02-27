Release a new version of esk. Argument: `$ARGUMENTS` (must be `major`, `minor`, or `patch`).

## Steps

1. **Validate argument**: Ensure `$ARGUMENTS` is exactly one of `major`, `minor`, or `patch`. If missing or invalid, stop and tell the user.

2. **Ensure clean state**: Run `git status --porcelain`. If there are uncommitted changes, stop and tell the user to commit or stash first.

3. **Ensure on main branch**: Run `git branch --show-current`. If not `main`, stop and tell the user.

4. **Pull latest**: Run `git pull --rebase origin main`.

5. **Read current version**: Read `Cargo.toml` and extract the `version = "X.Y.Z"` line from the `[package]` section.

6. **Compute new version**: Parse the current version as semver and bump the requested part:
   - `patch`: X.Y.Z → X.Y.(Z+1)
   - `minor`: X.Y.Z → X.(Y+1).0
   - `major`: X.Y.Z → (X+1).0.0

7. **Run preflight checks** (all must pass before any changes are made):
   - `cargo fmt --check`
   - `cargo clippy -- -D warnings`
   - `cargo test`

   If any check fails, stop and report the failure. Do not modify any files.

8. **Bump version**: Edit the `version = "..."` line in `Cargo.toml` to the new version.

9. **Commit**: Stage `Cargo.toml` and `Cargo.lock` (if changed), then commit with message `chore: bump version to {new_version}`.

10. **Tag**: Create an annotated tag: `git tag -a v{new_version} -m "release v{new_version}"`.

11. **Push**: Run `git push origin main && git push origin v{new_version}`.

12. **Report**: Print the old version, new version, and tag. Mention that GitHub Actions should now run the Release workflow.
