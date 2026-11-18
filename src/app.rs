//! State shared by the Discord bot, the web server and the sync task.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serenity::model::id::UserId;
use tokio::sync::Notify;

use crate::access::Report;
use crate::config::Config;
use crate::esi::Esi;
use crate::sso::{RefreshError, Sso};
use crate::store::{Character, Home, Store};

/// How long a login link from `/standings auth` stays usable.
const LOGIN_TTL: Duration = Duration::from_secs(15 * 60);

struct PendingLogin {
    user: UserId,
    created: Instant,
}

pub struct App {
    pub config: Config,
    pub store: Store,
    pub sso: Sso,
    pub esi: Esi,
    /// Wakes the sync task after something changed.
    pub sync: Notify,
    logins: Mutex<HashMap<String, PendingLogin>>,
    access_report: Mutex<Option<Report>>,
    home: Mutex<Option<Home>>,
    /// Access list names each user saw last, for autocomplete.
    list_names: Mutex<HashMap<UserId, Vec<String>>>,
}

impl App {
    pub fn new(config: Config, store: Store, sso: Sso, esi: Esi, home: Option<Home>) -> Self {
        Self {
            config,
            store,
            sso,
            esi,
            sync: Notify::new(),
            logins: Mutex::new(HashMap::new()),
            access_report: Mutex::new(None),
            home: Mutex::new(home),
            list_names: Mutex::new(HashMap::new()),
        }
    }

    /// The server and channel the bot works in, once `/standings-admin setup` has run.
    pub fn home(&self) -> Option<Home> {
        *self.home.lock().expect("home lock poisoned")
    }

    pub fn set_home(&self, home: Home) {
        *self.home.lock().expect("home lock poisoned") = Some(home);
    }

    /// Access list names the user's characters manage, as of their last
    /// `/standings access-list` run.
    pub fn list_names(&self, user: UserId) -> Vec<String> {
        self.list_names
            .lock()
            .expect("list names lock poisoned")
            .get(&user)
            .cloned()
            .unwrap_or_default()
    }

    pub fn set_list_names(&self, user: UserId, names: Vec<String>) {
        self.list_names
            .lock()
            .expect("list names lock poisoned")
            .insert(user, names);
    }

    /// Starts a login for a Discord user and returns the OAuth `state` for it.
    pub fn begin_login(&self, user: UserId) -> String {
        let state = format!("{:032x}", rand::random::<u128>());
        let mut logins = self.logins.lock().expect("login map lock poisoned");
        logins.retain(|_, login| login.created.elapsed() < LOGIN_TTL);
        logins.insert(
            state.clone(),
            PendingLogin {
                user,
                created: Instant::now(),
            },
        );
        state
    }

    /// Ends a login and returns who started it, if the state is known and fresh.
    pub fn finish_login(&self, state: &str) -> Option<UserId> {
        let login = self
            .logins
            .lock()
            .expect("login map lock poisoned")
            .remove(state)?;
        (login.created.elapsed() < LOGIN_TTL).then_some(login.user)
    }

    /// Gets an ESI access token for a character and keeps a rotated refresh token.
    pub async fn access_token(&self, character: &Character) -> Result<String, RefreshError> {
        let tokens = self.sso.refresh(&character.refresh_token).await?;
        if tokens.refresh_token != character.refresh_token {
            self.store
                .set_refresh_token(character.character_id, &tokens.refresh_token)
                .await?;
        }
        Ok(tokens.access_token)
    }

    /// The latest access list check, if a list is tracked and was checked.
    pub fn access_report(&self) -> Option<Report> {
        self.access_report
            .lock()
            .expect("access report lock poisoned")
            .clone()
    }

    pub fn set_access_report(&self, report: Option<Report>) {
        *self
            .access_report
            .lock()
            .expect("access report lock poisoned") = report;
    }
}
