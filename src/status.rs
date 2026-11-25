//! The pinned message in the network channel that shows the current state.

use anyhow::{Context, Result};
use serenity::all::{
    ButtonStyle, CreateActionRow, CreateAllowedMentions, CreateButton, CreateMessage, EditMessage,
    Http, HttpError, MessageId, StatusCode,
};
use tracing::warn;

use crate::access::{Group, Outcome, Report, Verdict};
use crate::app::App;
use crate::store::BlueContact;

/// Ids of the buttons under the status message.
pub const ADD_BUTTON: &str = "standings:add";
pub const MINE_BUTTON: &str = "standings:mine";
pub const REMOVE_BUTTON: &str = "standings:remove";

/// Discord takes 2000 characters per message. This leaves room for the cut mark.
const MESSAGE_LIMIT: usize = 1900;

/// Size of the network.
#[derive(Debug, Clone, Copy)]
pub struct Summary {
    pub members: usize,
    pub characters: usize,
}

/// The full text of the pinned message.
pub fn render(summary: Summary, blues: &[BlueContact], report: Option<&Report>) -> String {
    truncate(format!(
        "**Standings network**\n{} characters of {} members set each other to blue. Use the buttons below to add or remove yours.\n{}\n{}",
        summary.characters,
        summary.members,
        blue_section(blues),
        access_section(report)
    ))
}

/// The corporations and alliances the network also sets blue.
pub fn blue_section(blues: &[BlueContact]) -> String {
    if blues.is_empty() {
        return String::new();
    }
    let names: Vec<String> = blues
        .iter()
        .map(|blue| format!("{} ({})", blue.name, blue.kind.as_str()))
        .collect();
    format!("Also set blue: {}.\n", names.join(", "))
}

/// What has to change on the tracked access list.
pub fn access_section(report: Option<&Report>) -> String {
    let Some(report) = report else {
        return "No access list is tracked yet. A manager of the list can pick one with `/standings access-list`.".to_owned();
    };
    let list = &report.list_name;
    let (missing, extra) = match &report.outcome {
        Outcome::Unreadable => {
            return format!(
                "I cannot read the access list **{list}** right now. A character that is manager or admin of it has to be linked with `/standings auth access_lists:True`."
            );
        }
        Outcome::Checked { missing, extra } => (missing, extra),
    };
    if missing.is_empty() && extra.is_empty() {
        return format!(
            "**Access list {list}**\nEvery character in the network is on it by name, and nobody else is."
        );
    }

    let mut text = format!("**Access list {list}**");
    if !missing.is_empty() {
        // Sorted by name, the way the list reads in game.
        let mut sorted: Vec<_> = missing.iter().collect();
        sorted.sort_by_cached_key(|entry| entry.name.to_lowercase());
        text.push_str(&format!("\nAdd to the ACL ({}):", missing.len()));
        for entry in sorted {
            let note = match entry.verdict {
                Verdict::Blocked => ", blocked",
                Verdict::Inherited(Group::Corporation) => ", corp only",
                Verdict::Inherited(Group::Alliance) => ", alliance only",
                Verdict::Allowed | Verdict::NotListed => "",
            };
            text.push_str(&format!("\n- `{}` (<@{}>{note})", entry.name, entry.owner));
        }
    }
    if !extra.is_empty() {
        text.push_str(&format!(
            "\nNot linked, remove from the ACL ({}):",
            extra.len()
        ));
        text.push_str(&code_block(extra.iter().cloned()));
    }
    text.trim_end().to_owned()
}

/// One name per line in a code block, so the list is easy to read and copy.
fn code_block(lines: impl Iterator<Item = String>) -> String {
    let lines: Vec<String> = lines.collect();
    format!("\n```\n{}\n```\n", lines.join("\n"))
}

/// Writes the text into the pinned message, creating and pinning it if it is
/// not there yet or was deleted.
pub async fn publish(app: &App, http: &Http, text: &str) -> Result<()> {
    let Some(home) = app.home() else {
        return Ok(());
    };
    let channel = home.channel_id;

    if let Some(id) = app.store.status_message().await? {
        let edit = EditMessage::new().content(text).components(buttons());
        match channel.edit_message(http, MessageId::new(id), edit).await {
            Ok(_) => return Ok(()),
            Err(error) if is_not_found(&error) => {}
            Err(error) => return Err(error).context("editing the status message"),
        }
    }

    let message = CreateMessage::new()
        .content(text)
        .components(buttons())
        .allowed_mentions(CreateAllowedMentions::new());
    let message = channel
        .send_message(http, message)
        .await
        .context("posting the status message")?;
    app.store.set_status_message(message.id.get()).await?;
    if let Err(error) = message.pin(http).await {
        warn!("could not pin the status message: {error}");
    }
    Ok(())
}

