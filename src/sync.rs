//! Keeps the contacts of all linked characters in line with the network.
//!
//! The network is every linked character whose owner can see the configured
//! channel. Each of them gets every other one as a contact with the configured
//! standing. Characters that drop out lose those contacts again, and the others
//! lose them.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use serenity::http::Http;
use tracing::{debug, error, info, warn};

use crate::access::{AccessList, Outcome, Outstanding, Report, Verdict};
use crate::app::App;
use crate::discord;
use crate::esi::EsiError;
use crate::sso::RefreshError;
use crate::status::{self, Summary};
use crate::store::{Character, ManagedContact};

/// How often contacts are checked against ESI even if nothing changed, to
/// repair contacts a player removed or edited by hand.
const VERIFY_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// Wait after a change so a burst of events ends up in one sync.
const DEBOUNCE: Duration = Duration::from_secs(5);

/// ESI caches the contact list for 300 seconds, so a read this soon after a
/// write can miss that write.
const CONTACTS_CACHE: Duration = Duration::from_secs(310);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Only act on characters whose stored contacts differ from the network.
    Changes,
    /// Also compare every character's stored contacts with ESI.
    Verify,
}

#[derive(Debug, thiserror::Error)]
enum SyncError {
    /// The character's EVE login no longer works.
    #[error("EVE login is no longer valid")]
    LoginLost,
    #[error("ESI rate limit hit")]
    Throttled { retry_after: Option<Duration> },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<EsiError> for SyncError {
    fn from(error: EsiError) -> Self {
        match error {
            EsiError::Unauthorized => Self::LoginLost,
            EsiError::Throttled { retry_after } => Self::Throttled { retry_after },
            EsiError::Other(error) => Self::Other(error),
        }
    }
}

impl From<RefreshError> for SyncError {
    fn from(error: RefreshError) -> Self {
        match error {
            RefreshError::Revoked => Self::LoginLost,
            RefreshError::Other(error) => Self::Other(error),
        }
    }
}

pub struct Syncer {
    app: Arc<App>,
    discord: Arc<Http>,
    /// When the bot last changed a character's contacts.
    last_write: HashMap<i64, Instant>,
    /// Text of the status message as last published.
    published: Option<String>,
}

impl Syncer {
    pub fn new(app: Arc<App>, discord: Arc<Http>) -> Self {
        Self {
            app,
            discord,
            last_write: HashMap::new(),
            published: None,
        }
    }

    /// Runs forever: syncs on every wake-up and on a timer.
    pub async fn run(mut self) {
        let mut verify = tokio::time::interval(VERIFY_INTERVAL);
        loop {
            let mode = tokio::select! {
                () = self.app.sync.notified() => {
                    tokio::time::sleep(DEBOUNCE).await;
                    Mode::Changes
                }
                _ = verify.tick() => Mode::Verify,
            };
            if let Err(error) = self.sync(mode).await {
                error!("sync failed: {error:#}");
            }
        }
    }

