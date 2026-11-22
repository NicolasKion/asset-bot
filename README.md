# asset-bot

A Discord bot for EVE Online. You say what you want to have at a station or structure, the bot compares that with the assets of your characters or your corporation, and answers with what is missing as a list you can paste into the multibuy window.

- A stockpile is a named list of items and quantities at one location.
- Personal stockpiles are yours alone and work anywhere, also in a DM with the bot. Shared ones belong to a server: everyone there can check them, the creator and members who can manage the server can change them.
- A stockpile counts the assets of all your characters, of one character, or of a corporation. They are always read through the characters of whoever created the stockpile.
- By default everything at the location counts, including what is inside containers, but not what is fitted to or loaded in ships, since that is in use. Hulls always count. With `ships:True` a stockpile also counts what is in ships, which is what you want for a doctrine stock of fitted ships. A stockpile can be narrowed to one named container or one corporation hangar.

## Commands

All answers are private unless you ask for a public one.

- `/assets auth` gives you a login link. The same link works for each further character. With `corporation:True` the login also asks for the corporation's assets, which only works for a character with the Director role.
- `/assets characters` lists your linked characters, `/assets unlink character:<name>` removes one.
- `/stockpile create name:<name> location:<station or structure>` starts a stockpile. The location suggests the places where you have assets. For a place where you have nothing yet, type its name: the bot searches for it, and finds structures one of your characters may dock at. Options: `assets` (whose assets count), `shared`, `ships`, `container` and `hangar`.
- `/stockpile paste name:<stockpile>` opens a window to paste into. It takes multibuy lines (`Nanite Repair Paste 500`), copies from the inventory, and EFT fits. `times:10` wants everything ten times, so one fit becomes ten ships. `mode` says what happens to items the stockpile wants already: add on top (wanting 5000 and pasting 5000 wants 10000, the default), keep the higher number (still 5000), or replace the whole list. For lists longer than 4000 characters attach a text file with `file`.
- `/stockpile set name:<stockpile> item:<name> quantity:<n>` sets one item. 0 removes it.
- `/stockpile check` compares all your stockpiles with the assets, `/stockpile check name:<stockpile>` only one. `public:True` posts the answer in the channel. Buttons under the answer send you the lists again in another format: multibuy, EFT style (`Name x5`) or tab separated for a spreadsheet.
- `/stockpile copy name:<stockpile>` gives you what the stockpile wants as text and as a file. Edit it and send it back with `/stockpile paste` and `mode: Replace the whole list`, or press Edit here to change the list in a window right in Discord (for lists up to 4000 characters).
- `/stockpile edit name:<stockpile> ships:<yes or no>` changes whether what is in ships counts.
- `/stockpile show`, `/stockpile list` and `/stockpile delete` do what they say.
- `/assets guide` posts a how-to message in the channel (or in `channel`) and pins it. Only members who can manage the server can run it, and the bot needs View Channel, Send Messages and Pin Messages there.

From a fit, the hull, modules, drones and cargo count. A charge loaded in a module counts once.

When the EVE login of a character stops working (password change, app removed from the account, character moved), the bot sends its owner a direct message, and says so if no working character is left. It notices this when a command needs the login, and otherwise checks every six hours. The message only arrives if the owner shares a server with the bot and allows direct messages from it.

ESI updates asset lists once an hour, so the bot reads them at most that often. What you bought five minutes ago can still show as missing.

## Setup

### 1. Discord application

1. Create an application at https://discord.com/developers/applications and add a bot to it.
2. Copy the token. No privileged intents are needed.
3. Invite it with the scopes `bot` and `applications.commands`. It needs no permissions, except in a channel where it should post the guide.

### 2. EVE application

1. Create an application at https://developers.eveonline.com.
2. Set the callback URL to your public URL plus `/callback`, for example `https://assets.example.com/callback`.
3. Add the scopes `esi-assets.read_assets.v1`, `esi-assets.read_corporation_assets.v1`, `esi-universe.read_structures.v1` and `esi-search.search_structures.v1`.
4. Copy the client id and the secret.

### 3. Run it

| Variable | Required | Description |
| --- | --- | --- |
| `DISCORD_TOKEN` | yes | Bot token |
| `EVE_CLIENT_ID` | yes | EVE application client id |
| `EVE_CLIENT_SECRET` | yes | EVE application secret |
| `PUBLIC_URL` | yes | Public URL of the service, for example `https://assets.example.com` |
| `CONTACT` | yes | How CCP can reach you, sent in the ESI user agent: `eve:Character Name`, `discord:username` or an email |
| `BIND_ADDR` | no | Default `0.0.0.0:8080` |
| `DATABASE_PATH` | no | Default `/data/asset-bot.db` in the image, `asset-bot.db` otherwise |

With Docker:

```sh
cp .env.example .env   # fill it in
docker build -t asset-bot .
docker run -d --env-file .env -p 8080:8080 -v asset-bot-data:/data asset-bot
```

The volume holds the SQLite database with the EVE refresh tokens, so keep it private.

Locally:

```sh
cp .env.example .env   # fill it in
cargo run
```

EVE has to reach the callback URL from the player's browser only, so for local testing `PUBLIC_URL=http://localhost:8080` works if the EVE application has `http://localhost:8080/callback` as callback URL.