/// The buttons for the everyday actions, which also work when the channel is
/// read-only and slash commands cannot be typed there.
fn buttons() -> Vec<CreateActionRow> {
    vec![CreateActionRow::Buttons(vec![
        CreateButton::new(ADD_BUTTON)
            .label("Add character")
            .style(ButtonStyle::Primary),
        CreateButton::new(MINE_BUTTON)
            .label("My characters")
            .style(ButtonStyle::Secondary),
        CreateButton::new(REMOVE_BUTTON)
            .label("Remove character")
            .style(ButtonStyle::Secondary),
    ])]
}

fn is_not_found(error: &serenity::Error) -> bool {
    matches!(
        error,
        serenity::Error::Http(HttpError::UnsuccessfulRequest(response))
            if response.status_code == StatusCode::NOT_FOUND
    )
}

/// Cuts a message to what Discord accepts.
pub fn truncate(mut content: String) -> String {
    if content.len() > MESSAGE_LIMIT {
        let mut end = MESSAGE_LIMIT;
        while !content.is_char_boundary(end) {
            end -= 1;
        }
        content.truncate(end);
        // Close a code block the cut left open.
        if content.matches("```").count() % 2 == 1 {
            content.push_str("\n```");
        }
        content.push_str("\n...");
    }
    content
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use serenity::all::UserId;

    use super::*;
    use crate::access::Outstanding;

    fn report(missing: Vec<Outstanding>, extra: Vec<String>) -> Report {
        Report {
            list_name: "Home".to_owned(),
            checked_at: SystemTime::now(),
            outcome: Outcome::Checked { missing, extra },
        }
    }

    fn outstanding(name: &str, owner: u64) -> Outstanding {
        Outstanding {
            name: name.to_owned(),
            owner: UserId::new(owner),
            verdict: Verdict::NotListed,
        }
    }

    #[test]
    fn clean_list_says_so() {
        let text = access_section(Some(&report(Vec::new(), Vec::new())));
        assert!(
            text.contains("Every character in the network is on it by name"),
            "{text}"
        );
    }

    #[test]
    fn missing_characters_are_sorted_by_name_with_their_owner() {
        let missing = vec![
            outstanding("beta", 7),
            outstanding("Gamma", 7),
            outstanding("Alpha", 9),
        ];
        let text = access_section(Some(&report(missing, Vec::new())));
        assert!(
            text.contains(
                "Add to the ACL (3):\n- `Alpha` (<@9>)\n- `beta` (<@7>)\n- `Gamma` (<@7>)"
            ),
            "{text}"
        );
        assert!(!text.contains("remove from the ACL"), "{text}");
    }

    #[test]
    fn characters_on_the_list_through_their_corporation_are_marked() {
        let mut alpha = outstanding("Alpha", 7);
        alpha.verdict = Verdict::Inherited(Group::Corporation);
        let text = access_section(Some(&report(vec![alpha], Vec::new())));
        assert!(text.contains("- `Alpha` (<@7>, corp only)"), "{text}");
    }

    #[test]
    fn extra_characters_are_listed_for_removal() {
        let text = access_section(Some(&report(Vec::new(), vec!["Gamma".to_owned()])));
        assert!(
            text.contains("Not linked, remove from the ACL (1):\n```\nGamma\n```"),
            "{text}"
        );
    }

    #[test]
    fn blue_contacts_are_listed() {
        use crate::store::BlueKind;
        let blues = vec![BlueContact {
            contact_id: 99_000_001,
            name: "Friends".to_owned(),
            kind: BlueKind::Alliance,
        }];
        assert_eq!(blue_section(&blues), "Also set blue: Friends (alliance).\n");
        assert_eq!(blue_section(&[]), "");
    }

    #[test]
    fn long_text_is_cut() {
        let text = truncate("ä".repeat(3000));
        assert!(text.len() <= MESSAGE_LIMIT + 4, "{}", text.len());
        assert!(text.ends_with("..."));
    }

    #[test]
    fn cut_inside_a_code_block_closes_it() {
        let text = truncate(format!("```\n{}", "a\n".repeat(1500)));
        assert!(text.ends_with("```\n..."), "{text}");
    }
}
