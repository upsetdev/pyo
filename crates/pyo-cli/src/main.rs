//! `pyo`, the command line for Pyo: link a folder to a service, then
//! deploy, follow logs, manage its environment and roll back from a
//! terminal or CI. Settings live in pyo.toml (see config.rs).

mod api;
mod backup;
mod compose;
mod config;
mod db;
mod domain;
mod dotenv;
mod exec;
mod login;
mod registry;
mod service;
mod ui;
mod volume;

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "pyo",
    version,
    about = "Deploy and manage your Pyo services.",
    propagate_version = true
)]
struct Cli {
    /// Your Pyo, e.g. https://pyo.example.com (default: the one you signed in to).
    #[arg(long, global = true, env = "PYO_URL", hide_env_values = true)]
    url: Option<String>,
    /// An API key, for CI (default: the one `pyo login` saved).
    #[arg(long, global = true, env = "PYO_TOKEN", hide_env_values = true)]
    token: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct Which {
    /// The service: a name ("api", "shop/api") or id. Default: pyo.toml's.
    // Global: taken before or after a subcommand (`pyo env -s api set`,
    // `pyo env set -s api`).
    #[arg(long, short, global = true)]
    service: Option<String>,
}

/// `pyo volume attach`'s service, taken before or after the subcommand
/// like the others' `-s`.
#[derive(Args)]
struct AttachTo {
    /// The service to attach a volume to (pyo volume attach): "api", "shop/api" or an id.
    #[arg(long, short, global = true)]
    service: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Sign in through the browser, or with --token for CI.
    Login {
        /// When the new API key expires.
        #[arg(long, default_value = "1y", value_parser = ["30d", "90d", "1y", "never"])]
        expires: String,
    },
    /// Forget this computer's sign-in.
    Logout,
    /// Who you're signed in as.
    Whoami,
    /// Whether observability (analytics, request logs, usage history) is on
    /// for this Pyo, or turn it on or off. Turning it is for the
    /// installation's admin, with a full-access key.
    Observability {
        #[arg(value_parser = ["on", "off"])]
        state: Option<String>,
    },
    /// Link this folder to a service (writes pyo.toml).
    Link {
        /// "api", "shop/api" or "team/shop/api". Without it, pick from a list.
        service: Option<String>,
    },
    /// Apply pyo.toml and deploy, then follow it to the end. With -f, import
    /// a Docker Compose file instead: see what it makes, then deploy it.
    Deploy {
        #[command(flatten)]
        which: Which,
        /// Start it and exit, without following it.
        #[arg(long)]
        detach: bool,
        /// A Docker Compose file to import as a new project (a .env next to
        /// it is read too).
        #[arg(long, short = 'f')]
        file: Option<std::path::PathBuf>,
        /// The team to import into (slug or id). Defaults to your first team.
        #[arg(long)]
        team: Option<String>,
        /// The cluster to run it on (name or id). Defaults to the team's default cluster.
        #[arg(long)]
        cluster: Option<String>,
        /// Import without asking.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// The service's state, live version and domains.
    Status {
        #[command(flatten)]
        which: Which,
        /// The whole service, as JSON.
        #[arg(long)]
        json: bool,
    },
    /// The service's recent logs.
    Logs {
        #[command(flatten)]
        which: Which,
        /// Keep printing new lines.
        #[arg(long, short)]
        follow: bool,
    },
    /// Environment variables.
    Env {
        #[command(subcommand)]
        command: Option<EnvCommand>,
        #[command(flatten)]
        which: Which,
    },
    /// Put back the previous version (or a deployment by id).
    Rollback {
        /// A deployment id. Defaults to the previous version.
        deployment: Option<String>,
        #[command(flatten)]
        which: Which,
    },
    /// Restart the service, one replica at a time.
    Restart {
        #[command(flatten)]
        which: Which,
    },
    /// A shell in the service.
    Exec {
        #[command(flatten)]
        which: Which,
        /// A replica name. Defaults to any ready replica.
        #[arg(long)]
        replica: Option<String>,
        /// bash, sh, zsh or ash. Defaults to whichever the service has.
        #[arg(long)]
        shell: Option<String>,
    },
    /// Open the service on the dashboard.
    Open {
        #[command(flatten)]
        which: Which,
    },
    /// Pyo registry, for the team's own images.
    Registry {
        #[command(subcommand)]
        command: RegistryCommand,
    },
    /// Built-in databases.
    Db {
        #[command(subcommand)]
        command: DbCommand,
    },
    /// Volumes: their backups, restoring and growing them, attaching them.
    Volume {
        #[command(subcommand)]
        command: VolumeCommand,
        #[command(flatten)]
        to: AttachTo,
    },
    /// The service's domains.
    Domain {
        #[command(subcommand)]
        command: DomainCommand,
        #[command(flatten)]
        which: Which,
    },
    /// Backups of volumes and databases.
    Backup {
        #[command(subcommand)]
        command: BackupCommand,
        #[command(flatten)]
        which: Which,
    },
}

#[derive(Subcommand)]
enum DomainCommand {
    /// The service's domains and their status.
    List,
    /// Serve the service on a hostname. It applies without a redeploy.
    Add {
        /// e.g. app.example.com.
        #[arg(required_unless_present = "free", conflicts_with = "free")]
        hostname: Option<String>,
        /// A free nip.io domain instead (no DNS record needed).
        #[arg(long)]
        free: bool,
        /// The HTTP port to route to, by name or number (default: the first).
        #[arg(long)]
        port: Option<String>,
        /// Wait until it's live.
        #[arg(long)]
        wait: bool,
    },
    /// Stop serving the service on a hostname.
    Remove {
        /// e.g. app.example.com.
        hostname: String,
        /// Don't ask to confirm.
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum BackupCommand {
    /// Back up a volume or database now.
    ///
    /// Without --volume or --db: the service's volume, when it has one.
    Now {
        /// The volume: an id, "name" or "project/name".
        #[arg(long, conflicts_with = "db")]
        volume: Option<String>,
        /// The database: its name or id.
        #[arg(long)]
        db: Option<String>,
        /// Wait until it's done.
        #[arg(long)]
        wait: bool,
    },
}

#[derive(Subcommand)]
enum VolumeCommand {
    /// Your teams' volumes.
    List,
    /// A volume's backups, newest first.
    Snapshots {
        /// The volume: an id, "name" or "project/name".
        volume: String,
    },
    /// Replace a volume's data with one of its backups. Its service is
    /// stopped while it runs and started again after.
    Restore {
        volume: String,
        /// A snapshot id (see `pyo volume snapshots`). Without it, pick from a list.
        snapshot: Option<String>,
        /// Don't ask to confirm.
        #[arg(long, short = 'y')]
        yes: bool,
        /// Start it and exit, without following it.
        #[arg(long)]
        detach: bool,
    },
    /// Delete one of a volume's backups for good. Owners and admins.
    DeleteBackup {
        volume: String,
        /// A snapshot id (see `pyo volume snapshots`). Without it, pick from a list.
        snapshot: Option<String>,
        /// Don't ask to confirm.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Grow a volume (it never shrinks), where its storage class can.
    Resize {
        volume: String,
        /// The new size in GB.
        size: i64,
    },
    /// Attach a detached volume to a service on its cluster. For a service in
    /// another project, the volume moves there first.
    Attach {
        volume: String,
        /// The folder in the service (default: where it was, or /data).
        #[arg(long)]
        path: Option<String>,
        /// Start a move and exit, without following it.
        #[arg(long)]
        detach: bool,
    },
}

#[derive(Subcommand)]
enum DbCommand {
    /// Copy a Postgres or MySQL database, or a dump file, into an empty
    /// database, and follow it. Owners and admins.
    Import {
        /// The database to import into: its name or id.
        database: String,
        /// The database to copy, as a URL: postgres://user:password@host:5432/db
        /// or mysql://… (it must be reachable from the cluster).
        /// PYO_IMPORT_URL keeps the password out of your shell's history.
        #[arg(
            long,
            value_name = "URL",
            env = "PYO_IMPORT_URL",
            hide_env_values = true,
            conflicts_with = "file"
        )]
        from: Option<String>,
        /// A dump instead: .sql, .sql.gz, or a Postgres .dump (pg_dump -Fc).
        #[arg(long, short = 'f')]
        file: Option<std::path::PathBuf>,
        /// Run the last failed import again, from the same source.
        #[arg(long, conflicts_with_all = ["from", "file"])]
        retry: bool,
        /// The team (slug or id), when several have a database by that name.
        #[arg(long)]
        team: Option<String>,
        /// Start it and exit, without following it.
        #[arg(long)]
        detach: bool,
    },
}

#[derive(Subcommand)]
enum RegistryCommand {
    /// Log Docker in with a new 30-day token, to push <team>/<project>/<service>
    /// images. Only works from a device on Pyo's tailnet.
    Login {
        /// The team (slug or id). Defaults to your first team.
        #[arg(long)]
        team: Option<String>,
        /// A token that can only pull.
        #[arg(long)]
        pull_only: bool,
    },
}

#[derive(Subcommand)]
enum EnvCommand {
    /// List variables (secrets stay hidden).
    List,
    /// Set KEY=value pairs.
    Set {
        #[arg(required = true, value_name = "KEY=VALUE")]
        pairs: Vec<String>,
        /// Store them as secrets: encrypted, never shown again.
        #[arg(long)]
        secret: bool,
        /// Deploy right away.
        #[arg(long)]
        deploy: bool,
    },
    /// Remove variables.
    Unset {
        #[arg(required = true, value_name = "KEY")]
        keys: Vec<String>,
        /// Deploy right away.
        #[arg(long)]
        deploy: bool,
    },
    /// Set the variables of a .env file.
    ///
    /// New keys are variables unless marked secret. A secret that's
    /// overwritten stays a secret. In a terminal, you're asked about keys
    /// that are already set. Elsewhere, pass --overwrite or --skip-existing.
    Push {
        #[arg(default_value = ".env")]
        file: String,
        /// Overwrite keys that are set already.
        #[arg(long, conflicts_with = "skip_existing")]
        overwrite: bool,
        /// Keep keys that are set already.
        #[arg(long)]
        skip_existing: bool,
        /// Make this key a secret (repeatable).
        #[arg(long, value_name = "KEY")]
        secret: Vec<String>,
        /// Make every key a secret.
        #[arg(long)]
        secrets: bool,
        /// Show what would change, without saving.
        #[arg(long)]
        dry_run: bool,
        /// Deploy right away.
        #[arg(long)]
        deploy: bool,
    },
    /// Write the variables to a .env file.
    Pull {
        #[arg(default_value = ".env")]
        file: String,
        /// Overwrite the file.
        #[arg(long)]
        force: bool,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprintln!("{} {e:#}", ui::red("error:"));
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let (url, token) = (cli.url.as_deref(), cli.token.as_deref());
    match cli.command {
        Command::Login { expires } => {
            login::run(cli.url.clone(), cli.token.clone(), &expires).await
        }
        Command::Logout => {
            config::Credentials::clear()?;
            ui::ok("Signed out. Revoke the key under API keys if you won't use it again.");
            Ok(())
        }
        Command::Whoami => {
            let api = config::client(url, token)?;
            let me = api.me().await?;
            println!(
                "{} {}",
                me["email"].as_str().unwrap_or(""),
                ui::dim(&format!("on {}", api.base))
            );
            Ok(())
        }
        Command::Observability { state } => {
            let api = config::client(url, token)?;
            if let Some(state) = state {
                api.op(
                    "setObservability",
                    serde_json::json!({ "enabled": state == "on" }),
                )
                .await?;
            }
            let v = api.get("/api/v1/observability/settings").await?;
            let on = v["enabled"].as_bool().unwrap_or(true);
            println!("Observability is {}.", if on { "on" } else { "off" });
            if v["small"].as_bool() == Some(true) && on {
                println!(
                    "{}",
                    ui::dim("This server has under 3.5 GB of memory: turning it off saves memory.")
                );
            } else if v["small"].as_bool() == Some(false) && !on {
                println!("{}", ui::dim("This server has enough memory for it now."));
            }
            Ok(())
        }
        Command::Link { service } => service::link(&config::client(url, token)?, service).await,
        Command::Deploy {
            file: Some(f),
            team,
            cluster,
            yes,
            ..
        } => {
            compose::deploy(
                &config::client(url, token)?,
                &f,
                team.as_deref(),
                cluster.as_deref(),
                yes,
            )
            .await
        }
        Command::Deploy { which, detach, .. } => {
            service::deploy(
                &config::client(url, token)?,
                which.service.as_deref(),
                detach,
            )
            .await
        }
        Command::Status { which, json } => {
            service::status(&config::client(url, token)?, which.service.as_deref(), json).await
        }
        Command::Logs { which, follow } => {
            service::logs(
                &config::client(url, token)?,
                which.service.as_deref(),
                follow,
            )
            .await
        }
        Command::Env { command, which } => {
            let api = config::client(url, token)?;
            let s = which.service.as_deref();
            match command.unwrap_or(EnvCommand::List) {
                EnvCommand::List => service::env_list(&api, s).await,
                EnvCommand::Set {
                    pairs,
                    secret,
                    deploy,
                } => service::env_set(&api, s, &pairs, secret, deploy).await,
                EnvCommand::Unset { keys, deploy } => {
                    service::env_unset(&api, s, &keys, deploy).await
                }
                EnvCommand::Push {
                    file,
                    overwrite,
                    skip_existing,
                    secret,
                    secrets,
                    dry_run,
                    deploy,
                } => {
                    let existing = if overwrite {
                        service::Existing::Overwrite
                    } else if skip_existing {
                        service::Existing::Skip
                    } else {
                        service::Existing::Ask
                    };
                    service::env_push(
                        &api,
                        s,
                        &file,
                        service::Push {
                            existing,
                            secret,
                            secrets,
                            dry_run,
                            deploy,
                        },
                    )
                    .await
                }
                EnvCommand::Pull { file, force } => service::env_pull(&api, s, &file, force).await,
            }
        }
        Command::Rollback { deployment, which } => {
            service::rollback(
                &config::client(url, token)?,
                which.service.as_deref(),
                deployment,
            )
            .await
        }
        Command::Restart { which } => {
            service::restart(&config::client(url, token)?, which.service.as_deref()).await
        }
        Command::Exec {
            which,
            replica,
            shell,
        } => {
            exec::run(
                &config::client(url, token)?,
                which.service.as_deref(),
                replica,
                shell,
            )
            .await
        }
        Command::Open { which } => {
            service::open(&config::client(url, token)?, which.service.as_deref()).await
        }
        Command::Registry {
            command: RegistryCommand::Login { team, pull_only },
        } => registry::login(&config::client(url, token)?, team.as_deref(), pull_only).await,
        Command::Db {
            command:
                DbCommand::Import {
                    database,
                    from,
                    file,
                    retry,
                    team,
                    detach,
                },
        } => {
            db::import(
                &config::client(url, token)?,
                team.as_deref(),
                &database,
                from,
                file,
                retry,
                detach,
            )
            .await
        }
        Command::Volume { command, to } => {
            let api = config::client(url, token)?;
            if to.service.is_some() && !matches!(command, VolumeCommand::Attach { .. }) {
                anyhow::bail!("-s/--service is for pyo volume attach.");
            }
            match command {
                VolumeCommand::List => volume::list(&api).await,
                VolumeCommand::Snapshots { volume } => volume::snapshots(&api, &volume).await,
                VolumeCommand::Restore {
                    volume,
                    snapshot,
                    yes,
                    detach,
                } => volume::restore(&api, &volume, snapshot, yes, detach).await,
                VolumeCommand::DeleteBackup {
                    volume,
                    snapshot,
                    yes,
                } => volume::delete_backup(&api, &volume, snapshot, yes).await,
                VolumeCommand::Resize { volume, size } => volume::resize(&api, &volume, size).await,
                VolumeCommand::Attach {
                    volume,
                    path,
                    detach,
                } => {
                    let service = to.service.ok_or_else(|| {
                        anyhow::anyhow!(
                            "Name the service: pyo volume attach {volume} -s <service>."
                        )
                    })?;
                    volume::attach(&api, &volume, &service, path, detach).await
                }
            }
        }
        Command::Domain { command, which } => {
            let api = config::client(url, token)?;
            let s = which.service.as_deref();
            match command {
                DomainCommand::List => domain::list(&api, s).await,
                DomainCommand::Add {
                    hostname,
                    free,
                    port,
                    wait,
                } => domain::add(&api, s, hostname, free, port, wait).await,
                DomainCommand::Remove { hostname, yes } => {
                    domain::remove(&api, s, &hostname, yes).await
                }
            }
        }
        Command::Backup {
            command: BackupCommand::Now { volume, db, wait },
            which,
        } => {
            backup::now(
                &config::client(url, token)?,
                which.service.as_deref(),
                volume,
                db,
                wait,
            )
            .await
        }
    }
}
