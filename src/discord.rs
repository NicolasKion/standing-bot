//! The Discord side: slash commands and who counts as a channel member.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use serenity::all::{
    Channel, ChannelId, Command, CommandDataOption, CommandDataOptionValue, CommandInteraction,
    CommandOptionType, ComponentInteraction, ComponentInteractionDataKind, Context,
    CreateActionRow, CreateAutocompleteResponse, CreateButton, CreateCommand, CreateCommandOption,
    CreateInteractionResponse, CreateInteractionResponseMessage, CreateSelectMenu,
    CreateSelectMenuKind, CreateSelectMenuOption, EditInteractionResponse, EventHandler,
    GuildChannel, GuildId, GuildMemberUpdateEvent, Http, Interaction, Member, Permissions, Ready,
    Role, RoleId, User, UserId,
};
use serenity::async_trait;
use tracing::{error, info, warn};

use crate::access::Report;
use crate::app::{App, LoginScopes};
use crate::status::{self, truncate};
use crate::store::{Home, TrackedList};

/// Discord returns at most this many members per request.
const MEMBER_PAGE: u64 = 1000;

/// Id of the menu that picks the character to remove.
const UNLINK_MENU: &str = "standings:unlink";

/// Discord shows at most this many autocomplete or menu choices.
const MAX_CHOICES: usize = 25;

/// Discord drops an autocomplete reply after three seconds.
const AUTOCOMPLETE_WAIT: Duration = Duration::from_millis(2500);

/// Everyone who can see the channel, bots excluded.
///
/// This asks Discord directly instead of trusting the gateway cache, because a
/// half-filled cache would make members look like they left.
pub async fn channel_members(
    http: &Http,
    guild_id: GuildId,
    channel_id: ChannelId,
) -> Result<HashSet<UserId>> {
    let guild = guild_id
        .to_partial_guild(http)
        .await
        .context("fetching the Discord server")?;
    let Channel::Guild(channel) = channel_id
        .to_channel(http)
        .await
        .context("fetching the Discord channel")?
    else {
        anyhow::bail!("channel {channel_id} is not a server channel");
    };

    let mut members = HashSet::new();
    let mut after = None;
    loop {
        let page = guild_id
            .members(http, Some(MEMBER_PAGE), after)
            .await
            .context("listing the server members")?;
        for member in &page {
            if !member.user.bot && guild.user_permissions_in(&channel, member).view_channel() {
                members.insert(member.user.id);
            }
        }
        match page.last() {
            Some(last) if page.len() as u64 == MEMBER_PAGE => after = Some(last.user.id),
            _ => return Ok(members),
        }
    }
}

pub struct Handler {
    app: Arc<App>,
}

impl Handler {
    pub fn new(app: Arc<App>) -> Self {
        Self { app }
    }

    async fn command(
        &self,
        http: &Http,
        command: &CommandInteraction,
    ) -> Result<EditInteractionResponse> {
        let reply = EditInteractionResponse::new();
        let name = subcommand_name(command);
        if name == "setup" {
            return self.setup(http, command).await;
        }

        let Some(home) = self.app.home() else {
            return Ok(reply.content(
                "I am not set up yet. A server admin has to run `/standings-admin setup` in the channel I should use.",
            ));
        };
        // Admin commands work in any channel of the server.
        if matches!(name, "whois" | "kick") {
            if command.guild_id != Some(home.guild_id) || !may_manage(command) {
                return Ok(reply.content("Only members who can manage the server can run this."));
            }
            return match name {
                "whois" => self.whois(command).await,
                _ => self.kick(command).await,
            };
        }
        // Commands work anywhere in the server, in case the network channel is
        // read-only. What counts is that the caller can see that channel.
        if !self.sees_home(http, home, command).await? {
            return Ok(reply.content("This is only for members of the standings channel."));
        }

        match name {
            "auth" => self.auth(command),
            "characters" => self.characters(command.user.id).await,
            "unlink" => self.unlink(command).await,
            "access" => {
                let report = self.app.access_report();
                Ok(reply.content(truncate(describe_access(report.as_ref()))))
            }
            "access-list" => self.access_list(command).await,
            other => Ok(reply.content(format!("Unknown command {other}."))),
        }
    }

