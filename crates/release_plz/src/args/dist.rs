use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clap::{Args, Subcommand};
use release_plz_core::{
    GitHub, RepoUrl,
    dist::{self, BuildOptions, Plan},
};
use secrecy::SecretString;

use super::{config_path::ConfigPath, manifest_command::ManifestCommand};

#[derive(Debug, Args)]
pub struct Dist {
    #[command(subcommand)]
    command: DistCommand,
}

#[derive(Debug, Subcommand)]
enum DistCommand {
    /// Print the complete distribution plan as JSON without building or publishing.
    Plan(PlanArgs),
    /// Build one target and stage its archives, checksums and build manifest.
    Build(Build),
    /// Verify targets, generate installers, upload assets, and publish a GitHub draft.
    Publish(Publish),
}

#[derive(Debug, Args)]
struct Common {
    /// Path to Cargo.toml.
    #[arg(long)]
    manifest_path: Option<PathBuf>,
    #[command(flatten)]
    config: ConfigPath,
    /// Workspace binary package to distribute. Optional if only one has binaries.
    #[arg(short, long)]
    package: Option<String>,
    /// Release tag. Build and publish require a clean checkout of this tag.
    #[arg(long)]
    tag: String,
}

impl ManifestCommand for Common {
    fn optional_manifest(&self) -> Option<&Path> {
        self.manifest_path.as_deref()
    }
}

#[derive(Debug, Args)]
struct Targets {
    /// Complete target list from CI. Repeat --target or separate targets with commas.
    #[arg(long = "target", required = true, value_delimiter = ',')]
    targets: Vec<String>,
}

#[derive(Debug, Args)]
struct PlanArgs {
    #[command(flatten)]
    common: Common,
    #[command(flatten)]
    targets: Targets,
}

#[derive(Debug, Args)]
struct Build {
    #[command(flatten)]
    common: Common,
    /// Rust target triple to build. Install its toolchain/linker first.
    #[arg(long)]
    target: String,
    /// Directory for archives and manifests (relative to the current directory).
    #[arg(long, default_value = "target/distrib")]
    output_dir: PathBuf,
    /// Cargo features to activate when building binaries.
    #[arg(long, value_delimiter = ',')]
    features: Vec<String>,
    /// Disable the package's default Cargo features.
    #[arg(long)]
    no_default_features: bool,
}

#[derive(Debug, Args)]
struct Publish {
    #[command(flatten)]
    common: Common,
    #[command(flatten)]
    targets: Targets,
    /// Directory containing the collected output of every target's dist build.
    #[arg(long, default_value = "target/distrib")]
    artifacts_dir: PathBuf,
    /// GitHub repository URL. Defaults to `workspace.repo_url` or the origin remote.
    #[arg(long)]
    repo_url: Option<String>,
    /// GitHub token with contents:write permission.
    #[arg(long, env = "GITHUB_TOKEN", hide_env_values = true)]
    git_token: Option<SecretString>,
    /// Validate artifacts and generate installers, manifest, checksums and notes locally.
    #[arg(long)]
    dry_run: bool,
}

impl Dist {
    pub async fn run(self) -> anyhow::Result<()> {
        let (common, targets) = match &self.command {
            DistCommand::Plan(args) => (&args.common, args.targets.targets.as_slice()),
            DistCommand::Build(args) => (&args.common, std::slice::from_ref(&args.target)),
            DistCommand::Publish(args) => (&args.common, args.targets.targets.as_slice()),
        };
        let config = common.config.load()?;
        // Distribution needs workspace packages, not a dependency resolution or
        // registry access. --locked also prevents metadata from changing the tag.
        let metadata = cargo_metadata::MetadataCommand::new()
            .manifest_path(common.manifest_path())
            .no_deps()
            .other_options(vec!["--locked".to_owned()])
            .exec()?;
        let plan = Plan::new(&metadata, common.package.as_deref(), targets, &common.tag)?;
        match self.command {
            DistCommand::Plan(_) => println!("{}", serde_json::to_string_pretty(&plan)?),
            DistCommand::Build(args) => {
                dist::build(
                    &plan,
                    &metadata,
                    &BuildOptions {
                        output_dir: args.output_dir,
                        features: args.features,
                        no_default_features: args.no_default_features,
                    },
                )?;
            }
            DistCommand::Publish(args) => {
                plan.verify_checkout(metadata.workspace_root.as_std_path())?;
                let repository = match args.repo_url.as_deref().or(config
                    .workspace
                    .repo_url
                    .as_ref()
                    .map(|url| url.as_str()))
                {
                    Some(url) => RepoUrl::new(url)?,
                    None => RepoUrl::from_repo(&git_cmd::Repo::new(&metadata.workspace_root)?)?,
                };
                let prepared = dist::prepare(&plan, &args.artifacts_dir, &repository)?;
                if args.dry_run {
                    println!("{}", serde_json::to_string_pretty(&prepared)?);
                } else {
                    let token = args
                        .git_token
                        .context("provide --git-token or GITHUB_TOKEN to publish")?;
                    let github = GitHub::from_repo_url(repository, token)?;
                    dist::publish(&github, &prepared).await?;
                }
            }
        }
        Ok(())
    }
}
