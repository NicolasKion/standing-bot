//! Access lists: who of the network is allowed on the tracked list.

use std::collections::HashSet;
use std::time::SystemTime;

use serde::Deserialize;
use serenity::model::id::UserId;

/// What an access list entry grants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum Access {
    Unspecified,
    Allowed,
    Blocked,
    Manager,
    Admin,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
pub struct CharacterEntry {
    pub character_id: i64,
    pub access: Access,
}

#[derive(Debug, Deserialize)]
pub struct CorporationEntry {
    pub corporation_id: i64,
    pub access: Access,
}

#[derive(Debug, Deserialize)]
pub struct AllianceEntry {
    pub alliance_id: i64,
    pub access: Access,
}

#[derive(Debug, Default, Deserialize)]
pub struct Membership {
    /// Everyone is allowed unless blocked.
    #[serde(default)]
    pub allow_everyone: bool,
    #[serde(default)]
    pub characters: Vec<CharacterEntry>,
    #[serde(default)]
    pub corporations: Vec<CorporationEntry>,
    #[serde(default)]
    pub alliances: Vec<AllianceEntry>,
}

#[derive(Debug, Deserialize)]
pub struct AccessList {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub membership: Membership,
}

/// The corporation and alliance a character is in.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Affiliation {
    pub character_id: i64,
    pub corporation_id: i64,
    pub alliance_id: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allowed,
    NotListed,
    Blocked,
}

impl Membership {
    /// Whether a character gets access.
    ///
    /// The most specific entry decides: the character's own, then its
    /// corporation's, then its alliance's. Without any of those the list's
    /// "everyone" setting applies.
    pub fn verdict(&self, who: &Affiliation) -> Verdict {
        let character = self
            .characters
            .iter()
            .find(|entry| entry.character_id == who.character_id)
            .map(|entry| entry.access);
        let corporation = self
            .corporations
            .iter()
            .find(|entry| entry.corporation_id == who.corporation_id)
            .map(|entry| entry.access);
        let alliance = self
            .alliances
            .iter()
            .find(|entry| Some(entry.alliance_id) == who.alliance_id)
            .map(|entry| entry.access);

        for access in [character, corporation, alliance].into_iter().flatten() {
            match access {
                Access::Blocked => return Verdict::Blocked,
                Access::Allowed | Access::Manager | Access::Admin => return Verdict::Allowed,
                Access::Unspecified | Access::Unknown => {}
            }
        }
        if self.allow_everyone {
            Verdict::Allowed
        } else {
            Verdict::NotListed
        }
    }
}

impl Membership {
    /// Characters that were added to the list by name but are not part of the
    /// network. Managers, admins and blocked characters are left alone.
    pub fn extra_characters(&self, network: &HashSet<i64>) -> Vec<i64> {
        self.characters
            .iter()
            .filter(|entry| entry.access == Access::Allowed)
            .map(|entry| entry.character_id)
            .filter(|id| !network.contains(id))
            .collect()
    }
}

/// A network character that is not allowed on the tracked list.
#[derive(Debug, Clone)]
pub struct Outstanding {
    pub name: String,
    pub owner: UserId,
    pub blocked: bool,
}

#[derive(Debug, Clone)]
pub enum Outcome {
    /// No linked character could read the list.
    Unreadable,
    Checked {
        /// Network characters that are not allowed on the list.
        missing: Vec<Outstanding>,
        /// Names of characters with their own entry that are not in the network.
        extra: Vec<String>,
    },
}

/// Result of the latest access list check.
#[derive(Debug, Clone)]
pub struct Report {
    pub list_name: String,
    pub checked_at: SystemTime,
    pub outcome: Outcome,
}

#[cfg(test)]
mod tests {
    use super::*;

    const PILOT: Affiliation = Affiliation {
        character_id: 1,
        corporation_id: 10,
        alliance_id: Some(100),
    };

    fn alliance(access: Access) -> AllianceEntry {
        AllianceEntry {
            alliance_id: 100,
            access,
        }
    }

    #[test]
    fn empty_list_lists_nobody() {
        assert_eq!(Membership::default().verdict(&PILOT), Verdict::NotListed);
    }

    #[test]
    fn alliance_entry_covers_its_characters() {
        let list = Membership {
            alliances: vec![alliance(Access::Allowed)],
            ..Membership::default()
        };
        assert_eq!(list.verdict(&PILOT), Verdict::Allowed);
    }

    #[test]
    fn corporation_entry_covers_its_characters() {
        let list = Membership {
            corporations: vec![CorporationEntry {
                corporation_id: 10,
                access: Access::Manager,
            }],
            ..Membership::default()
        };
        assert_eq!(list.verdict(&PILOT), Verdict::Allowed);
    }

    #[test]
    fn blocked_character_beats_allowed_alliance() {
        let list = Membership {
            characters: vec![CharacterEntry {
                character_id: 1,
                access: Access::Blocked,
            }],
            alliances: vec![alliance(Access::Allowed)],
            ..Membership::default()
        };
        assert_eq!(list.verdict(&PILOT), Verdict::Blocked);
    }

    #[test]
    fn allowed_character_beats_blocked_alliance() {
        let list = Membership {
            characters: vec![CharacterEntry {
                character_id: 1,
                access: Access::Allowed,
            }],
            alliances: vec![alliance(Access::Blocked)],
            ..Membership::default()
        };
        assert_eq!(list.verdict(&PILOT), Verdict::Allowed);
    }

    #[test]
    fn everyone_is_allowed_unless_blocked() {
        let open = Membership {
            allow_everyone: true,
            ..Membership::default()
        };
        assert_eq!(open.verdict(&PILOT), Verdict::Allowed);

        let blocked = Membership {
            allow_everyone: true,
            alliances: vec![alliance(Access::Blocked)],
            ..Membership::default()
        };
        assert_eq!(blocked.verdict(&PILOT), Verdict::Blocked);
    }

    #[test]
    fn extra_characters_are_allowed_entries_outside_the_network() {
        let entry = |character_id, access| CharacterEntry {
            character_id,
            access,
        };
        let list = Membership {
            characters: vec![
                entry(1, Access::Allowed),
                entry(2, Access::Allowed),
                entry(3, Access::Admin),
                entry(4, Access::Blocked),
            ],
            ..Membership::default()
        };
        assert_eq!(list.extra_characters(&HashSet::from([1])), [2]);
    }

    #[test]
    fn other_alliance_does_not_count() {
        let list = Membership {
            alliances: vec![AllianceEntry {
                alliance_id: 999,
                access: Access::Allowed,
            }],
            ..Membership::default()
        };
        assert_eq!(list.verdict(&PILOT), Verdict::NotListed);
    }
}
