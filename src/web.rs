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
use crate::sso::ACCESS_LIST_SCOPE;
use crate::store::AccessLists;

#[derive(Deserialize)]
struct Callback {
    code: String,
    state: String,
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(|| async { "standing-bot is running" }))
        .route("/callback", get(callback))
        .with_state(app)
}

async fn callback(
    State(app): State<Arc<App>>,
    Query(params): Query<Callback>,
) -> (StatusCode, Html<String>) {
    match link_character(&app, &params).await {
        Ok(name) => (
            StatusCode::OK,
            page(&format!(
                "{name} is linked. Standings are set within a minute. You can close this tab."
            )),
        ),
        Err(error) => {
            warn!("login failed: {error:#}");
            (
                StatusCode::BAD_REQUEST,
                page(&format!(
                    "That did not work: {error}. Run /standings auth in Discord to try again."
                )),
            )
        }
    }
}

/// Finishes a login and returns the name of the linked character.
async fn link_character(app: &App, params: &Callback) -> Result<String> {
    let user = app
        .finish_login(&params.state)
        .context("this login link is expired or was already used")?;

    let tokens = app.sso.exchange(&params.code).await?;
    let identity = app.sso.verify(&tokens.access_token).await?;
    if !identity.has_required_scopes() {
        bail!("the login did not grant the contact scopes");
    }

    let access_lists = if identity.has_scope(ACCESS_LIST_SCOPE) {
        AccessLists::Readable
    } else {
        AccessLists::Unreadable
    };
    app.store
        .upsert_character(
            identity.character_id,
            &identity.name,
            user,
            &tokens.refresh_token,
            access_lists,
        )
        .await?;
    app.sync.notify_one();
    info!("linked {} to Discord user {user}", identity.name);
    Ok(identity.name)
}

fn page(message: &str) -> Html<String> {
    let message = message
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    Html(format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>Standing Bot</title>\
         <style>body{{font-family:system-ui,sans-serif;background:#111;color:#eee;\
         display:grid;place-items:center;min-height:100vh;margin:0}}\
         p{{max-width:32rem;padding:1rem;text-align:center;line-height:1.5}}</style>\
         </head><body><p>{message}</p></body></html>"
    ))
}
