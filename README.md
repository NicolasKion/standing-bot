# standing-bot

A Discord bot for EVE Online groups. Everyone who can see one Discord channel links their EVE characters, and the bot makes all of those characters set each other to blue (personal contacts with a standing of 6.7, an odd value so you can tell which contacts the bot set).

- Characters are grouped by Discord account, and one account can link as many as it wants.
- When someone can no longer see the channel (left the server, lost the role), their characters drop out: the others remove them and they remove the others.
- When a character's EVE login stops working, the bot pings the owner in the channel and removes the character.
- Contacts a player already had keep their old standing and get it back when the other side leaves. Contacts the bot created are deleted again.

## Commands

Members use the buttons under the pinned message in the network channel: Add character, My characters and Remove character.

The slash commands do the same and more. They answer privately and work in any channel of the server, which matters if you make the network channel read-only: `/standings` for everyone who can see the network channel, `/standings-admin` for members who can manage the server.

- `/standings-admin setup` makes the channel you run it in the network channel and pins a status message with the buttons there. It does not change the channel's permissions. Running it in another channel moves the network there.
- `/standings auth` gives you a login link. The same link works for each further character, and the page after a login has a button for the next one.
- `/standings characters` lists your linked characters.
- `/standings unlink character:<name>` removes one of them. The name autocompletes from your characters.
- `/standings-admin whois user:<user>` or `/standings-admin whois character:<name>` shows all characters of a user, found directly or through any one of their characters.
- `/standings-admin add user:<user> character:<name>` adds a character by its exact name for a member who cannot log in with it. The rest of the network sets it blue, but the bot cannot set contacts on that character, so its owner has to do that side by hand. A normal login later replaces it.
- `/standings-admin blue name:<name>` makes every character in the network set a corporation or alliance blue, by its exact name. If a corporation and an alliance share the name, the alliance is used. `/standings-admin blue` without a name lists them, and `/standings-admin unblue name:<name>` removes one again. This only works one way: they have to set you blue themselves.
- `/standings-admin kick character:<name>` removes one character and `/standings-admin kick user:<user>` removes all of a user's characters. The owner can link them again with `/standings auth`, so to keep someone out, take away their access to the channel.
- `/standings access` shows which characters of the network are not on the tracked access list yet.
- `/standings access-list` shows the access lists your characters manage, and `/standings access-list name:<name>` picks the one to track.

## Access lists

The bot can check the network against one in-game access list, for example the one on your structures. ESI can only read access lists, so the bot reports who is missing and a manager adds them in game.

1. Someone whose character is manager or admin of the list runs `/standings auth access_lists:True` and logs in with that character. This asks for one more scope so the bot can read the lists that character manages.
2. The same person runs `/standings access-list` to see the lists and `/standings access-list name:<name>` to pick one.
3. From then on the pinned status message and `/standings access` show what has to change on the list: characters of the network that are missing, grouped by Discord account, and characters that are on the list by name but not in the network.

A character counts as on the list only if it has an entry of its own. One that gets in through its corporation or alliance loses access the moment it leaves, so it is reported as missing, marked "corp only" or "alliance only". A blocked character, corporation or alliance is marked "blocked".

## Setup

### 1. Discord application

1. Create an application at https://discord.com/developers/applications and add a bot to it.
2. Under Bot, turn on Server Members Intent and copy the token.
3. Invite it with the scopes `bot` and `applications.commands` and the permissions View Channels, Send Messages and Pin Messages.
4. Turn off Public Bot there if you do not want others to invite your instance. One instance serves one server.

### 2. EVE application

1. Create an application at https://developers.eveonline.com.
2. Set the callback URL to your public URL plus `/callback`, for example `https://standings.example.com/callback`.
3. Add the scopes `esi-characters.read_contacts.v1`, `esi-characters.write_contacts.v1` and `esi-access.read_lists.v1`.
4. Copy the client id and the secret.

### 3. Deploy on Coolify

1. New resource, public repository, this repo, build pack Dockerfile.
2. Port 8080, and give it the domain you used for the callback URL.
3. Add a persistent storage volume mounted at `/data`. It holds the SQLite database with the EVE refresh tokens.
4. Set the environment variables below and deploy.
5. In Discord, run `/standings-admin setup` in the channel whose members should form the network.

| Variable | Required | Description |
| --- | --- | --- |
| `DISCORD_TOKEN` | yes | Bot token |
| `EVE_CLIENT_ID` | yes | EVE application client id |
| `EVE_CLIENT_SECRET` | yes | EVE application secret |
| `PUBLIC_URL` | yes | Public URL of the service, for example `https://standings.example.com` |
| `CONTACT` | yes | How CCP can reach you, sent in the ESI user agent: `eve:Character Name`, `discord:username` or an email |
| `STANDING` | no | Standing to set, default `6.7` |
| `BIND_ADDR` | no | Default `0.0.0.0:8080` |
| `DATABASE_PATH` | no | Default `/data/standing-bot.db` in the image |

### Plain Docker

```sh
cp .env.example .env   # fill it in
docker build -t standing-bot .
docker run -d --env-file .env -p 8080:8080 -v standing-bot-data:/data standing-bot
```

### Local

```sh
cp .env.example .env
cargo run
```

## How it syncs

The bot syncs a few seconds after a login, an unlink, or a member, role or channel change. Every 30 minutes it also compares each character's contacts with ESI and repairs anything a player removed or changed by hand.

EVE allows 1024 contacts per character, so that is the upper limit for the size of the network.

The database stores EVE refresh tokens that can read and write the contacts of the linked characters. Keep the volume private.
