//! Discord bot that compares EVE Online assets with the stock players want
//! at a location and writes the shopping list for what is missing.

mod app;
mod assets;
mod config;
mod discord;
mod esi;
mod monitor;
mod parse;
mod shopping;
mod sso;
mod store;
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
use crate::monitor::Monitor;
use crate::sso::Sso;
use crate::store::Store;

#[tokio::main]
async fn main() -> Result<()> {
    // A missing .env file is fine: in Docker the variables come from the host.
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt::init();

    let config = Config::from_env()?;
    let http = reqwest::Client::builder()
        .user_agent(format!(
            "asset-bot/{} ({})",
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
    let app = Arc::new(App::new(config, store, sso, Esi::new(http)));

    let listener = TcpListener::bind(app.config.bind_addr)
        .await
        .with_context(|| format!("binding to {}", app.config.bind_addr))?;
    info!("web server listening on {}", app.config.bind_addr);
    let router = web::router(Arc::clone(&app));

    // Slash commands need no privileged intents.
    let mut client = Client::builder(&app.config.discord_token, GatewayIntents::empty())
        .event_handler(discord::Handler::new(Arc::clone(&app)))
        .await
        .context("creating the Discord client")?;

    let monitor = Monitor::new(Arc::clone(&app), Arc::clone(&client.http));

    let mut terminate = signal(SignalKind::terminate()).context("listening for SIGTERM")?;
    tokio::select! {
        result = client.start() => result.context("Discord connection ended")?,
        result = axum::serve(listener, router) => result.context("web server ended")?,
        () = monitor.run() => {}
        _ = tokio::signal::ctrl_c() => info!("shutting down"),
        _ = terminate.recv() => info!("shutting down"),
    }
    Ok(())
}
