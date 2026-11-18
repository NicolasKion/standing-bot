//! State shared by the Discord bot, the web server and the sync task.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serenity::model::id::UserId;
use tokio::sync::Notify;
use tracing::warn;

use crate::access::Report;
use crate::config::Config;
use crate::esi::Esi;
use crate::sso::{ACCESS_LIST_SCOPE, RefreshError, Sso};
use crate::store::{Character, Home, Store};

/// How many access lists of one character are looked at.
const MAX_LISTS_PER_CHARACTER: usize = 25;

/// How long a login link from `/standings auth` stays usable.
const LOGIN_TTL: Duration = Duration::from_secs(15 * 60);

/// What a login asks the player to grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginScopes {
    Contacts,
    ContactsAndAccessLists,
}

struct PendingLogin {
    user: UserId,
    scopes: LoginScopes,
    last_used: Instant,
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

    /// Asks ESI for the access lists the user's characters are manager or
    /// admin of, as id and name, and remembers the names for autocomplete.
    pub async fn managed_lists(&self, user: UserId) -> anyhow::Result<BTreeMap<i64, String>> {
        // Keyed by id so a list two characters manage shows up once.
        let mut lists = BTreeMap::new();
        let characters = self.store.characters_of(user).await?;
        for reader in characters.iter().filter(|c| c.reads_access_lists) {
            let id = reader.character_id;
            let token = match self.access_token(reader).await {
                Ok(token) => token,
                Err(error) => {
                    warn!("no token for {}: {error}", reader.name);
                    continue;
                }
            };
            let ids = match self.esi.access_list_ids(id, &token).await {
                Ok(ids) => ids,
                Err(error) => {
                    warn!("could not list access lists of {}: {error}", reader.name);
                    continue;
                }
            };
            for list_id in ids.into_iter().take(MAX_LISTS_PER_CHARACTER) {
                if lists.contains_key(&list_id) {
                    continue;
                }
                match self.esi.access_list(id, &token, list_id).await {
                    Ok(list) => {
                        lists.insert(list.id, list.name);
                    }
                    Err(error) => warn!("could not read access list {list_id}: {error}"),
                }
            }
        }
        self.list_names
            .lock()
            .expect("list names lock poisoned")
            .insert(user, lists.values().cloned().collect());
        Ok(lists)
    }

    /// Access list names from the user's last lookup.
    pub fn list_names(&self, user: UserId) -> Vec<String> {
        self.list_names
            .lock()
            .expect("list names lock poisoned")
            .get(&user)
            .cloned()
            .unwrap_or_default()
    }

    /// Opens a login session for a Discord user and returns the OAuth `state`
    /// for it. The session can link any number of characters.
    pub fn begin_login(&self, user: UserId, scopes: LoginScopes) -> String {
        let state = format!("{:032x}", rand::random::<u128>());
        let mut logins = self.logins.lock().expect("login map lock poisoned");
        logins.retain(|_, login| login.last_used.elapsed() < LOGIN_TTL);
        logins.insert(
            state.clone(),
            PendingLogin {
                user,
                scopes,
                last_used: Instant::now(),
            },
        );
        state
    }

    /// Who a login session belongs to and which scopes it asks for, if the
    /// state is known and fresh. Using a session keeps it alive.
    pub fn login(&self, state: &str) -> Option<(UserId, LoginScopes)> {
        let mut logins = self.logins.lock().expect("login map lock poisoned");
        let login = logins.get_mut(state)?;
        if login.last_used.elapsed() >= LOGIN_TTL {
            logins.remove(state);
            return None;
        }
        login.last_used = Instant::now();
        Some((login.user, login.scopes))
    }

    /// The EVE login URL of a session.
    pub fn login_url(&self, state: &str, scopes: LoginScopes) -> anyhow::Result<reqwest::Url> {
        let extra: &[&str] = match scopes {
            LoginScopes::Contacts => &[],
            LoginScopes::ContactsAndAccessLists => &[ACCESS_LIST_SCOPE],
        };
        self.sso.authorize_url(state, extra)
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
