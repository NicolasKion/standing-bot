//! Discord bot that makes the EVE characters of a channel's members set each
//! other to a positive standing.

mod access;
mod app;
mod config;
mod discord;
mod esi;
mod sso;
mod status;
mod store;
mod sync;
mod web;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serenity::Client;
use serenity::model::gateway::GatewayIntents;
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};
use tracing::info;

use crate::app::App;
use crate::config::Config;
use crate::esi::Esi;
use crate::sso::Sso;
use crate::store::Store;
use crate::sync::Syncer;

#[tokio::main]
async fn main() -> Result<()> {
    // A missing .env file is fine: in Docker the variables come from the host.
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt::init();

    let config = Config::from_env()?;
    let http = reqwest::Client::builder()
        .user_agent(format!(
            "standing-bot/{} ({})",
            env!("CARGO_PKG_VERSION"),
            config.contact
        ))
        .timeout(Duration::from_secs(30))
        .build()
        .context("building the HTTP client")?;

    let store = Store::open(&config.database_path)
        .await
        .with_context(|| format!("opening the database {}", config.database_path.display()))?;
    let sso = Sso::connect(http.clone(), &config).await?;
    let home = store.home().await?;
    if home.is_none() {
        info!("not set up yet: a server admin has to run /standings-admin setup in the channel");
    }
    let app = Arc::new(App::new(config, store, sso, Esi::new(http), home));

    let listener = TcpListener::bind(app.config.bind_addr)
        .await
        .with_context(|| format!("binding to {}", app.config.bind_addr))?;
    info!("web server listening on {}", app.config.bind_addr);
    let router = web::router(Arc::clone(&app));

    let intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_MEMBERS;
    let mut client = Client::builder(&app.config.discord_token, intents)
        .event_handler(discord::Handler::new(Arc::clone(&app)))
        .await
        .context("creating the Discord client")?;

    let syncer = Syncer::new(Arc::clone(&app), Arc::clone(&client.http));

    let mut terminate = signal(SignalKind::terminate()).context("listening for SIGTERM")?;
    tokio::select! {
        result = client.start() => result.context("Discord connection ended")?,
        result = axum::serve(listener, router) => result.context("web server ended")?,
        () = syncer.run() => {}
        _ = tokio::signal::ctrl_c() => info!("shutting down"),
        _ = terminate.recv() => info!("shutting down"),
    }
    Ok(())
}
