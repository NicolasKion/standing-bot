//! The pinned message in the network channel that shows the current state.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serenity::all::{
    CreateAllowedMentions, CreateMessage, EditMessage, Http, HttpError, MessageId, StatusCode,
    UserId,
};
use tracing::warn;

use crate::access::{Outcome, Report};
use crate::app::App;

/// Discord takes 2000 characters per message. This leaves room for the cut mark.
const MESSAGE_LIMIT: usize = 1900;

/// Size of the network.
#[derive(Debug, Clone, Copy)]
pub struct Summary {
    pub members: usize,
    pub characters: usize,
}

/// The full text of the pinned message.
pub fn render(summary: Summary, report: Option<&Report>) -> String {
    truncate(format!(
        "**Standings network**\n{} characters of {} members set each other to blue. Add yours with `/standings auth`, list them with `/standings characters`.\n\n{}",
        summary.characters,
        summary.members,
        access_section(report)
    ))
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
            "**Access list {list}**\nEveryone in the network is on it, and nobody else is."
        );
    }

    let mut text = format!("**Access list {list}**");
    if !missing.is_empty() {
        let mut by_owner: BTreeMap<UserId, Vec<String>> = BTreeMap::new();
        for entry in missing {
            let note = if entry.blocked { " (blocked)" } else { "" };
            by_owner
                .entry(entry.owner)
                .or_default()
                .push(format!("{}{note}", entry.name));
        }
        text.push_str(&format!("\nAdd to the list ({}):", missing.len()));
        for (owner, names) in &by_owner {
            text.push_str(&format!("\n- {} (<@{owner}>)", names.join(", ")));
        }
    }
    if !extra.is_empty() {
        text.push_str(&format!("\nRemove from the list ({}):", extra.len()));
        for name in extra {
            text.push_str(&format!("\n- {name}"));
        }
    }
    text
}

/// Writes the text into the pinned message, creating and pinning it if it is
/// not there yet or was deleted.
pub async fn publish(app: &App, http: &Http, text: &str) -> Result<()> {
    let Some(home) = app.home() else {
        return Ok(());
    };
    let channel = home.channel_id;

    if let Some(id) = app.store.status_message().await? {
        let edit = EditMessage::new().content(text);
        match channel.edit_message(http, MessageId::new(id), edit).await {
            Ok(_) => return Ok(()),
            Err(error) if is_not_found(&error) => {}
            Err(error) => return Err(error).context("editing the status message"),
        }
    }

    let message = CreateMessage::new()
        .content(text)
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
        content.push_str("\n...");
    }
    content
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

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
            blocked: false,
        }
    }

    #[test]
    fn clean_list_says_so() {
        let text = access_section(Some(&report(Vec::new(), Vec::new())));
        assert!(text.contains("Everyone in the network is on it"), "{text}");
    }

    #[test]
    fn missing_characters_are_grouped_by_owner() {
        let missing = vec![outstanding("Alpha", 7), outstanding("Beta", 7)];
        let text = access_section(Some(&report(missing, Vec::new())));
        assert!(
            text.contains("Add to the list (2):\n- Alpha, Beta (<@7>)"),
            "{text}"
        );
        assert!(!text.contains("Remove from the list"), "{text}");
    }

    #[test]
    fn extra_characters_are_listed_for_removal() {
        let text = access_section(Some(&report(Vec::new(), vec!["Gamma".to_owned()])));
        assert!(
            text.contains("Remove from the list (1):\n- Gamma"),
            "{text}"
        );
    }

    #[test]
    fn long_text_is_cut() {
        let text = truncate("ä".repeat(3000));
        assert!(text.len() <= MESSAGE_LIMIT + 4, "{}", text.len());
        assert!(text.ends_with("..."));
    }
}