    /// Suggests the user's own characters or access lists while they type.
    async fn autocomplete(&self, http: &Http, command: &CommandInteraction) {
        let Some(focused) = command.data.autocomplete() else {
            return;
        };
        let user = command.user.id;
        let names: Vec<String> = match subcommand_name(command) {
            "unlink" => match self.app.store.characters_of(user).await {
                Ok(characters) => characters.into_iter().map(|c| c.name).collect(),
                Err(error) => {
                    warn!("could not load characters for autocomplete: {error:#}");
                    Vec::new()
                }
            },
            "access-list" => self.list_names(user).await,
            "whois" | "kick" if may_manage(command) => match self.app.store.characters().await {
                Ok(characters) => characters.into_iter().map(|c| c.name).collect(),
                Err(error) => {
                    warn!("could not load characters for autocomplete: {error:#}");
                    Vec::new()
                }
            },
            _ => Vec::new(),
        };

        let typed = focused.value.trim().to_lowercase();
        let mut response = CreateAutocompleteResponse::new();
        let matches = names
            .iter()
            .filter(|name| name.to_lowercase().contains(&typed))
            .take(MAX_CHOICES);
        for name in matches {
            response = response.add_string_choice(name, name);
        }
        let response = CreateInteractionResponse::Autocomplete(response);
        if let Err(error) = command.create_response(http, response).await {
            warn!("could not answer autocomplete: {error}");
        }
    }

