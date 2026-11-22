//! The small web server EVE SSO sends players back to after logging in.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Html;
use axum::routing::get;
use serde::Deserialize;
use tracing::{info, warn};

use crate::app::App;
use crate::sso::CORPORATION_SCOPE;
use crate::store::CorpAssets;

#[derive(Deserialize)]
struct Callback {
    code: String,
    state: String,
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(|| async { "asset-bot is running" }))
        .route("/callback", get(callback))
        .with_state(app)
}

async fn callback(
    State(app): State<Arc<App>>,
    Query(params): Query<Callback>,
) -> (StatusCode, Html<String>) {
    match link_character(&app, &params).await {
        Ok(linked) => (
            StatusCode::OK,
            page(
                &format!(
                    "{} is linked. I am reading its assets now, you can go back to Discord.",
                    linked.name
                ),
                Some(("Add another character", linked.next_login.as_str())),
            ),
        ),
        Err(error) => {
            warn!("login failed: {error:#}");
            (
                StatusCode::BAD_REQUEST,
                page(
                    &format!(
                        "That did not work: {error}. Run /assets auth in Discord to try again."
                    ),
                    None,
                ),
            )
        }
    }
}

struct Linked {
    name: String,
    /// EVE login URL for adding the next character in the same session.
    next_login: String,
}

/// Links the character that just logged in to the session's Discord user.
async fn link_character(app: &Arc<App>, params: &Callback) -> Result<Linked> {
    let (user, scopes) = app
        .login(&params.state)
        .context("this login link has expired")?;

    let tokens = app.sso.exchange(&params.code).await?;
    let identity = app.sso.verify(&tokens.access_token).await?;
    if !identity.has_required_scopes() {
        bail!("the login did not grant the asset scopes");
    }

    let corp_assets = if identity.has_scope(CORPORATION_SCOPE) {
        CorpAssets::Readable
    } else {
        CorpAssets::Unreadable
    };
    app.store
        .upsert_character(
            identity.character_id,
            &identity.name,
            user,
            &tokens.refresh_token,
            corp_assets,
        )
        .await?;
    info!("linked {} to Discord user {user}", identity.name);

    // Read the assets right away, so locations and item names can be
    // suggested by the time the player is back in Discord.
    let background = Arc::clone(app);
    tokio::spawn(async move {
        match background.store.characters_of(user).await {
            Ok(mut characters) => background.update_corporations(&mut characters).await,
            Err(error) => warn!("could not load characters after login: {error:#}"),
        }
        if let Err(error) = background.refresh_user(user).await {
            warn!("could not read assets after login: {error:#}");
        }
    });

    Ok(Linked {
        name: identity.name,
        next_login: app.login_url(&params.state, scopes)?.into(),
    })
}

/// A plain page with a message and optionally one link as (label, url).
fn page(message: &str, link: Option<(&str, &str)>) -> Html<String> {
    let message = escape(message);
    let link = link.map_or_else(String::new, |(label, url)| {
        format!("<p><a href=\"{}\">{}</a></p>", escape(url), escape(label))
    });
    Html(format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Asset Bot</title>\
         <style>body{{font-family:system-ui,sans-serif;background:#111;color:#eee;\
         display:grid;place-content:center;min-height:100vh;margin:0}}\
         p{{max-width:32rem;padding:0 1rem;text-align:center;line-height:1.5}}\
         a{{display:inline-block;padding:.6rem 1.2rem;border-radius:.4rem;\
         background:#2f6fed;color:#fff;text-decoration:none}}</style>\
         </head><body><p>{message}</p>{link}</body></html>"
    ))
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
