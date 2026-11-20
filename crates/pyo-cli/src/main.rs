//! `pyo`, the command line for Pyo: link a folder to a service, then
//! deploy, follow logs, manage its environment and roll back from a
//! terminal or CI. Settings live in pyo.jsonc (see config.rs).

mod api;
mod backup;
mod cloudflare;
mod compose;
mod config;
mod db;
mod domain;
mod dotenv;
mod exec;
mod firewall;
mod import;
mod login;
mod pack;
mod registry;
mod sandbox;
mod server;
mod service;
mod ui;
mod upgrade;
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
    // PYO_TOKEN, the old name, still works when PYO_API_KEY isn't set.
    #[arg(
        long,
        alias = "api-key",
        global = true,
        env = "PYO_API_KEY",
        hide_env_values = true
    )]
    token: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct Which {
    /// The service: a name ("api", "shop/api") or id. Default: pyo.jsonc's.
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
    /// Link this folder to a service (writes pyo.jsonc).
    Link {
        /// "api", "shop/api" or "team/shop/api". Without it, pick from a list.
        service: Option<String>,
    },
    /// Apply pyo.jsonc and deploy, then follow it to the end.
    ///
    /// A service built from a folder gets this folder, packed and uploaded
    /// (.gitignore and .pyoignore apply). In a folder no service is linked
    /// to, it makes one and links it. With -f, import a Docker Compose file
    /// instead: see what it makes, then deploy it.
    Deploy {
        #[command(flatten)]
        which: Which,
        /// Start it and exit, without following it.
        #[arg(long)]
        detach: bool,
        /// Send this folder, also for a service built from Git.
        #[arg(long, conflicts_with = "file")]
        local: bool,
        /// Send .env files too (they're left out by default).
        #[arg(long, conflicts_with = "file")]
        include_env: bool,
        /// A new service's name. Defaults to the folder's.
        #[arg(long, conflicts_with = "file")]
        name: Option<String>,
        /// A new service's project (name or id), made when there's none by that name.
        #[arg(long, conflicts_with = "file")]
        project: Option<String>,
        /// A Docker Compose file to import as a new project (a .env next to
        /// it is read too).
        #[arg(long, short = 'f')]
        file: Option<std::path::PathBuf>,
        /// The team of a new service or Compose import (slug or id). Defaults to your first team.
        #[arg(long)]
        team: Option<String>,
        /// The cluster to run it on (name or id). Defaults to the team's default cluster.
        #[arg(long)]
        cluster: Option<String>,
        /// Don't ask: import the Compose file, or go ahead with a very
        /// large replica change from pyo.jsonc.
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
    /// The service's logs, from every replica.
    Logs {
        #[command(flatten)]
        which: Which,
        /// Keep printing new lines.
        #[arg(long, short)]
        follow: bool,
        /// Only lines since then: 30s, 10m, 2h or 1d.
        #[arg(long)]
        since: Option<String>,
        /// One replica, by its name as the lines show it.
        #[arg(long)]
        replica: Option<String>,
        /// The latest deployment's build and deploy log instead.
        #[arg(long, conflicts_with_all = ["since", "replica"])]
        build: bool,
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
    /// Where the service runs: any server, or only the servers and tags
    /// given. It moves over right away, without a redeploy. Without
    /// options, shows where it runs now.
    Place {
        #[command(flatten)]
        which: Which,
        /// A server to run on, by name (repeatable).
        #[arg(long = "server")]
        servers: Vec<String>,
        /// Run on every server with this tag, tagged now or later (repeatable).
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// Run on any server.
        #[arg(long, conflicts_with_all = ["servers", "tags"])]
        any: bool,
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
    /// Sandboxes: isolated machines on your sandbox servers. exec, shell and
    /// cp reach the sandbox's server directly, over your tailnet for now.
    Sandbox {
        #[command(subcommand)]
        command: sandbox::Command,
        /// The team (slug or id). Defaults to your first team.
        #[arg(long, global = true)]
        team: Option<String>,
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
    /// The team's Cloudflare account: Pyo creates DNS records for domains in
    /// its zones. Owners and admins connect it.
    Cloudflare {
        #[command(subcommand)]
        command: CloudflareCommand,
        /// The team (slug or id). Defaults to your first team.
        #[arg(long, global = true)]
        team: Option<String>,
    },
    /// Who may reach the service: its firewall rules, and a CDN in front.
    Firewall {
        #[command(subcommand)]
        command: FirewallCommand,
        #[command(flatten)]
        which: Which,
    },
    /// Bring services over from Coolify, Dokploy, Railway or Vercel: see
    /// what they become, then import them. Owners and admins.
    Import {
        /// The platform: coolify, dokploy, railway or vercel.
        #[arg(value_parser = ["coolify", "dokploy", "railway", "vercel"])]
        platform: String,
        /// Its address, e.g. https://coolify.example.com. Not for Railway or Vercel.
        #[arg(long, required_if_eq_any = [("platform", "coolify"), ("platform", "dokploy")])]
        from: Option<String>,
        /// Its API token (Coolify's with read and read:sensitive, a Railway account, workspace or project token, a Vercel token). Used for this import only.
        #[arg(long, env = "PYO_IMPORT_TOKEN", hide_env_values = true)]
        source_token: String,
        /// Vercel only: the team to read (its slug or id). The token's own scope by default.
        #[arg(long)]
        source_team: Option<String>,
        /// The team to import into (slug or id). Defaults to your first team.
        #[arg(long)]
        team: Option<String>,
        /// The cluster to run it on (name or id). Defaults to the team's default cluster.
        #[arg(long)]
        cluster: Option<String>,
        /// Deploy the services once they're made.
        #[arg(long)]
        deploy: bool,
        /// Import without asking.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Backups of volumes and databases.
    Backup {
        #[command(subcommand)]
        command: BackupCommand,
        #[command(flatten)]
        which: Which,
    },
    /// Hetzner Cloud: the team's API token, for making servers there.
    Hetzner {
        #[command(subcommand)]
        command: HetznerCommand,
        /// The team (slug or id). Defaults to your first team.
        #[arg(long, global = true)]
        team: Option<String>,
    },
    /// Servers Pyo makes at Hetzner and joins to a cluster over your tailnet.
    Server {
        #[command(subcommand)]
        command: ServerCommand,
    },
    /// The CLI's version, like --version.
    #[command(disable_version_flag = true)]
    Version,
    /// Update the CLI to the latest release.
    ///
    /// Once a day, the CLI looks for a new release and says when one is
    /// out. PYO_NO_UPDATE_CHECK=1 turns that off.
    #[command(disable_version_flag = true)]
    Upgrade {
        /// A release instead of the latest, e.g. 0.3.0.
        #[arg(long)]
        version: Option<String>,
    },
}

#[derive(Subcommand)]
enum HetznerCommand {
    /// Save a project API token with Read & Write. Owners and admins.
    Connect {
        /// The token. Without it, it's read from PYO_HETZNER_TOKEN, a pipe, or asked for.
        #[arg(long, env = "PYO_HETZNER_TOKEN", hide_env_values = true)]
        hetzner_token: Option<String>,
    },
    /// List the locations the saved token sees.
    Test,
    /// Forget the token. Servers Pyo made there keep running.
    Disconnect {
        /// Don't ask to confirm.
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum ServerCommand {
    /// Server types and their monthly prices at Hetzner.
    Types {
        /// One location, e.g. fsn1.
        #[arg(long)]
        location: Option<String>,
        /// The team (slug or id). Defaults to your first team.
        #[arg(long)]
        team: Option<String>,
    },
    /// Make a server at Hetzner and join it to a cluster. Shows the price and asks first.
    Create {
        /// The cluster it joins (name or id). Defaults to the team's default cluster.
        #[arg(long, conflicts_with = "new_cluster")]
        cluster: Option<String>,
        /// Make a new cluster with this name instead. The server is its first.
        #[arg(long)]
        new_cluster: Option<String>,
        /// Its name. Defaults to <cluster>-<n>.
        #[arg(long, conflicts_with = "new_cluster")]
        name: Option<String>,
        /// A Hetzner location, e.g. fsn1 (see pyo server types).
        #[arg(long)]
        location: String,
        /// A server type, e.g. cx22 (see pyo server types).
        #[arg(long = "type")]
        server_type: String,
        /// A build-only server.
        #[arg(long, conflicts_with = "new_cluster")]
        builder: bool,
        /// Your SSH public key file, to sign in over your tailnet.
        #[arg(long, value_name = "FILE")]
        ssh_key: Option<std::path::PathBuf>,
        /// A tag for placing services (repeatable).
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// The team (slug or id). Defaults to your first team.
        #[arg(long)]
        team: Option<String>,
        /// Don't ask to confirm the price.
        #[arg(long, short = 'y')]
        yes: bool,
        /// Start it and exit, without following it.
        #[arg(long)]
        detach: bool,
    },
    /// The servers Pyo made at Hetzner, and how they're doing.
    List,
    /// Move a server's services off it, take it out of its cluster and your tailnet, and delete it at Hetzner.
    Delete {
        /// Its name or id.
        name: String,
        /// Its cluster, when names repeat.
        #[arg(long)]
        cluster: Option<String>,
        /// Don't ask to confirm.
        #[arg(long, short = 'y')]
        yes: bool,
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
        /// Don't create the DNS record in Cloudflare, even when a connected zone covers it.
        #[arg(long)]
        no_cloudflare: bool,
        /// Proxied by Cloudflare (once its certificate is issued).
        #[arg(long, conflicts_with = "no_cloudflare")]
        proxied: bool,
    },
    /// Stop serving the service on a hostname. Deletes the DNS record Pyo made for it in Cloudflare.
    Remove {
        /// e.g. app.example.com.
        hostname: String,
        /// Don't ask to confirm.
        #[arg(long, short = 'y')]
        yes: bool,
        /// Keep the DNS record Pyo made in Cloudflare.
        #[arg(long)]
        keep_dns_record: bool,
    },
    /// Create a domain's DNS record in Cloudflare, or try again. Never overwrites a record.
    Dns {
        /// e.g. app.example.com.
        hostname: String,
        /// Proxied by Cloudflare (once its certificate is issued).
        #[arg(long)]
        proxied: bool,
        /// Replace the records in the way. Owners and admins.
        #[arg(long)]
        replace: bool,
    },
}

#[derive(Subcommand)]
enum CloudflareCommand {
    /// Connect Cloudflare, or replace its token. The token needs Zone:Read and DNS:Edit.
    Connect {
        /// A file with the token, or - for stdin. Or set CLOUDFLARE_API_TOKEN.
        #[arg(long)]
        token_file: Option<String>,
        /// The token, from the environment.
        #[arg(
            long = "from-env",
            env = "CLOUDFLARE_API_TOKEN",
            hide_env_values = true,
            hide = true
        )]
        env_token: Option<String>,
    },
    /// Check the token: the zones it covers and what it's missing.
    Test,
    /// The zones Pyo can create records in.
    Zones,
    /// Forget the token. Records Pyo made stay in Cloudflare.
    Remove {
        /// Don't ask to confirm.
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum FirewallCommand {
    /// The rules, in the order they're checked.
    List,
    /// Add a rule at the end.
    ///
    /// Values: IP addresses and ranges, two-letter country codes, or
    /// presets: cloudflare, fastly, bunny, cloudfront (a CDN's current
    /// ranges), tailnet, private. None: any client.
    Add {
        /// allow, block or rate-limit.
        action: String,
        /// The values to match.
        values: Vec<String>,
        /// Match clients not in the values instead.
        #[arg(long)]
        not: bool,
        /// A rate limit's requests from each client address.
        #[arg(long)]
        requests: Option<u32>,
        /// A rate limit's window: second or minute.
        #[arg(long, value_parser = ["second", "minute"])]
        per: Option<String>,
        /// A name for it.
        #[arg(long)]
        name: Option<String>,
    },
    /// Remove a rule, by its number in the list or its id.
    Remove {
        rule: String,
        /// Don't ask to confirm.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Put the service's websites behind a CDN, or "off".
    ///
    /// Behind Cloudflare, only Cloudflare reaches them, and rules, rate
    /// limits and analytics see the visitor it forwards.
    Behind {
        #[arg(value_parser = ["cloudflare", "off"])]
        cdn: String,
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
    upgrade::remove_old();
    let check = match cli.command {
        Command::Upgrade { .. } => None,
        _ => upgrade::Check::start(),
    };
    let result = run(cli).await;
    if let Err(e) = &result {
        eprintln!("{} {e:#}", ui::red("error:"));
    }
    if let Some(check) = check {
        check.finish();
    }
    if result.is_err() {
        std::process::exit(1);
    }
}

/// `--token` or PYO_API_KEY, else PYO_TOKEN (the old name). Blank is unset.
fn api_key(token: Option<String>, old: Option<String>) -> Option<String> {
    let set = |k: &String| !k.trim().is_empty();
    token.filter(set).or(old.filter(set))
}

async fn run(mut cli: Cli) -> anyhow::Result<()> {
    cli.token = api_key(cli.token.take(), std::env::var("PYO_TOKEN").ok());
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
        Command::Import {
            platform,
            from,
            source_token,
            source_team,
            team,
            cluster,
            deploy,
            yes,
        } => {
            import::run(
                &config::client(url, token)?,
                import::Args {
                    platform: &platform,
                    from: from.as_deref().unwrap_or(""),
                    token: &source_token,
                    source_team: source_team.as_deref(),
                    team: team.as_deref(),
                    cluster: cluster.as_deref(),
                    deploy,
                    yes,
                },
            )
            .await
        }
        Command::Deploy {
            which,
            detach,
            yes,
            local,
            include_env,
            name,
            project,
            team,
            cluster,
            ..
        } => {
            service::deploy(
                &config::client(url, token)?,
                which.service.as_deref(),
                detach,
                yes,
                service::Folder {
                    local,
                    with_env: include_env,
                    name,
                    project,
                    team,
                    cluster,
                },
            )
            .await
        }
        Command::Status { which, json } => {
            service::status(&config::client(url, token)?, which.service.as_deref(), json).await
        }
        Command::Logs {
            which,
            follow,
            since,
            replica,
            build,
        } => {
            service::logs(
                &config::client(url, token)?,
                which.service.as_deref(),
                service::LogsOpts {
                    follow,
                    since,
                    replica,
                    build,
                },
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
        Command::Place {
            which,
            servers,
            tags,
            any,
        } => {
            service::place(
                &config::client(url, token)?,
                which.service.as_deref(),
                servers,
                tags,
                any,
            )
            .await
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
        Command::Firewall { command, which } => {
            let api = config::client(url, token)?;
            let s = which.service.as_deref();
            match command {
                FirewallCommand::List => firewall::list(&api, s).await,
                FirewallCommand::Add {
                    action,
                    values,
                    not,
                    requests,
                    per,
                    name,
                } => {
                    firewall::add(
                        &api,
                        s,
                        firewall::NewRule {
                            action,
                            not,
                            sources: values,
                            requests,
                            per,
                            name,
                        },
                    )
                    .await
                }
                FirewallCommand::Remove { rule, yes } => {
                    firewall::remove(&api, s, &rule, yes).await
                }
                FirewallCommand::Behind { cdn } => firewall::behind(&api, s, &cdn).await,
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
                    no_cloudflare,
                    proxied,
                } => {
                    let dns = domain::Dns {
                        cloudflare: !no_cloudflare,
                        proxied,
                    };
                    domain::add(&api, s, hostname, free, port, wait, dns).await
                }
                DomainCommand::Remove {
                    hostname,
                    yes,
                    keep_dns_record,
                } => domain::remove(&api, s, &hostname, yes, keep_dns_record).await,
                DomainCommand::Dns {
                    hostname,
                    proxied,
                    replace,
                } => cloudflare::domain_dns(&api, s, &hostname, proxied, replace).await,
            }
        }
        Command::Cloudflare { command, team } => {
            let api = config::client(url, token)?;
            let team = team.as_deref();
            match command {
                CloudflareCommand::Connect {
                    token_file,
                    env_token,
                } => cloudflare::connect(&api, team, token_file.as_deref(), env_token).await,
                CloudflareCommand::Test => cloudflare::test(&api, team).await,
                CloudflareCommand::Zones => cloudflare::zones(&api, team).await,
                CloudflareCommand::Remove { yes } => cloudflare::remove(&api, team, yes).await,
            }
        }
        Command::Sandbox { command, team } => {
            sandbox::run(&config::client(url, token)?, team.as_deref(), command).await
        }
        Command::Version => {
            println!("pyo {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Upgrade { version } => upgrade::run(version).await,
        Command::Hetzner { command, team } => {
            let api = config::client(url, token)?;
            let team = team.as_deref();
            match command {
                HetznerCommand::Connect { hetzner_token } => {
                    server::connect(&api, team, hetzner_token).await
                }
                HetznerCommand::Test => server::test(&api, team).await,
                HetznerCommand::Disconnect { yes } => server::disconnect(&api, team, yes).await,
            }
        }
        Command::Server { command } => {
            let api = config::client(url, token)?;
            match command {
                ServerCommand::Types { location, team } => {
                    server::types(&api, team.as_deref(), location.as_deref()).await
                }
                ServerCommand::Create {
                    cluster,
                    new_cluster,
                    name,
                    location,
                    server_type,
                    builder,
                    ssh_key,
                    tags,
                    team,
                    yes,
                    detach,
                } => {
                    server::create(
                        &api,
                        server::New {
                            team: team.as_deref(),
                            cluster: cluster.as_deref(),
                            new_cluster: new_cluster.as_deref(),
                            name: name.as_deref(),
                            location: &location,
                            server_type: &server_type,
                            builder,
                            ssh_key: ssh_key.as_deref(),
                            tags,
                            yes,
                            detach,
                        },
                    )
                    .await
                }
                ServerCommand::List => server::list_servers(&api).await,
                ServerCommand::Delete { name, cluster, yes } => {
                    server::delete(&api, &name, cluster.as_deref(), yes).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn pyo_api_key_wins_over_pyo_token() {
        let s = |v: &str| Some(v.to_string());
        // --token or PYO_API_KEY first, PYO_TOKEN only as a fallback.
        assert_eq!(api_key(s("k"), s("t")), s("k"));
        assert_eq!(api_key(None, s("t")), s("t"));
        assert_eq!(api_key(s(" "), s("t")), s("t"));
        assert_eq!(api_key(None, s(" ")), None);
        assert_eq!(api_key(None, None), None);
    }

    #[test]
    fn the_key_is_documented_as_pyo_api_key() {
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("PYO_API_KEY"), "{help}");
        assert!(!help.contains("PYO_TOKEN"), "{help}");
        let cli = Cli::try_parse_from(["pyo", "--api-key", "k", "whoami"]).unwrap();
        assert_eq!(cli.token.as_deref(), Some("k"));
    }

    #[test]
    fn every_command_parses_and_has_help() {
        Cli::command().debug_assert();
        // The docs list every command from its help: each one has an about.
        fn walk(c: &clap::Command, path: &str) {
            for sub in c.get_subcommands().filter(|s| s.get_name() != "help") {
                let p = format!("{path} {}", sub.get_name());
                assert!(sub.get_about().is_some(), "{p} has no help line");
                walk(sub, &p);
            }
        }
        walk(&Cli::command(), "pyo");
    }

    #[test]
    fn sandbox_commands_map_to_the_api() {
        let parse = |args: &[&str]| Cli::try_parse_from(args).map(|c| c.command);
        let Ok(Command::Sandbox {
            command: sandbox::Command::Exec { id, command, .. },
            team,
        }) = parse(&[
            "pyo", "sandbox", "exec", "sbx_a", "--team", "acme", "--", "ls", "-la",
        ])
        else {
            panic!("exec didn't parse");
        };
        assert_eq!((id.as_str(), team.as_deref()), ("sbx_a", Some("acme")));
        assert_eq!(command, ["ls", "-la"]);
        assert!(matches!(
            parse(&[
                "pyo", "sandbox", "create", "node-22", "--ttl", "1h", "-e", "A=1"
            ]),
            Ok(Command::Sandbox {
                command: sandbox::Command::Create { .. },
                ..
            })
        ));
        assert!(matches!(
            parse(&["pyo", "sandbox", "servers", "rm", "sandbox-1", "-y"]),
            Ok(Command::Sandbox {
                command: sandbox::Command::Servers { .. },
                ..
            })
        ));
        assert!(parse(&["pyo", "sandbox", "create", "x", "--network", "wifi"]).is_err());
        assert!(parse(&["pyo", "sandbox", "exec", "sbx_a"]).is_err());
    }
}