    async fn sync(&mut self, mode: Mode) -> Result<()> {
        let Some(home) = self.app.home() else {
            return Ok(());
        };
        let members =
            discord::channel_members(&self.discord, home.guild_id, home.channel_id).await?;
        let characters = self.app.store.characters().await?;

        let network: HashSet<i64> = characters
            .iter()
            .filter(|c| !c.unlink_requested && members.contains(&c.discord_user_id))
            .map(|c| c.character_id)
            .collect();

        for character in &characters {
            let id = character.character_id;
            let mut desired = HashSet::new();
            if network.contains(&id) {
                desired.clone_from(&network);
                desired.remove(&id);
            }

            match self.sync_character(character, &desired, mode).await {
                Ok(()) => {
                    if character.unlink_requested {
                        self.app.store.delete_character(id).await?;
                        info!("unlinked {}", character.name);
                    }
                }
                Err(SyncError::LoginLost) => {
                    warn!(
                        "EVE login of {} no longer works, removing it",
                        character.name
                    );
                    self.app.store.delete_character(id).await?;
                    let still_here = members.contains(&character.discord_user_id);
                    if still_here && !character.unlink_requested {
                        self.alert_login_lost(character).await;
                    }
                    // The others still have this character as a contact.
                    self.app.sync.notify_one();
                }
                Err(SyncError::Throttled { retry_after }) => {
                    warn!("ESI rate limit hit, stopping this sync (retry after {retry_after:?})");
                    return Ok(());
                }
                Err(SyncError::Other(error)) => {
                    warn!("could not sync {}: {error:#}", character.name);
                }
            }
        }

        let in_network: Vec<&Character> = characters
            .iter()
            .filter(|c| network.contains(&c.character_id))
            .collect();
        self.check_access(&in_network).await?;

        let owners: HashSet<_> = in_network.iter().map(|c| c.discord_user_id).collect();
        let summary = Summary {
            members: owners.len(),
            characters: in_network.len(),
        };
        let text = status::render(summary, self.app.access_report().as_ref());
        // The timed pass always writes, which also brings back a deleted message.
        if mode == Mode::Verify || self.published.as_deref() != Some(text.as_str()) {
            status::publish(&self.app, &self.discord, &text).await?;
            self.published = Some(text);
        }
        Ok(())
    }

    /// Checks the network against the tracked access list, stores the result
    /// for `/standings access` and the status message.
    async fn check_access(&mut self, network: &[&Character]) -> Result<()> {
        let Some(tracked) = self.app.store.tracked_list().await? else {
            self.app.set_access_report(None);
            return Ok(());
        };

        let Some(list) = self.read_access_list(tracked.id, network).await else {
            self.app.set_access_report(Some(Report {
                list_name: tracked.name,
                checked_at: SystemTime::now(),
                outcome: Outcome::Unreadable,
            }));
            return Ok(());
        };

        let ids: Vec<i64> = network.iter().map(|c| c.character_id).collect();
        let affiliations = self.app.esi.affiliations(&ids).await?;
        let mut outstanding = Vec::new();
        for affiliation in &affiliations {
            let verdict = list.membership.verdict(affiliation);
            let character = network
                .iter()
                .find(|c| c.character_id == affiliation.character_id);
            if let (Some(character), false) = (character, verdict == Verdict::Allowed) {
                outstanding.push(Outstanding {
                    name: character.name.clone(),
                    owner: character.discord_user_id,
                    blocked: verdict == Verdict::Blocked,
                });
            }
        }
        outstanding.sort_by(|a, b| a.name.cmp(&b.name));

        let network_ids: HashSet<i64> = ids.iter().copied().collect();
        let extra_ids = list.membership.extra_characters(&network_ids);
        let names = match self.app.esi.names(&extra_ids).await {
            Ok(names) => names,
            Err(error) => {
                warn!("could not look up character names: {error}");
                HashMap::new()
            }
        };
        let mut extra: Vec<String> = extra_ids
            .iter()
            .map(|id| {
                names
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| format!("character {id}"))
            })
            .collect();
        extra.sort();

        self.app.set_access_report(Some(Report {
            list_name: list.name,
            checked_at: SystemTime::now(),
            outcome: Outcome::Checked {
                missing: outstanding,
                extra,
            },
        }));
        Ok(())
    }

    /// Reads the list through the first character that manages it.
    async fn read_access_list(&self, list_id: i64, network: &[&Character]) -> Option<AccessList> {
        for reader in network.iter().filter(|c| c.reads_access_lists) {
            let id = reader.character_id;
            let token = match self.app.access_token(reader).await {
                Ok(token) => token,
                Err(error) => {
                    debug!("no token for {}: {error}", reader.name);
                    continue;
                }
            };
            match self.app.esi.access_list(id, &token, list_id).await {
                Ok(list) => return Some(list),
                Err(error) => debug!("{} cannot read the access list: {error}", reader.name),
            }
        }
        None
    }

