#![allow(dead_code)]

use anyhow::Result;
use esk::config::Config;
use esk::store::SecretStore;
use esk::targets::{CommandOpts, CommandOutput, CommandRunner};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tempfile::TempDir;

/// A temporary esk project for testing.
pub struct TestProject {
    pub dir: TempDir,
}

impl TestProject {
    /// Create a test project with a custom esk.yaml content.
    pub fn new(yaml: &str) -> Result<Self> {
        let dir = TempDir::new()?;
        std::fs::write(dir.path().join("esk.yaml"), yaml)?;
        Ok(Self { dir })
    }

    /// Create a test project with store initialized (key + enc file).
    pub fn with_store(yaml: &str) -> Result<Self> {
        let project = Self::new(yaml)?;
        SecretStore::load_or_create(project.root())?;
        Ok(project)
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    pub fn config(&self) -> Result<Config> {
        let path = self.dir.path().join("esk.yaml");
        Config::load(&path)
    }

    pub fn store(&self) -> Result<SecretStore> {
        SecretStore::open(self.root())
    }

    pub fn deploy_index_path(&self) -> PathBuf {
        self.dir.path().join(".esk/deploy-index.json")
    }

    pub fn sync_index_path(&self) -> PathBuf {
        self.dir.path().join(".esk/sync-index.json")
    }
}

/// Minimal valid config: project + 1 env, no apps/targets/secrets.
pub const MINIMAL_CONFIG: &str = r#"
project: testapp
environments: [dev]
"#;

/// Full config with all target types, apps, vendors, and remotes.
pub const FULL_CONFIG: &str = r#"
project: myapp
environments: [dev, prod]

apps:
  web:
    path: apps/web
  api:
    path: apps/api

targets:
  env:
    pattern: "{app_path}/.env{env_suffix}.local"
    env_suffix:
      dev: ""
      prod: ".production"
  cloudflare:
    env_flags:
      prod: "--env production"
  convex:
    path: apps/api
    deployment_source: apps/api/.env.local
    env_flags:
      prod: "--prod"

remotes:
  1password:
    vault: Engineering
    item_pattern: "{project} - {Environment}"

secrets:
  Stripe:
    STRIPE_KEY:
      description: Stripe API key
      targets:
        env: [web:dev, web:prod]
        cloudflare: [web:prod]
    STRIPE_WEBHOOK:
      targets:
        env: [web:dev, web:prod]
  Convex:
    CONVEX_URL:
      targets:
        env: [web:dev, web:prod]
        convex: [dev, prod]
  General:
    API_SECRET:
      targets:
        env: [api:dev, api:prod]
"#;

/// Config with only env target.
pub const ENV_ONLY_CONFIG: &str = r#"
project: testapp
environments: [dev, prod]

apps:
  web:
    path: apps/web

targets:
  env:
    pattern: "{app_path}/.env{env_suffix}.local"
    env_suffix:
      dev: ""
      prod: ".production"

secrets:
  General:
    MY_SECRET:
      targets:
        env: [web:dev, web:prod]
    OTHER_SECRET:
      targets:
        env: [web:dev]
"#;

/// Cloudflare target config for integration testing.
pub const CLOUDFLARE_CONFIG: &str = r#"
project: testapp
environments: [dev, prod]

apps:
  web:
    path: apps/web

targets:
  cloudflare:
    env_flags:
      prod: "--env production"

secrets:
  Stripe:
    STRIPE_KEY:
      targets:
        cloudflare: [web:dev, web:prod]
    STRIPE_WEBHOOK:
      targets:
        cloudflare: [web:dev]
"#;

/// Convex target config for integration testing.
pub const CONVEX_CONFIG: &str = r#"
project: testapp
environments: [dev, prod]

targets:
  convex:
    path: apps/api
    deployment_source: apps/api/.env.local
    env_flags:
      prod: "--prod"

secrets:
  Convex:
    CONVEX_URL:
      targets:
        convex: [dev, prod]
"#;

/// OnePassword remote config for integration testing.
pub const ONEPASSWORD_REMOTE_CONFIG: &str = r#"
project: testapp
environments: [dev, prod]

remotes:
  1password:
    vault: Engineering
    item_pattern: "{project} - {Environment}"

secrets:
  Stripe:
    STRIPE_KEY:
      targets: {}
"#;

/// Config with cloud_file remotes for testing.
pub const REMOTE_CONFIG: &str = r#"
project: testapp
environments: [dev, prod]

remotes:
  1password:
    vault: Test
    item_pattern: "{project} - {Environment}"
"#;

/// Fly target config for integration testing.
pub const FLY_CONFIG: &str = r#"
project: testapp
environments: [dev, prod]

apps:
  web:
    path: apps/web

targets:
  fly:
    app_names:
      web: my-fly-app
    env_flags:
      prod: "--stage"

secrets:
  General:
    API_KEY:
      targets:
        fly: [web:dev, web:prod]
"#;

/// Netlify target config for integration testing.
pub const NETLIFY_CONFIG: &str = r#"
project: testapp
environments: [dev, prod]

targets:
  netlify:
    site: my-site-id
    env_flags:
      prod: "--context production"

secrets:
  General:
    API_KEY:
      targets:
        netlify: [dev, prod]
"#;

/// Vercel target config for integration testing.
pub const VERCEL_CONFIG: &str = r#"
project: testapp
environments: [dev, prod]

targets:
  vercel:
    env_names:
      dev: development
      prod: production
    env_flags:
      prod: "--scope my-team"

secrets:
  General:
    API_KEY:
      targets:
        vercel: [dev, prod]
"#;

/// GitHub target config for integration testing.
pub const GITHUB_CONFIG: &str = r#"
project: testapp
environments: [dev, prod]

targets:
  github:
    repo: owner/repo
    env_flags:
      prod: "--env production"

secrets:
  General:
    API_KEY:
      targets:
        github: [dev, prod]
"#;

/// Heroku target config for integration testing.
pub const HEROKU_CONFIG: &str = r#"
project: testapp
environments: [dev, prod]

apps:
  web:
    path: apps/web

targets:
  heroku:
    app_names:
      web: my-heroku-app
    env_flags:
      prod: "--remote staging"

secrets:
  General:
    API_KEY:
      targets:
        heroku: [web:dev, web:prod]
"#;

/// Supabase target config for integration testing.
pub const SUPABASE_CONFIG: &str = r#"
project: testapp
environments: [dev, prod]

targets:
  supabase:
    project_ref: abcdef123456
    env_flags:
      prod: "--experimental"

secrets:
  General:
    API_KEY:
      targets:
        supabase: [dev, prod]
"#;

/// Railway target config for integration testing.
pub const RAILWAY_CONFIG: &str = r#"
project: testapp
environments: [dev, prod]

targets:
  railway:
    env_flags:
      prod: "--environment production"

secrets:
  General:
    API_KEY:
      targets:
        railway: [dev, prod]
"#;

/// AWS SSM target config for integration testing.
pub const AWS_SSM_CONFIG: &str = r#"
project: testapp
environments: [dev, prod]

targets:
  aws_ssm:
    path_prefix: "/{project}/{environment}/"
    region: us-east-1
    env_flags:
      prod: "--no-paginate"

secrets:
  General:
    API_KEY:
      targets:
        aws_ssm: [dev, prod]
"#;

/// Kubernetes target config for integration testing.
pub const KUBERNETES_CONFIG: &str = r#"
project: testapp
environments: [dev, prod]

targets:
  kubernetes:
    namespace:
      dev: testapp-dev
      prod: testapp-prod
    context:
      prod: prod-cluster
    env_flags: {}

secrets:
  General:
    API_KEY:
      targets:
        kubernetes: [dev, prod]
"#;

/// Docker Swarm target config for integration testing.
pub const DOCKER_CONFIG: &str = r#"
project: testapp
environments: [dev, prod]

targets:
  docker:
    name_pattern: "{project}-{environment}-{key}"
    labels:
      managed-by: esk
    env_flags:
      prod: "--context prod-swarm"

secrets:
  General:
    API_KEY:
      targets:
        docker: [dev, prod]
"#;

/// GitLab target config for integration testing.
pub const GITLAB_CONFIG: &str = r#"
project: testapp
environments: [dev, prod]

targets:
  gitlab:
    env_flags:
      prod: "--masked"

secrets:
  General:
    API_KEY:
      targets:
        gitlab: [dev, prod]
"#;

/// Config with validation constraints for testing.
pub const VALIDATION_CONFIG: &str = r#"
project: testapp
environments: [dev, prod]
apps:
  web:
    path: apps/web
targets:
  env:
    pattern: "{app_path}/.env{env_suffix}.local"
    env_suffix: { dev: "", prod: ".production" }
secrets:
  General:
    DATABASE_URL:
      validate:
        format: url
      targets:
        env: [web:dev, web:prod]
    PORT:
      validate:
        format: integer
        range: [1, 65535]
      targets:
        env: [web:dev, web:prod]
    NODE_ENV:
      validate:
        enum: [development, staging, production]
      targets:
        env: [web:dev, web:prod]
    ENABLE_CACHE:
      validate:
        format: boolean
      targets:
        env: [web:dev, web:prod]
"#;

/// Config with required-variable auditing for testing.
pub const REQUIRED_CONFIG: &str = r#"
project: testapp
environments: [dev, prod]
apps:
  web:
    path: apps/web
targets:
  env:
    pattern: "{app_path}/.env{env_suffix}.local"
    env_suffix: { dev: "", prod: ".production" }
secrets:
  General:
    DB_URL:
      targets:
        env: [web:dev, web:prod]
    ANALYTICS:
      required: false
      targets:
        env: [web:dev, web:prod]
    SENTRY_DSN:
      required: [prod]
      targets:
        env: [web:dev, web:prod]
"#;

/// Records calls made to a mock command runner.
#[derive(Debug, Clone)]
pub struct RecordedCall {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub stdin: Option<Vec<u8>>,
    pub env: Vec<(String, String)>,
}

/// A mock CommandRunner that records calls and returns configurable responses.
pub struct MockCommandRunner {
    pub calls: Mutex<Vec<RecordedCall>>,
    pub responses: Mutex<Vec<Result<CommandOutput, String>>>,
}

impl MockCommandRunner {
    pub fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            responses: Mutex::new(Vec::new()),
        }
    }

    pub fn with_success() -> Self {
        let runner = Self::new();
        runner.push_success(b"", b"");
        runner
    }

    pub fn push_success(&self, stdout: &[u8], stderr: &[u8]) {
        self.responses.lock().unwrap().push(Ok(CommandOutput {
            success: true,
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }));
    }

    pub fn push_failure(&self, stderr: &[u8]) {
        self.responses.lock().unwrap().push(Ok(CommandOutput {
            success: false,
            stdout: Vec::new(),
            stderr: stderr.to_vec(),
        }));
    }

    pub fn push_error(&self, msg: &str) {
        self.responses.lock().unwrap().push(Err(msg.to_string()));
    }

    pub fn take_calls(&self) -> Vec<RecordedCall> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }
}

impl CommandRunner for MockCommandRunner {
    fn run(&self, program: &str, args: &[&str], opts: CommandOpts) -> Result<CommandOutput> {
        self.calls.lock().unwrap().push(RecordedCall {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: opts.cwd.clone(),
            stdin: opts.stdin.clone(),
            env: opts.env.clone(),
        });

        let mut responses = self.responses.lock().unwrap();
        if responses.is_empty() {
            Ok(CommandOutput {
                success: true,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        } else {
            let response = responses.remove(0);
            match response {
                Ok(output) => Ok(output),
                Err(msg) => Err(anyhow::anyhow!(msg)),
            }
        }
    }
}
