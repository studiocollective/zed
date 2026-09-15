mod agent;
mod connection;
mod git;
mod protocol;
mod prefs;
mod terminal;
mod workspace;

use anyhow::{Context as _, Result};
use clap::Parser;
use client::{Client, ProxySettings, UserStore};
use connection::Connection;
use extension::ExtensionHostProxy;
use fs::RealFs;
use gpui::http_client::read_proxy_from_env;
use gpui::{App, AppContext as _, Entity, UpdateGlobal as _};
use gpui_tokio::Tokio;
use language::LanguageRegistry;
use node_runtime::{NodeBinaryOptions, NodeRuntime};
use project::{LocalProjectFlags, Project};
use release_channel::AppVersion;
use reqwest_client::ReqwestClient;
use settings::{Settings as _, SettingsStore};
use smol::net::TcpListener;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use util::ResultExt as _;
use workspace::HeadlessWorkspace;

/// Serve a Zed project headlessly over a JSON WebSocket protocol.
#[derive(Parser)]
struct Cli {
    /// Directory to open as the workspace root.
    #[arg(default_value = ".")]
    root: PathBuf,
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:9400")]
    listen: SocketAddr,
    /// Used by the project's shell-environment loader, which re-executes this binary.
    #[arg(long, hide = true)]
    printenv: bool,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();
    if cli.printenv {
        util::shell_env::print_env();
        return Ok(());
    }
    let root = cli
        .root
        .canonicalize()
        .context("resolving workspace root")?;
    let listen = cli.listen;

    let http_client = Arc::new(ReqwestClient::new());
    let app = gpui_platform::headless().with_http_client(http_client);

    app.run(move |cx| {
        let project = init_project(cx);
        cx.spawn(async move |cx| {
            let result: Result<()> = async {
                let project = project?;
                let workspace = HeadlessWorkspace::open(project, &root, cx).await?;
                let listener = TcpListener::bind(listen)
                    .await
                    .with_context(|| format!("binding {listen}"))?;
                log::info!(
                    "ide_server listening on ws://{listen} for {}",
                    root.display()
                );
                loop {
                    let (stream, peer) = listener.accept().await?;
                    log::info!("client connected from {peer}");
                    let workspace = workspace.clone();
                    cx.spawn(async move |cx| {
                        if let Err(error) = Connection::serve(stream, workspace, cx).await {
                            log::warn!("connection from {peer} ended with error: {error:#}");
                        }
                    })
                    .detach();
                }
            }
            .await;
            if let Err(error) = result {
                log::error!("{error:#}");
                cx.update(|cx| cx.quit());
            }
            anyhow::Ok::<()>(())
        })
        .detach();
    });

    Ok(())
}

/// External ACP agents are configured under `agent_servers` in the user's Zed
/// settings, so read that file once so the same agents show up here.
fn load_user_settings(cx: &mut App) {
    let path = paths::settings_file();
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            log::warn!("reading {}: {error}", path.display());
            return;
        }
    };
    SettingsStore::update_global(cx, |store, cx| {
        let result = store.set_user_settings(&content, cx);
        if let settings::ParseStatus::Failed { error } = &result.parse_status {
            log::warn!("parsing {}: {error}", path.display());
        }
    });
}

fn init_project(cx: &mut App) -> Result<Entity<Project>> {
    let app_version = AppVersion::load(env!("CARGO_PKG_VERSION"), None, None);
    release_channel::init(app_version, cx);
    gpui_tokio::init(cx);
    settings::init(cx);
    load_user_settings(cx);
    feature_flags::FeatureFlagStore::init(cx);
    language_model::init(cx);

    let proxy_url = ProxySettings::get_global(cx)
        .proxy
        .as_ref()
        .and_then(|input| input.parse().ok())
        .or_else(read_proxy_from_env);
    let user_agent = format!(
        "ide_server/{} ({}; {})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    let http = {
        let _guard = Tokio::handle(cx).enter();
        ReqwestClient::proxy_and_user_agent(proxy_url, &user_agent)
            .context("starting HTTP client")?
    };
    cx.set_http_client(Arc::new(http));

    let client = Client::production(cx);
    cx.set_http_client(client.http_client());

    let fs = Arc::new(RealFs::new(None, cx.background_executor().clone()));
    let languages = Arc::new(LanguageRegistry::new(cx.background_executor().clone()));
    let user_store = cx.new(|cx| UserStore::new(client.clone(), cx));

    extension::init(cx);
    let _extension_host_proxy = ExtensionHostProxy::global(cx);
    project::AgentRegistryStore::init_global(cx, fs.clone(), client.http_client());

    // Registry agents (Claude Agent, Codex, Gemini CLI) run through npx, so
    // fetch Node the way Zed does when it isn't on PATH — first-run agent
    // setup has to work on a machine that has never had Node or Zed.
    let (mut node_options_tx, node_options_rx) = watch::channel(None);
    node_options_tx
        .send(Some(NodeBinaryOptions {
            allow_path_lookup: true,
            allow_binary_download: true,
            use_paths: None,
        }))
        .log_err();
    let node_runtime = NodeRuntime::new(client.http_client(), None, node_options_rx);

    Project::init(&client, cx);
    Ok(Project::local(
        client,
        node_runtime,
        user_store,
        languages,
        fs,
        None,
        LocalProjectFlags {
            init_worktree_trust: false,
            watch_global_configs: false,
        },
        cx,
    ))
}