    async fn sync_character(
        &mut self,
        character: &Character,
        desired: &HashSet<i64>,
        mode: Mode,
    ) -> Result<(), SyncError> {
        let id = character.character_id;
        let store = &self.app.store;
        let standing = self.app.config.standing;

        let managed = store.managed_contacts(id).await?;
        if desired.is_empty() && managed.is_empty() {
            return Ok(());
        }

        let recently_written = self
            .last_write
            .get(&id)
            .is_some_and(|at| at.elapsed() < CONTACTS_CACHE);
        let verify = mode == Mode::Verify && !recently_written;
        let in_sync =
            managed.len() == desired.len() && desired.iter().all(|c| managed.contains_key(c));
        if in_sync && !verify {
            return Ok(());
        }

        let token = self.app.access_token(character).await?;
        let token = token.as_str();
        let esi = &self.app.esi;
        let current = esi.contacts(id, token).await?;

        let Plan {
            add,
            edit,
            newly_managed,
            removed,
            delete,
            mut restore,
        } = Plan::new(
            desired,
            &managed,
            &current,
            standing,
            Checks {
                verify,
                recently_written,
            },
        );

        let wrote = !(add.is_empty() && edit.is_empty() && delete.is_empty() && restore.is_empty());
        if wrote {
            self.last_write.insert(id, Instant::now());
        }

        if !add.is_empty() {
            esi.add_contacts(id, token, &add, standing).await?;
        }
        if !edit.is_empty() {
            esi.edit_contacts(id, token, &edit, standing).await?;
        }
        store.add_managed(id, &newly_managed).await?;

        if !delete.is_empty() {
            esi.delete_contacts(id, token, &delete).await?;
        }
        restore.sort_by(|a, b| a.0.total_cmp(&b.0));
        for group in restore.chunk_by(|a, b| !differs(a.0, b.0)) {
            let ids: Vec<i64> = group.iter().map(|&(_, contact_id)| contact_id).collect();
            esi.edit_contacts(id, token, &ids, group[0].0).await?;
        }
        store.remove_managed(id, &removed).await?;

        if wrote {
            info!(
                "synced {}: {} added, {} set, {} removed",
                character.name,
                add.len(),
                edit.len(),
                removed.len()
            );
        }
        Ok(())
    }

    /// Pings the owner in the channel so they can log the character in again.
    async fn alert_login_lost(&self, character: &Character) {
        let message = format!(
            "<@{}> the EVE login for **{}** stopped working, so it was removed from the standings network. Use `/standings auth` to add it again.",
            character.discord_user_id, character.name
        );
        let Some(home) = self.app.home() else {
            return;
        };
        if let Err(error) = home.channel_id.say(&self.discord, message).await {
            warn!(
                "could not post the login alert for {}: {error}",
                character.name
            );
        }
    }
}

fn differs(a: f64, b: f64) -> bool {
    (a - b).abs() > 0.001
}

#[derive(Clone, Copy)]
struct Checks {
    /// Compare contacts the bot already manages with what ESI reports.
    verify: bool,
    /// The bot changed this character's contacts so recently that ESI's
    /// cached list may not show it yet.
    recently_written: bool,
}

/// What has to change on one character's contact list.
#[derive(Debug, Default, PartialEq)]
struct Plan {
    /// Contacts to create with the network standing.
    add: Vec<i64>,
    /// Existing contacts to move to the network standing.
    edit: Vec<i64>,
    /// Contacts to start tracking, with the standing they had before.
    newly_managed: Vec<ManagedContact>,
    /// Contacts to stop tracking.
    removed: Vec<i64>,
    /// Contacts the bot created that have to go.
    delete: Vec<i64>,
    /// Contacts to put back to their earlier standing, as (standing, id).
    restore: Vec<(f64, i64)>,
}