    /// Access list names for autocomplete. Without a cached answer this asks
    /// ESI and waits only as long as Discord gives an autocomplete reply; the
    /// lookup keeps running, so the next keystroke finds the names.
    async fn list_names(&self, user: UserId) -> Vec<String> {
        let cached = self.app.list_names(user);
        if !cached.is_empty() {
            return cached;
        }
        let app = Arc::clone(&self.app);
        let lookup = tokio::spawn(async move { app.managed_lists(user).await });
        match tokio::time::timeout(AUTOCOMPLETE_WAIT, lookup).await {
            Ok(Ok(Ok(lists))) => lists.into_values().collect(),
            Ok(Ok(Err(error))) => {
                warn!("could not load access lists for autocomplete: {error:#}");
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Makes the channel the command ran in the home of the network.
    async fn setup(
        &self,
        http: &Http,
        command: &CommandInteraction,
    ) -> Result<EditInteractionResponse> {
        let reply = EditInteractionResponse::new();
        let Some(guild_id) = command.guild_id else {
            return Ok(reply.content("Run this in a channel of your server."));
        };
        if !may_manage(command) {
            return Ok(reply.content("Only members who can manage the server can run this."));
        }
        // One instance serves one server, and another server must not take it over.
        if self
            .app
            .home()
            .is_some_and(|home| home.guild_id != guild_id)
        {
            return Ok(reply.content("This bot is already set up in another server."));
        }

        let channel_id = command.channel_id;
        let moved = self
            .app
            .home()
            .is_none_or(|home| home.channel_id != channel_id);
        let home = Home {
            guild_id,
            channel_id,
        };
        self.app.store.set_home(home).await?;
        if moved {
            self.app.store.clear_status_message().await?;
        }
        self.app.set_home(home);
        info!("set up in server {guild_id}, channel {channel_id}");

        if let Err(error) =
            status::publish(&self.app, http, "**Standings network**\nSetting up...").await
        {
            warn!("setup could not post in {channel_id}: {error:#}");
            return Ok(reply.content(
                "I cannot post in this channel. Give me View Channel and Send Messages here, then run `/standings-admin setup` again.",
            ));
        }
        self.app.sync.notify_one();

        Ok(reply.content(
            "Done. The members of this channel form the network now, and the pinned message shows its state and has the buttons to add characters.",
        ))
    }

    /// The user an admin command is about: given directly, or as the owner of
    /// a character.
    async fn target(&self, command: &CommandInteraction) -> Result<Result<UserId, String>> {
        if let Some(user) = option(command, "user").and_then(|value| value.as_user_id()) {
            return Ok(Ok(user));
        }
        let Some(name) = option(command, "character").and_then(|value| value.as_str()) else {
            return Ok(Err("Give me a user or a character name.".to_owned()));
        };
        Ok(self
            .app
            .store
            .owner_of(name)
            .await?
            .ok_or_else(|| format!("No linked character is named {name}.")))
    }

    /// Lists every character of a user, found directly or through one of
    /// their characters.
    async fn whois(&self, command: &CommandInteraction) -> Result<EditInteractionResponse> {
        let reply = EditInteractionResponse::new();
        let user = match self.target(command).await? {
            Ok(user) => user,
            Err(problem) => return Ok(reply.content(problem)),
        };
        let characters = self.app.store.characters_of(user).await?;
        if characters.is_empty() {
            return Ok(reply.content(format!("<@{user}> has no characters linked.")));
        }
        let names: Vec<String> = characters.iter().map(|c| format!("- {}", c.name)).collect();
        Ok(reply.content(truncate(format!(
            "<@{user}> has {} characters linked:\n{}",
            characters.len(),
            names.join("\n")
        ))))
    }

    /// Removes one character, or all characters of a user, from the network.
    async fn kick(&self, command: &CommandInteraction) -> Result<EditInteractionResponse> {
        let reply = EditInteractionResponse::new();
        let store = &self.app.store;

        if let Some(user) = option(command, "user").and_then(|value| value.as_user_id()) {
            let removed = store.request_unlink_all(user).await?;
            self.app.sync.notify_one();
            info!("{} kicked all characters of {user}", command.user.id);
            return Ok(reply.content(format!(
                "Removing {removed} characters of <@{user}> from the network."
            )));
        }

        let Some(name) = option(command, "character").and_then(|value| value.as_str()) else {
            return Ok(reply.content("Give me a user or a character name."));
        };
        let Some(owner) = store.owner_of(name).await? else {
            return Ok(reply.content(format!("No linked character is named {name}.")));
        };
        store.request_unlink(owner, name).await?;
        self.app.sync.notify_one();
        info!("{} kicked {name}", command.user.id);
        Ok(reply.content(format!(
            "Removing {name} of <@{owner}> from the network. Its contacts get cleaned up in a moment."
        )))
    }

    /// Whether the caller is in the bot's server and can see the network channel.
    async fn sees_home(
        &self,
        http: &Http,
        home: Home,
        command: &CommandInteraction,
    ) -> Result<bool> {
        if command.guild_id != Some(home.guild_id) {
            return Ok(false);
        }
        if command.channel_id == home.channel_id {
            return Ok(true);
        }
        let Some(member) = command.member.as_deref() else {
            return Ok(false);
        };
        let guild = home.guild_id.to_partial_guild(http).await?;
        let Channel::Guild(channel) = home.channel_id.to_channel(http).await? else {
            return Ok(false);
        };
        Ok(guild.user_permissions_in(&channel, member).view_channel())
    }

    /// Answers a button or menu of the status message. Only people who see
    /// the channel can press them, so no further membership check is needed.
    async fn component(&self, http: &Http, component: &ComponentInteraction) -> Result<()> {
        let user = component.user.id;
        let message = CreateInteractionResponseMessage::new().ephemeral(true);
        if self.app.home().map(|home| home.channel_id) != Some(component.channel_id) {
            let message = message.content("This message is no longer in use.");
            component
                .create_response(http, CreateInteractionResponse::Message(message))
                .await?;
            return Ok(());
        }

        let response = match component.data.custom_id.as_str() {
            status::ADD_BUTTON => {
                let (text, button) = self.login_prompt(user, LoginScopes::Contacts)?;
                let row = CreateActionRow::Buttons(vec![button]);
                CreateInteractionResponse::Message(message.content(text).components(vec![row]))
            }
            status::MINE_BUTTON => {
                let text = self.character_list(user).await?;
                CreateInteractionResponse::Message(message.content(text))
            }
            status::REMOVE_BUTTON => {
                let characters = self.app.store.characters_of(user).await?;
                if characters.is_empty() {
                    CreateInteractionResponse::Message(
                        message.content("You have no characters linked."),
                    )
                } else {
                    let options = characters
                        .iter()
                        .take(MAX_CHOICES)
                        .map(|c| CreateSelectMenuOption::new(&c.name, &c.name))
                        .collect();
                    let menu = CreateSelectMenu::new(
                        UNLINK_MENU,
                        CreateSelectMenuKind::String { options },
                    )
                    .placeholder("Pick the character to remove");
                    CreateInteractionResponse::Message(
                        message
                            .content("Which character should leave the network?")
                            .components(vec![CreateActionRow::SelectMenu(menu)]),
                    )
                }
            }
            UNLINK_MENU => {
                let picked = match &component.data.kind {
                    ComponentInteractionDataKind::StringSelect { values } => values.first(),
                    _ => None,
                };
                let name = picked.context("the menu came without a choice")?;
                let text = if self.app.store.request_unlink(user, name).await? {
                    self.app.sync.notify_one();
                    format!(
                        "Removing {name} from the network. Its contacts get cleaned up in a moment."
                    )
                } else {
                    format!("You have no linked character named {name}.")
                };
                CreateInteractionResponse::UpdateMessage(
                    CreateInteractionResponseMessage::new()
                        .content(text)
                        .components(Vec::new()),
                )
            }
            _ => return Ok(()),
        };
        component.create_response(http, response).await?;
        Ok(())
    }

    /// Opens a login session and returns the text and link button for it.
    fn login_prompt(
        &self,
        user: UserId,
        scopes: LoginScopes,
    ) -> Result<(&'static str, CreateButton)> {
        let state = self.app.begin_login(user, scopes);
        let url = self.app.login_url(&state, scopes)?;
        let button = CreateButton::new_link(url).label("Log in with EVE Online");
        Ok((
            "Log in with the character you want to add. You can use the same link again for each further character, until it has been unused for 15 minutes.",
            button,
        ))
    }

    fn auth(&self, command: &CommandInteraction) -> Result<EditInteractionResponse> {
        let wants_access_lists = option(command, "access_lists")
            .and_then(CommandDataOptionValue::as_bool)
            .unwrap_or(false);
        let scopes = if wants_access_lists {
            LoginScopes::ContactsAndAccessLists
        } else {
            LoginScopes::Contacts
        };
        let (text, button) = self.login_prompt(command.user.id, scopes)?;
        Ok(EditInteractionResponse::new()
            .content(text)
            .components(vec![CreateActionRow::Buttons(vec![button])]))
    }

    async fn characters(&self, user: UserId) -> Result<EditInteractionResponse> {
        Ok(EditInteractionResponse::new().content(self.character_list(user).await?))
    }

    /// The text listing a user's linked characters.
    async fn character_list(&self, user: UserId) -> Result<String> {
        let characters = self.app.store.characters_of(user).await?;
        if characters.is_empty() {
            return Ok("You have no characters linked.".to_owned());
        }
        let names: Vec<String> = characters
            .iter()
            .map(|c| {
                let note = if c.reads_access_lists {
                    " (reads access lists)"
                } else {
                    ""
                };
                format!("- {}{note}", c.name)
            })
            .collect();
        Ok(truncate(format!("Your characters:\n{}", names.join("\n"))))
    }

    async fn unlink(&self, command: &CommandInteraction) -> Result<EditInteractionResponse> {
        let name = option(command, "character")
            .and_then(CommandDataOptionValue::as_str)
            .context("the unlink command came without a name")?;
        let reply = EditInteractionResponse::new();
        if self.app.store.request_unlink(command.user.id, name).await? {
            self.app.sync.notify_one();
            Ok(reply.content(format!(
                "Removing {name} from the network. Its contacts get cleaned up in a moment."
            )))
        } else {
            Ok(reply.content(format!("You have no linked character named {name}.")))
        }
    }

    /// Shows the access lists the caller's characters manage, or picks the one
    /// to track.
    async fn access_list(&self, command: &CommandInteraction) -> Result<EditInteractionResponse> {
        let reply = EditInteractionResponse::new();
        let readers: Vec<_> = self
            .app
            .store
            .characters_of(command.user.id)
            .await?
            .into_iter()
            .filter(|c| c.reads_access_lists)
            .collect();
        if readers.is_empty() {
            return Ok(reply.content(
                "None of your characters lets me read access lists. Run `/standings auth access_lists:True` and log in with a character that is manager or admin of the list.",
            ));
        }

        let lists = self.app.managed_lists(command.user.id).await?;
        if lists.is_empty() {
            return Ok(
                reply.content("I found no access list your characters are manager or admin of.")
            );
        }

        let Some(wanted) = option(command, "name").and_then(CommandDataOptionValue::as_str) else {
            let names: Vec<String> = lists.values().map(|name| format!("- {name}")).collect();
            let tracked = match self.app.store.tracked_list().await? {
                Some(list) => format!("Tracked right now: {}.", list.name),
                None => "No list is tracked yet.".to_owned(),
            };
            return Ok(reply.content(truncate(format!(
                "Access lists I can read through your characters:\n{}\n{tracked} Pick one with `/standings access-list name:<name>`.",
                names.join("\n")
            ))));
        };

        let wanted = wanted.trim();
        let Some((&id, name)) = lists
            .iter()
            .find(|(_, name)| name.trim().eq_ignore_ascii_case(wanted))
        else {
            return Ok(reply.content(format!(
                "None of your characters manages an access list named {wanted}."
            )));
        };
        let list = TrackedList {
            id,
            name: name.clone(),
        };
        self.app.store.set_tracked_list(&list).await?;
        self.app.sync.notify_one();
        Ok(reply.content(format!(
            "Tracking the access list {name} now. `/standings access` shows who is missing in a moment."
        )))
    }
}

fn may_manage(command: &CommandInteraction) -> bool {
    command
        .member
        .as_ref()
        .and_then(|member| member.permissions)
        .is_some_and(|permissions| permissions.administrator() || permissions.manage_guild())
}

/// The subcommand that was run, with its options.
fn subcommand(command: &CommandInteraction) -> Option<(&str, &[CommandDataOption])> {
    let first = command.data.options.first()?;
    match &first.value {
        CommandDataOptionValue::SubCommand(options) => Some((first.name.as_str(), options)),
        _ => None,
    }
}

fn subcommand_name(command: &CommandInteraction) -> &str {
    subcommand(command).map_or("", |(name, _)| name)
}

fn option<'a>(command: &'a CommandInteraction, name: &str) -> Option<&'a CommandDataOptionValue> {
    let (_, options) = subcommand(command)?;
    options
        .iter()
        .find(|option| option.name == name)
        .map(|option| &option.value)
}

/// The text `/standings access` answers with.
fn describe_access(report: Option<&Report>) -> String {
    let section = status::access_section(report);
    match report {
        Some(report) => {
            let checked = report
                .checked_at
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_secs());
            format!("{section}\nChecked <t:{checked}:R>.")
        }
        None => section,
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        info!("connected to Discord as {}", ready.user.name);
        let sub = |name, description| {
            CreateCommandOption::new(CommandOptionType::SubCommand, name, description)
        };
        let user =
            |description| CreateCommandOption::new(CommandOptionType::User, "user", description);
        let character = |description| {
            CreateCommandOption::new(CommandOptionType::String, "character", description)
                .set_autocomplete(true)
        };

        let commands = vec![
            CreateCommand::new("standings")
                .description("Your EVE characters in the standings network")
                .add_option(
                    sub(
                        "auth",
                        "Add one of your EVE characters to the standings network",
                    )
                    .add_sub_option(CreateCommandOption::new(
                        CommandOptionType::Boolean,
                        "access_lists",
                        "Also let the bot read the access lists this character manages",
                    )),
                )
                .add_option(sub("characters", "List your linked EVE characters"))
                .add_option(
                    sub(
                        "unlink",
                        "Remove one of your EVE characters from the standings network",
                    )
                    .add_sub_option(character("Name of the character").required(true)),
                )
                .add_option(sub(
                    "access",
                    "Show which characters are not on the access list yet",
                ))
                .add_option(
                    sub(
                        "access-list",
                        "Show the access lists you manage, or pick the one to track",
                    )
                    .add_sub_option(
                        CreateCommandOption::new(
                            CommandOptionType::String,
                            "name",
                            "Name of the access list to track",
                        )
                        .set_autocomplete(true),
                    ),
                ),
            CreateCommand::new("standings-admin")
                .description("Manage the standings network")
                .default_member_permissions(Permissions::MANAGE_GUILD)
                .add_option(sub(
                    "setup",
                    "Make this channel the standings network channel",
                ))
                .add_option(
                    sub(
                        "whois",
                        "Show all linked characters of a user, or of a character's owner",
                    )
                    .add_sub_option(user("The Discord user"))
                    .add_sub_option(character("Any character of the user")),
                )
                .add_option(
                    sub(
                        "kick",
                        "Remove a character, or all characters of a user, from the network",
                    )
                    .add_sub_option(user("Remove all characters of this user"))
                    .add_sub_option(character("Remove this one character")),
                ),
        ];
        if let Err(error) = Command::set_global_commands(&ctx.http, commands).await {
            error!("could not register the slash commands: {error}");
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let command = match interaction {
            Interaction::Command(command) => command,
            Interaction::Autocomplete(command) => {
                self.autocomplete(&ctx.http, &command).await;
                return;
            }
            Interaction::Component(component) => {
                if let Err(error) = self.component(&ctx.http, &component).await {
                    error!("button {} failed: {error:#}", component.data.custom_id);
                }
                return;
            }
            _ => return,
        };
        let name = subcommand_name(&command);
        // Some commands call ESI, which can take longer than Discord waits.
        if let Err(error) = command.defer_ephemeral(&ctx.http).await {
            error!("could not acknowledge command {name}: {error}");
            return;
        }
        let reply = self
            .command(&ctx.http, &command)
            .await
            .unwrap_or_else(|error| {
                error!("command {name} failed: {error:#}");
                EditInteractionResponse::new().content("Something went wrong, try again in a bit.")
            });
        if let Err(error) = command.edit_response(&ctx.http, reply).await {
            error!("could not answer command {name}: {error}");
        }
    }

    // Anything that can change who sees the channel wakes the sync.

    async fn guild_member_removal(
        &self,
        _ctx: Context,
        _guild_id: GuildId,
        _user: User,
        _member: Option<Member>,
    ) {
        self.app.sync.notify_one();
    }

    async fn guild_member_update(
        &self,
        _ctx: Context,
        _old: Option<Member>,
        _new: Option<Member>,
        _event: GuildMemberUpdateEvent,
    ) {
        self.app.sync.notify_one();
    }

    async fn channel_update(&self, _ctx: Context, _old: Option<GuildChannel>, new: GuildChannel) {
        if self
            .app
            .home()
            .is_some_and(|home| home.channel_id == new.id)
        {
            self.app.sync.notify_one();
        }
    }

    async fn guild_role_update(&self, _ctx: Context, _old: Option<Role>, _new: Role) {
        self.app.sync.notify_one();
    }

    async fn guild_role_delete(
        &self,
        _ctx: Context,
        _guild_id: GuildId,
        _role_id: RoleId,
        _role: Option<Role>,
    ) {
        self.app.sync.notify_one();
    }
}