impl Plan {
    fn new(
        desired: &HashSet<i64>,
        managed: &HashMap<i64, Option<f64>>,
        current: &HashMap<i64, f64>,
        standing: f64,
        checks: Checks,
    ) -> Self {
        let mut plan = Self::default();

        for &contact_id in desired {
            let is_managed = managed.contains_key(&contact_id);
            if is_managed && !checks.verify {
                continue;
            }
            let existing = current.get(&contact_id).copied();
            match existing {
                None => plan.add.push(contact_id),
                Some(found) if differs(found, standing) => plan.edit.push(contact_id),
                Some(_) => {}
            }
            if !is_managed {
                plan.newly_managed.push(ManagedContact {
                    contact_id,
                    previous_standing: existing,
                });
            }
        }

        for (&contact_id, &previous) in managed {
            if desired.contains(&contact_id) {
                continue;
            }
            plan.removed.push(contact_id);
            // The player already removed it, unless the list is just stale.
            if !current.contains_key(&contact_id) && !checks.recently_written {
                continue;
            }
            match previous {
                None => plan.delete.push(contact_id),
                Some(previous) if differs(previous, standing) => {
                    plan.restore.push((previous, contact_id));
                }
                Some(_) => {}
            }
        }
        plan
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLUE: f64 = 10.0;
    const QUIET: Checks = Checks {
        verify: false,
        recently_written: false,
    };
    const VERIFY: Checks = Checks {
        verify: true,
        recently_written: false,
    };

    fn ids(ids: &[i64]) -> HashSet<i64> {
        ids.iter().copied().collect()
    }

    #[test]
    fn new_member_is_added_and_tracked_as_created() {
        let plan = Plan::new(&ids(&[2]), &HashMap::new(), &HashMap::new(), BLUE, QUIET);
        assert_eq!(plan.add, [2]);
        assert_eq!(plan.newly_managed.len(), 1);
        assert_eq!(plan.newly_managed[0].previous_standing, None);
        assert!(plan.edit.is_empty() && plan.removed.is_empty());
    }

    #[test]
    fn existing_contact_is_edited_and_its_standing_remembered() {
        let current = HashMap::from([(2, -5.0)]);
        let plan = Plan::new(&ids(&[2]), &HashMap::new(), &current, BLUE, QUIET);
        assert!(plan.add.is_empty());
        assert_eq!(plan.edit, [2]);
        assert_eq!(plan.newly_managed[0].previous_standing, Some(-5.0));
    }

    #[test]
    fn managed_contacts_are_left_alone_without_verify() {
        let managed = HashMap::from([(2, None)]);
        let plan = Plan::new(&ids(&[2]), &managed, &HashMap::new(), BLUE, QUIET);
        assert_eq!(plan, Plan::default());
    }

    #[test]
    fn verify_repairs_deleted_and_edited_contacts() {
        let managed = HashMap::from([(2, None), (3, None)]);
        let current = HashMap::from([(3, 0.0)]);
        let plan = Plan::new(&ids(&[2, 3]), &managed, &current, BLUE, VERIFY);
        assert_eq!(plan.add, [2]);
        assert_eq!(plan.edit, [3]);
        assert!(plan.newly_managed.is_empty());
    }

    #[test]
    fn leaver_is_deleted_or_restored() {
        let managed = HashMap::from([(2, None), (3, Some(5.0)), (4, Some(BLUE))]);
        let current = HashMap::from([(2, BLUE), (3, BLUE), (4, BLUE)]);
        let mut plan = Plan::new(&HashSet::new(), &managed, &current, BLUE, QUIET);
        plan.removed.sort_unstable();
        assert_eq!(plan.removed, [2, 3, 4]);
        assert_eq!(plan.delete, [2]);
        assert_eq!(plan.restore, [(5.0, 3)]);
    }

    #[test]
    fn contact_the_player_removed_is_only_forgotten() {
        let managed = HashMap::from([(2, None)]);
        let plan = Plan::new(&HashSet::new(), &managed, &HashMap::new(), BLUE, QUIET);
        assert_eq!(plan.removed, [2]);
        assert!(plan.delete.is_empty());
    }

    #[test]
    fn stale_contact_list_does_not_hide_a_fresh_contact() {
        let managed = HashMap::from([(2, None)]);
        let checks = Checks {
            verify: false,
            recently_written: true,
        };
        let plan = Plan::new(&HashSet::new(), &managed, &HashMap::new(), BLUE, checks);
        assert_eq!(plan.delete, [2]);
    }
}
