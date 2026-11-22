//! The Discord side: the `/assets` and `/stockpile` slash commands.

use std::fmt::Write as _;
use std::sync::Arc;

use anyhow::{Context as _, anyhow};
use serenity::all::{
    ActionRowComponent, ButtonStyle, Command, CommandDataOption, CommandDataOptionValue,
    CommandInteraction, CommandOptionType, ComponentInteraction, Context, CreateActionRow,
    CreateAllowedMentions, CreateAttachment, CreateAutocompleteResponse, CreateButton,
    CreateCommand, CreateCommandOption, CreateInputText, CreateInteractionResponse,
    CreateInteractionResponseMessage, CreateMessage, CreateModal, EditInteractionResponse,
    EventHandler, GuildId, Http, InputTextStyle, Interaction, Member, ModalInteraction, Ready,
    UserId,
};
use serenity::async_trait;
use tracing::{error, info, warn};

use crate::app::{App, LoginScopes};
use crate::assets;
use crate::esi::{EsiError, MIN_SEARCH_LEN, Owner};
use crate::parse::{self, Line, Part};
use crate::shopping::{self, FitPlan, Format};
use crate::sso::RefreshError;
use crate::store::{Character, Fit, Import, Item, NewFit, NewStockpile, Source, Stockpile};

/// Discord takes 2000 characters per message. This leaves room for the cut mark.
const MESSAGE_LIMIT: usize = 1900;

/// Discord shows at most this many autocomplete choices.
const MAX_CHOICES: u32 = 25;

/// Discord takes at most this many characters in the name of a choice.
const MAX_CHOICE_LEN: usize = 100;

/// Longest stockpile name.
const MAX_NAME_LEN: usize = 50;

/// Largest text file `/stockpile paste` reads.
const MAX_FILE_BYTES: u32 = 512 * 1024;

/// Most lines one pasted list may have.
const MAX_LINES: usize = 2000;

/// How many search hits or unknown names are spelled out in an answer.
const MAX_LISTED: usize = 10;

/// The how-to message `/assets guide` posts and pins.
const GUIDE: &str = "\
**Asset Bot: what is missing where**
You say what you want to have at a station or structure. I compare that with your EVE assets and write the shopping list.

**1. Link your characters**
`/assets auth` gives you a login link. Use it once per character. Add `corporation:True` if I should also read your corporation's assets (needs the Director role).

**2. Make a stockpile**
`/stockpile create name:Staging location:<station or structure>`
`shared:True` lets everyone in this server check it. `container` or `hangar` narrows what counts. `ships:True` also counts what is fitted to or loaded in ships.

**3. Say what you want there**
`/stockpile paste name:Staging` opens a window. Paste item lists (`Nanite Repair Paste 500`), copies from your inventory, or EFT fits. `times:10` wants everything ten times, so one fit becomes ten ships.
`mode` decides what happens to items the stockpile wants already: add on top, keep the higher number, or replace the whole list.
`fits: As fitted ships` wants ships with the fit's modules fitted and its drones and cargo aboard, whatever they are called, instead of just the parts.
`/stockpile set` changes a single item. `/stockpile copy` gives you the whole list as text and a file, with a button to edit it in place.

**4. Get the shopping list**
`/stockpile check` shows what is missing. The buttons under the answer give you the list for the multibuy window and in other formats.

Also: `/stockpile list`, `/stockpile show`, `/stockpile edit`, `/stockpile delete`, `/assets characters`, `/assets unlink`.
EVE updates asset lists once an hour, so something you just bought can still show as missing.";

/// Most characters Discord takes in the text field of a window.
const MAX_PASTE_CHARS: usize = 4000;

/// Id of the text field in the paste window.
const PASTE_FIELD: &str = "items";

/// Why a command did not do what was asked.
enum Failure {
    /// The user can fix it. The text is shown to them.
    Say(String),
    /// Something broke. It gets logged and the user sees a generic answer.
    Internal(anyhow::Error),
}

type Outcome<T> = Result<T, Failure>;

fn say<T>(text: impl Into<String>) -> Outcome<T> {
    Err(Failure::Say(text.into()))
}

impl From<anyhow::Error> for Failure {
    fn from(error: anyhow::Error) -> Self {
        Self::Internal(error)
    }
}

impl From<EsiError> for Failure {
    fn from(error: EsiError) -> Self {
        match error {
            EsiError::Unauthorized => Self::Say(
                "ESI refused the request. Link the character again with `/assets auth`.".to_owned(),
            ),
            EsiError::Throttled { .. } => Self::Say(
                "ESI is rate limiting me right now. Try again in a few minutes.".to_owned(),
            ),
            EsiError::Other(error) => Self::Internal(error),
        }
    }
}

impl From<RefreshError> for Failure {
    fn from(error: RefreshError) -> Self {
        match error {
            RefreshError::Revoked => Self::Say(
                "The EVE login of your character no longer works. Link it again with `/assets auth`."
                    .to_owned(),
            ),
            RefreshError::Other(error) => Self::Internal(error),
        }
    }
}

pub struct Handler {
    app: Arc<App>,
    /// Fetches files attached to commands. Kept apart from the ESI client so
    /// Discord's CDN does not get the ESI user agent.
    downloads: reqwest::Client,
}

impl Handler {
    pub fn new(app: Arc<App>) -> Self {
        Self {
            app,
            downloads: reqwest::Client::new(),
        }
    }

    async fn command(
        &self,
        http: &Http,
        command: &CommandInteraction,
    ) -> Outcome<EditInteractionResponse> {
        match (command.data.name.as_str(), subcommand_name(command)) {
            ("assets", "guide") => self.guide(http, command).await,
            ("stockpile", "edit") => self.edit(command).await,
            ("stockpile", "copy") => self.copy(command).await,
            ("stockpile", "fit") => self.fit(command).await,
            ("assets", "auth") => self.auth(command),
            ("assets", "characters") => self.characters(command.user.id).await,
            ("assets", "unlink") => self.unlink(command).await,
            ("stockpile", "create") => self.create(command).await,
            ("stockpile", "list") => self.list(command).await,
            ("stockpile", "show") => self.show(command).await,
            ("stockpile", "paste") => self.paste_file(command).await,
            ("stockpile", "set") => self.set(command).await,
            ("stockpile", "check") => self.check(command).await,
            ("stockpile", "delete") => self.delete(command).await,
            (group, name) => say(format!("Unknown command {group} {name}.")),
        }
    }

    // Characters

    fn auth(&self, command: &CommandInteraction) -> Outcome<EditInteractionResponse> {
        let scopes = if flag(command, "corporation") {
            LoginScopes::AssetsAndCorporation
        } else {
            LoginScopes::Assets
        };
        let state = self.app.begin_login(command.user.id, scopes);
        let url = self.app.login_url(&state, scopes)?;
        let button = CreateButton::new_link(url).label("Log in with EVE Online");
        Ok(reply(
            "Log in with the character you want to add. You can use the same link again for each further character, until it has been unused for 15 minutes.",
        )
        .components(vec![CreateActionRow::Buttons(vec![button])]))
    }

    async fn characters(&self, user: UserId) -> Outcome<EditInteractionResponse> {
        let characters = self.app.store.characters_of(user).await?;
        if characters.is_empty() {
            return say("You have no characters linked. Add one with `/assets auth`.");
        }
        let mut text = "Your characters:".to_owned();
        for character in &characters {
            let note = if character.login_broken() {
                " (the EVE login stopped working, link it again)".to_owned()
            } else if character.corp_assets {
                let corporation = self.app.display_name(character.corporation_id).await?;
                format!(" (also reads the assets of {corporation})")
            } else {
                String::new()
            };
            let _ = write!(text, "\n- {}{note}", character.name);
        }
        Ok(reply(text))
    }

    async fn unlink(&self, command: &CommandInteraction) -> Outcome<EditInteractionResponse> {
        let name = text(command, "character")?;
        if self
            .app
            .store
            .delete_character(command.user.id, name)
            .await?
        {
            Ok(reply(format!(
                "Removed {name}. Stockpiles that read its assets no longer count them."
            )))
        } else {
            say(format!("You have no linked character named {name}."))
        }
    }

    /// Posts the how-to message in a channel and pins it.
    async fn guide(
        &self,
        http: &Http,
        command: &CommandInteraction,
    ) -> Outcome<EditInteractionResponse> {
        if command.guild_id.is_none() {
            return say("Run this in a server.");
        }
        if !may_manage(command.member.as_deref()) {
            return say("Only members who can manage the server can post the guide.");
        }
        let channel = option(command, "channel")
            .and_then(CommandDataOptionValue::as_channel_id)
            .unwrap_or(command.channel_id);

        let message = CreateMessage::new().content(GUIDE);
        let Ok(posted) = channel.send_message(http, message).await else {
            return say(format!(
                "I cannot post in <#{channel}>. Give me View Channel and Send Messages there."
            ));
        };
        if let Err(error) = posted.pin(http).await {
            warn!("could not pin the guide in {channel}: {error}");
            return say(format!(
                "I posted the guide in <#{channel}> but could not pin it. Give me Pin Messages there (Manage Messages on older servers) and pin it, or run this again."
            ));
        }
        Ok(reply(format!(
            "Posted and pinned the guide in <#{channel}>."
        )))
    }

    // Stockpiles

    async fn edit(&self, command: &CommandInteraction) -> Outcome<EditInteractionResponse> {
        let stockpile = self.stockpile(command).await?;
        self.check_may_edit(&stockpile, command.user.id, command.member.as_deref())?;
        let Some(count_ships) = option(command, "ships").and_then(CommandDataOptionValue::as_bool)
        else {
            return say("Nothing to change. Set `ships`.");
        };
        self.app
            .store
            .set_count_ships(stockpile.id, count_ships)
            .await?;
        Ok(reply(if count_ships {
            format!(
                "**{}** now also counts what is fitted to or loaded in ships.",
                stockpile.name
            )
        } else {
            format!(
                "**{}** no longer counts what is fitted to or loaded in ships. Hulls still count.",
                stockpile.name
            )
        }))
    }

    /// Hands out what a stockpile wants as text to edit: in the answer, as a
    /// file, and behind a button that opens it for editing right in Discord.
    async fn copy(&self, command: &CommandInteraction) -> Outcome<EditInteractionResponse> {
        let stockpile = self.stockpile(command).await?;
        let items = self.app.store.items(stockpile.id).await?;
        let fits = self.app.store.fits(stockpile.id).await?;
        let header = self.describe(&stockpile).await?;
        if items.is_empty() && fits.is_empty() {
            return Ok(reply(format!("{header} wants nothing yet.")));
        }
        let list = wanted_text(&items, &fits);
        let may_edit = self
            .check_may_edit(&stockpile, command.user.id, command.member.as_deref())
            .is_ok();

        let mut answer = format!("{header} wants {}.", wanted_summary(&items, &fits));
        let block = format!("\n```\n{list}```");
        if answer.chars().count() + block.chars().count() <= MESSAGE_LIMIT - 300 {
            answer.push_str(&block);
        }
        if may_edit {
            answer.push_str(
                "\nTo change it, edit the list and send it back with `/stockpile paste` and `mode: Replace the whole list`, pasted or as the file.",
            );
            if !fits.is_empty() {
                answer.push_str(" Add `fits: As fitted ships` so the fits stay fits.");
            }
        }

        let file_name = format!("{}.txt", file_stem(&stockpile.name));
        let mut response = reply(answer).new_attachment(CreateAttachment::bytes(
            list.clone().into_bytes(),
            file_name,
        ));
        if may_edit && list.chars().count() <= MAX_PASTE_CHARS {
            let button = CreateButton::new(format!("edit:{}", stockpile.id))
                .label("Edit here")
                .style(ButtonStyle::Primary);
            response = response.components(vec![CreateActionRow::Buttons(vec![button])]);
        }
        Ok(response)
    }

    /// Sets how many ships of a fit a stockpile wants.
    async fn fit(&self, command: &CommandInteraction) -> Outcome<EditInteractionResponse> {
        let stockpile = self.stockpile(command).await?;
        self.check_may_edit(&stockpile, command.user.id, command.member.as_deref())?;
        let name = text(command, "fit")?;
        let quantity =
            integer(command, "quantity").context("the fit command came without a quantity")?;
        if !self
            .app
            .store
            .set_fit_quantity(stockpile.id, name, quantity)
            .await?
        {
            return say(format!(
                "**{}** has no fit named {name}. Add fits with `/stockpile paste` and `fits: As fitted ships`.",
                stockpile.name
            ));
        }
        Ok(reply(if quantity > 0 {
            format!(
                "**{}** wants {quantity} ships of {name} now.",
                stockpile.name
            )
        } else {
            format!("**{}** no longer wants {name}.", stockpile.name)
        }))
    }

    /// The Edit button under a copied list was pressed: opens the paste
    /// window filled with what the stockpile wants now. Sending it replaces
    /// the list. This has to be the first answer to the click.
    async fn edit_prompt(
        &self,
        component: &ComponentInteraction,
    ) -> Outcome<CreateInteractionResponse> {
        let id: i64 = component
            .data
            .custom_id
            .strip_prefix("edit:")
            .and_then(|id| id.parse().ok())
            .context("reading the stockpile id of an edit button")?;
        let visible = self.visible(component.user.id, component.guild_id).await?;
        let Some(stockpile) = visible.into_iter().find(|s| s.id == id) else {
            return say("That stockpile is gone.");
        };
        self.check_may_edit(&stockpile, component.user.id, component.member.as_ref())?;
        let fits = self.app.store.fits(id).await?;
        let list = wanted_text(&self.app.store.items(id).await?, &fits);
        if list.chars().count() > MAX_PASTE_CHARS {
            return say("The list has grown too long to edit here. Use the file instead.");
        }
        let job = PasteJob {
            times: 1,
            import: Import::Replace,
            fits: if fits.is_empty() {
                FitsAs::Parts
            } else {
                FitsAs::Ships
            },
        };
        Ok(paste_window(&stockpile, job, Some(list)))
    }

    async fn create(&self, command: &CommandInteraction) -> Outcome<EditInteractionResponse> {
        let user = command.user.id;
        let name = text(command, "name")?;
        if name.is_empty() || name.starts_with('#') || name.chars().count() > MAX_NAME_LEN {
            return say(format!(
                "A stockpile name has 1 to {MAX_NAME_LEN} characters and does not start with #."
            ));
        }
        let characters = self.app.store.characters_of(user).await?;
        if characters.is_empty() {
            return say("Link a character first with `/assets auth`.");
        }

        let source = source(command, &characters)?;
        let division = match integer(command, "hangar") {
            None => None,
            Some(_) if !matches!(source, Source::Corporation(_)) => {
                return say("`hangar` only works together with corporation assets.");
            }
            Some(hangar) => Some(u8::try_from(hangar).context("hangar out of range")?),
        };
        let guild_id = if flag(command, "shared") {
            match command.guild_id {
                Some(guild_id) => Some(guild_id),
                None => return say("A shared stockpile belongs to a server. Run this in one."),
            }
        } else {
            None
        };
        let container = optional_text(command, "container");
        let location_id = self.location(command, &characters).await?;

        let new = NewStockpile {
            name,
            created_by: user,
            guild_id,
            source,
            location_id,
            container,
            division,
            count_ships: flag(command, "ships"),
        };
        let Some(id) = self.app.store.create_stockpile(&new).await? else {
            return say(format!("There is already a stockpile named {name}."));
        };
        let stockpile = self
            .app
            .store
            .stockpile(id)
            .await?
            .context("the new stockpile is gone")?;
        info!("{user} created stockpile {id}");

        let mut answer = format!(
            "Created {}. Say what you want there with `/stockpile paste` or `/stockpile set`.",
            self.describe(&stockpile).await?
        );
        if let Some(container) = container {
            let sources = self.app.sources(&stockpile).await?;
            if self
                .app
                .store
                .stock(&sources.owners, &stockpile)
                .await?
                .is_empty()
            {
                let _ = write!(
                    answer,
                    "\nI see nothing inside a container named {container} there right now. The name has to match the one in game."
                );
            }
        }
        Ok(reply(answer))
    }

    /// The location a stockpile is created at: picked from the suggestions,
    /// or found by name among the user's assets or through ESI's search.
    async fn location(
        &self,
        command: &CommandInteraction,
        characters: &[Character],
    ) -> Outcome<i64> {
        let wanted = text(command, "location")?;
        if let Some(id) = wanted.strip_prefix('#').and_then(|id| id.parse().ok()) {
            return Ok(id);
        }

        let wanted_lower = wanted.to_lowercase();
        let known = self.app.store.locations(&owners_of(characters)).await?;
        let exact = known.iter().find(|(_, name)| {
            name.as_ref()
                .is_some_and(|n| n.to_lowercase() == wanted_lower)
        });
        if let Some(&(id, _)) = exact {
            return Ok(id);
        }

        if wanted.chars().count() < MIN_SEARCH_LEN {
            return say(format!(
                "Give me at least {MIN_SEARCH_LEN} characters of the station or structure name."
            ));
        }
        let Some(reader) = characters.iter().find(|c| !c.login_broken()) else {
            return say(
                "None of your EVE logins works any more. Link a character with `/assets auth`.",
            );
        };
        let token = self.app.access_token(reader).await?;
        let ids = self
            .app
            .esi
            .search_locations(reader.character_id, &token, wanted)
            .await?;

        let mut found = Vec::new();
        for &id in ids.iter().take(MAX_LISTED) {
            if let Some(name) = self.location_name(id, &token).await? {
                found.push((id, name));
            }
        }
        if let Some(&(id, _)) = found
            .iter()
            .find(|(_, name)| name.to_lowercase() == wanted_lower)
        {
            return Ok(id);
        }
        match found.as_slice() {
            [] => say(format!(
                "I found no station or structure named {wanted}. Structures only show up if {} may dock there.",
                reader.name
            )),
            [(id, _)] if ids.len() == 1 => Ok(*id),
            _ => {
                let names: Vec<String> =
                    found.iter().map(|(_, name)| format!("- {name}")).collect();
                let more = match ids.len().saturating_sub(found.len()) {
                    0 => String::new(),
                    more => format!("\nand {more} more"),
                };
                say(format!(
                    "Several places match {wanted}. Give me the full name of one:\n{}{more}",
                    names.join("\n")
                ))
            }
        }
    }

    /// The name of a station or structure, from the store or from ESI.
    async fn location_name(&self, id: i64, token: &str) -> Outcome<Option<String>> {
        if let Some(name) = self.app.store.name(id).await? {
            return Ok(Some(name));
        }
        let name = if assets::has_public_name(id) {
            self.app
                .esi
                .names(&[id])
                .await
                .map(|mut names| names.remove(&id))
        } else {
            self.app.esi.structure_name(id, token).await.map(Some)
        };
        match name {
            Ok(Some(name)) => {
                self.app
                    .store
                    .set_name(id, Some(&name), crate::app::now())
                    .await?;
                Ok(Some(name))
            }
            Ok(None) => Ok(None),
            Err(EsiError::Throttled { retry_after }) => {
                Err(EsiError::Throttled { retry_after }.into())
            }
            Err(error) => {
                warn!("could not name location {id}: {error}");
                Ok(None)
            }
        }
    }

    async fn list(&self, command: &CommandInteraction) -> Outcome<EditInteractionResponse> {
        let stockpiles = self.visible(command.user.id, command.guild_id).await?;
        if stockpiles.is_empty() {
            return say("There are no stockpiles yet. Make one with `/stockpile create`.");
        }
        let mut answer = String::new();
        for stockpile in &stockpiles {
            let wanted = self.app.store.items(stockpile.id).await?.len();
            let _ = writeln!(
                answer,
                "- {}, {wanted} item types, counts {}",
                self.describe(stockpile).await?,
                self.describe_source(stockpile).await?
            );
        }
        Ok(reply(answer))
    }

    async fn show(&self, command: &CommandInteraction) -> Outcome<EditInteractionResponse> {
        let stockpile = self.stockpile(command).await?;
        let items = self.app.store.items(stockpile.id).await?;
        let fits = self.app.store.fits(stockpile.id).await?;
        let header = self.describe(&stockpile).await?;
        if items.is_empty() && fits.is_empty() {
            return Ok(reply(format!("{header} wants nothing yet.")));
        }
        Ok(listing(
            &[Section {
                header: format!("{header} wants {}:", wanted_summary(&items, &fits)),
                list: wanted_text(&items, &fits),
            }],
            "stockpile.txt",
            "",
        ))
    }

    /// `/stockpile paste` without a file: opens the window to paste into.
    /// This has to be the first answer to the command, so nothing slow may
    /// happen before it.
    async fn paste_prompt(
        &self,
        command: &CommandInteraction,
    ) -> Outcome<CreateInteractionResponse> {
        let stockpile = self.stockpile(command).await?;
        self.check_may_edit(&stockpile, command.user.id, command.member.as_deref())?;
        Ok(paste_window(&stockpile, paste_job(command), None))
    }

    /// The paste window was sent.
    async fn pasted(&self, modal: &ModalInteraction) -> Outcome<EditInteractionResponse> {
        let mut parts = modal.data.custom_id.split(':');
        let (Some("paste"), Some(id), Some(times), Some(mode), fits) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return Err(anyhow!("unknown window {}", modal.data.custom_id).into());
        };
        let id: i64 = id.parse().context("reading the stockpile id")?;
        let times: i64 = times.parse().context("reading the multiplier")?;
        let job = PasteJob {
            times,
            import: match mode {
                "h" => Import::KeepHigher,
                "r" => Import::Replace,
                _ => Import::Add,
            },
            fits: if fits == Some("s") {
                FitsAs::Ships
            } else {
                FitsAs::Parts
            },
        };

        let visible = self.visible(modal.user.id, modal.guild_id).await?;
        let Some(stockpile) = visible.into_iter().find(|s| s.id == id) else {
            return say("That stockpile is gone.");
        };
        self.check_may_edit(&stockpile, modal.user.id, modal.member.as_ref())?;

        let pasted = modal
            .data
            .components
            .iter()
            .flat_map(|row| &row.components)
            .find_map(|component| match component {
                ActionRowComponent::InputText(input) if input.custom_id == PASTE_FIELD => {
                    input.value.as_deref()
                }
                _ => None,
            })
            .unwrap_or_default();
        let answer = self.import(&stockpile, pasted, job).await?;
        Ok(reply(answer))
    }

    /// `/stockpile paste` with a text file attached.
    async fn paste_file(&self, command: &CommandInteraction) -> Outcome<EditInteractionResponse> {
        let stockpile = self.stockpile(command).await?;
        self.check_may_edit(&stockpile, command.user.id, command.member.as_deref())?;
        let attachment = option(command, "file")
            .and_then(CommandDataOptionValue::as_attachment_id)
            .and_then(|id| command.data.resolved.attachments.get(&id))
            .context("the paste command came without its file")?;
        if attachment.size > MAX_FILE_BYTES {
            return say(format!(
                "That file is too big. I read up to {} KB.",
                MAX_FILE_BYTES / 1024
            ));
        }
        let bytes = self
            .downloads
            .get(&attachment.url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .context("downloading the attached file")?
            .bytes()
            .await
            .context("reading the attached file")?;
        let Ok(pasted) = std::str::from_utf8(&bytes) else {
            return say("That file is not plain text.");
        };

        let answer = self.import(&stockpile, pasted, paste_job(command)).await?;
        Ok(reply(answer))
    }

    /// Puts a pasted list into a stockpile and says what happened.
    async fn import(&self, stockpile: &Stockpile, pasted: &str, job: PasteJob) -> Outcome<String> {
        let pasted = parse::parse_fits(pasted);
        let line_count = pasted.loose.len()
            + pasted
                .fits
                .iter()
                .map(|fit| fit.lines.len() + 1)
                .sum::<usize>();
        if line_count == 0 {
            return say("I found no items in that.");
        }
        if line_count > MAX_LINES {
            return say(format!(
                "That is too long. I take up to {MAX_LINES} lines at once."
            ));
        }
        let times = job.times.max(1);
        let verb = match job.import {
            Import::Add => "Added",
            Import::KeepHigher => "Merged in",
            Import::Replace => "Replaced the list with",
        };

        if job.fits == FitsAs::Parts || pasted.fits.is_empty() {
            let resolved = self.app.resolve(&pasted.into_lines(), times).await?;
            let skipped = skipped_note(&resolved.unknown);
            if resolved.items.is_empty() {
                return say(format!("Nothing changed.{skipped}"));
            }
            self.app
                .store
                .import_items(stockpile.id, &resolved.items, job.import)
                .await?;
            let wanted = self.app.store.items(stockpile.id).await?.len();
            return Ok(format!(
                "{verb} {} item types. **{}** wants {wanted} item types now. `/stockpile check` shows what is missing.{skipped}",
                resolved.items.len(),
                stockpile.name
            ));
        }

        // Everything is looked up before anything is stored, so a failed
        // lookup cannot leave a half replaced list behind.
        let mut unknown = Vec::new();
        let loose = self.app.resolve(&pasted.loose, times).await?;
        unknown.extend(loose.unknown);
        let mut fits = Vec::new();
        for fit in &pasted.fits {
            let hull = Line {
                name: fit.hull.clone(),
                quantity: 1,
                whole: None,
            };
            let Some(&(hull_type_id, _)) = self.app.resolve(&[hull], 1).await?.items.first() else {
                unknown.push(format!("{} (a hull, so I skipped its whole fit)", fit.hull));
                continue;
            };
            let lines = |part: Part| -> Vec<Line> {
                fit.lines
                    .iter()
                    .filter(|line| line.part == part)
                    .map(|line| line.line.clone())
                    .collect()
            };
            let fitted = self.app.resolve(&lines(Part::Fitted), 1).await?;
            let carried = self.app.resolve(&lines(Part::Carried), 1).await?;
            let loaded = self.app.resolve(&lines(Part::Loaded), 1).await?;
            unknown.extend(fitted.unknown);
            unknown.extend(carried.unknown);
            unknown.extend(loaded.unknown);
            let name = if fit.name.is_empty() {
                &fit.hull
            } else {
                &fit.name
            };
            fits.push((
                cut(name, MAX_NAME_LEN),
                hull_type_id,
                fit.count.saturating_mul(times),
                fitted.items,
                carried.items,
                loaded.items,
            ));
        }
        unknown.dedup();
        let skipped = skipped_note(&unknown);
        if fits.is_empty() && loose.items.is_empty() {
            return say(format!("Nothing changed.{skipped}"));
        }

        let store = &self.app.store;
        store
            .import_items(stockpile.id, &loose.items, job.import)
            .await?;
        for (name, hull_type_id, quantity, fitted, carried, loaded) in &fits {
            let fit = NewFit {
                name,
                hull_type_id: *hull_type_id,
                quantity: *quantity,
                fitted,
                carried,
                loaded,
            };
            // Replacing has emptied the stockpile already.
            let import = match job.import {
                Import::Replace => Import::Add,
                other => other,
            };
            store.import_fit(stockpile.id, &fit, import).await?;
        }
        let items = store.items(stockpile.id).await?;
        let wanted_fits = store.fits(stockpile.id).await?;
        Ok(format!(
            "{verb} {} fits and {} item types. **{}** wants {} now. A ship counts for a fit when it is that hull with the fit's modules fitted and its drones and cargo aboard, whatever it is called. Loaded ammunition is not checked. `/stockpile check` shows what is missing.{skipped}",
            fits.len(),
            loose.items.len(),
            stockpile.name,
            wanted_summary(&items, &wanted_fits)
        ))
    }

    async fn set(&self, command: &CommandInteraction) -> Outcome<EditInteractionResponse> {
        let stockpile = self.stockpile(command).await?;
        self.check_may_edit(&stockpile, command.user.id, command.member.as_deref())?;
        let item = text(command, "item")?;
        let quantity =
            integer(command, "quantity").context("the set command came without a quantity")?;

        let line = Line {
            name: item.to_owned(),
            quantity: 1,
            whole: None,
        };
        let resolved = self.app.resolve(&[line], 1).await?;
        let Some(&(type_id, _)) = resolved.items.first() else {
            return say(format!(
                "EVE knows no item named {item}. The name has to match exactly."
            ));
        };
        self.app
            .store
            .set_item(stockpile.id, type_id, quantity)
            .await?;
        Ok(reply(if quantity > 0 {
            format!("**{}** wants {quantity} x {item} now.", stockpile.name)
        } else {
            format!("**{}** no longer wants {item}.", stockpile.name)
        }))
    }

    /// Compares stockpiles with the assets and answers with what is missing,
    /// with buttons to get the lists in other formats.
    async fn check(&self, command: &CommandInteraction) -> Outcome<EditInteractionResponse> {
        let one = option(command, "name").is_some();
        let stockpiles = if one {
            vec![self.stockpile(command).await?]
        } else {
            self.visible(command.user.id, command.guild_id).await?
        };
        if stockpiles.is_empty() {
            return say("There are no stockpiles yet. Make one with `/stockpile create`.");
        }

        let comparison = self.compare(&stockpiles, Format::Multibuy).await?;
        let response = listing(
            &comparison.sections,
            "shopping-list.txt",
            &comparison.footer,
        );
        if !comparison.anything_short {
            return Ok(response);
        }
        // The buttons name what was checked, so a click checks the same again.
        let target = if one {
            stockpiles[0].id.to_string()
        } else {
            "all".to_owned()
        };
        let buttons = Format::ALL
            .into_iter()
            .map(|format| {
                CreateButton::new(format!("list:{}:{target}", format.key()))
                    .label(format.label())
                    .style(ButtonStyle::Secondary)
            })
            .collect();
        Ok(response.components(vec![CreateActionRow::Buttons(buttons)]))
    }

    /// A format button under a check was pressed: answers the presser, and
    /// only them, with the lists in that format.
    async fn reformat(&self, component: &ComponentInteraction) -> Outcome<EditInteractionResponse> {
        let mut parts = component.data.custom_id.split(':');
        let (Some("list"), Some(format), Some(target)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(anyhow!("unknown button {}", component.data.custom_id).into());
        };
        let format = Format::from_key(format).context("reading the list format")?;

        let mut stockpiles = self.visible(component.user.id, component.guild_id).await?;
        if target != "all" {
            let id: i64 = target.parse().context("reading the stockpile id")?;
            stockpiles.retain(|stockpile| stockpile.id == id);
        }
        if stockpiles.is_empty() {
            return say("That stockpile is gone, or it is not one you can see.");
        }
        let comparison = self.compare(&stockpiles, format).await?;
        Ok(listing(
            &comparison.sections,
            "shopping-list.txt",
            &comparison.footer,
        ))
    }

    /// Compares stockpiles with the assets they count.
    async fn compare(&self, stockpiles: &[Stockpile], format: Format) -> Outcome<Comparison> {
        let mut sections = Vec::new();
        let mut problems: Vec<String> = Vec::new();
        let mut oldest: Option<i64> = None;
        let mut anything_short = false;
        for stockpile in stockpiles {
            let title = self.describe(stockpile).await?;
            let wanted = self.app.store.items(stockpile.id).await?;
            let fits = self.app.store.fits(stockpile.id).await?;
            if wanted.is_empty() && fits.is_empty() {
                sections.push(Section::plain(format!("{title} wants nothing yet.")));
                continue;
            }

            let sources = self.app.sources(stockpile).await?;
            for problem in sources.problems {
                if !problems.contains(&problem) {
                    problems.push(problem);
                }
            }
            if sources.owners.is_empty() {
                sections.push(Section::plain(format!(
                    "{title}: I have no assets to compare with."
                )));
                continue;
            }
            if let Some(at) = sources.oldest {
                oldest = Some(oldest.map_or(at, |oldest| oldest.min(at)));
            }

            let store = &self.app.store;
            let mut stock = store.stock(&sources.owners, stockpile).await?;
            let plan = if fits.is_empty() {
                FitPlan::default()
            } else {
                let hulls: Vec<i64> = fits.iter().map(|fit| fit.hull_type_id).collect();
                let ships = store
                    .docked_ships(&sources.owners, stockpile, &hulls)
                    .await?;
                shopping::plan_fits(&fits, &ships)
            };
            // Ships that stand for a fit are spoken for: neither their hulls
            // nor what is on them counts as stock for anything else.
            let mut spoken_for = vec![&plan.used_hulls];
            if stockpile.count_ships {
                spoken_for.push(&plan.used_contents);
            }
            for used in spoken_for {
                for (type_id, quantity) in used {
                    if let Some(there) = stock.get_mut(type_id) {
                        *there = (*there - quantity).max(0);
                    }
                }
            }

            let short: Vec<Item> =
                shopping::missing(&shopping::combine(&wanted, &plan.needed), &stock);
            let all_ready = plan
                .statuses
                .iter()
                .all(|status| status.ready >= status.wanted);
            let mut header = match (short.len(), all_ready) {
                (0, true) => format!("{title}: everything is there."),
                (0, false) => {
                    format!("{title}: all the parts are there, but not every ship is fitted.")
                }
                (count, _) => format!("{title}: {count} item types are short."),
            };
            for status in &plan.statuses {
                let _ = write!(
                    header,
                    "\n- {}: {} of {} ships ready",
                    status.name, status.ready, status.wanted
                );
                if status.incomplete > 0 {
                    let _ = write!(header, ", {} more need parts", status.incomplete);
                }
            }
            anything_short = anything_short || !short.is_empty();
            sections.push(Section {
                header,
                list: format.render(&short),
            });
        }

        let mut footer = String::new();
        if let Some(at) = oldest {
            let _ = writeln!(
                footer,
                "Assets as of <t:{at}:R>. ESI updates them once an hour."
            );
        }
        for problem in problems {
            let _ = writeln!(footer, "{problem}");
        }
        Ok(Comparison {
            sections,
            footer,
            anything_short,
        })
    }

    async fn delete(&self, command: &CommandInteraction) -> Outcome<EditInteractionResponse> {
        let stockpile = self.stockpile(command).await?;
        self.check_may_edit(&stockpile, command.user.id, command.member.as_deref())?;
        self.app.store.delete_stockpile(stockpile.id).await?;
        info!("{} deleted stockpile {}", command.user.id, stockpile.id);
        Ok(reply(format!("Deleted **{}**.", stockpile.name)))
    }

    /// The stockpiles a user can see here: their own and the server's.
    async fn visible(&self, user: UserId, guild: Option<GuildId>) -> Outcome<Vec<Stockpile>> {
        Ok(self.app.store.stockpiles(user, guild).await?)
    }

    /// The stockpile named in the `name` option. Suggestions send `#id`,
    /// typed text is taken as the name, personal stockpiles first.
    async fn stockpile(&self, command: &CommandInteraction) -> Outcome<Stockpile> {
        let wanted = text(command, "name")?;
        let visible = self.visible(command.user.id, command.guild_id).await?;
        let by_id = wanted
            .strip_prefix('#')
            .and_then(|id| id.parse::<i64>().ok());
        let wanted_lower = wanted.to_lowercase();
        let found = visible.into_iter().find(|stockpile| match by_id {
            Some(id) => stockpile.id == id,
            None => stockpile.name.to_lowercase() == wanted_lower,
        });
        match found {
            Some(stockpile) => Ok(stockpile),
            None => say(format!(
                "I know no stockpile named {wanted} here. `/stockpile list` shows them."
            )),
        }
    }

    /// Personal stockpiles belong to their creator. Shared ones can also be
    /// changed by members who can manage the server.
    #[expect(clippy::unused_self, reason = "reads better next to the other checks")]
    fn check_may_edit(
        &self,
        stockpile: &Stockpile,
        user: UserId,
        member: Option<&Member>,
    ) -> Outcome<()> {
        let manages = stockpile.guild_id.is_some() && may_manage(member);
        if stockpile.created_by == user || manages {
            Ok(())
        } else {
            say(format!(
                "Only <@{}> and members who can manage the server can change **{}**.",
                stockpile.created_by, stockpile.name
            ))
        }
    }

    /// Name and place of a stockpile, for answers.
    async fn describe(&self, stockpile: &Stockpile) -> Outcome<String> {
        let mut text = format!(
            "**{}** at {}",
            stockpile.name,
            self.app.display_name(stockpile.location_id).await?
        );
        if let Some(division) = stockpile.division {
            let _ = write!(text, ", hangar {division}");
        }
        if let Some(container) = &stockpile.container {
            let _ = write!(text, ", container {container}");
        }
        if stockpile.guild_id.is_some() {
            text.push_str(" (shared)");
        }
        Ok(text)
    }

    /// Whose assets a stockpile counts, for answers.
    async fn describe_source(&self, stockpile: &Stockpile) -> Outcome<String> {
        let owner = stockpile.created_by;
        let whose = match stockpile.source {
            Source::AllCharacters => format!("all characters of <@{owner}>"),
            Source::Character(id) => {
                let characters = self.app.store.characters_of(owner).await?;
                match characters.iter().find(|c| c.character_id == id) {
                    Some(character) => character.name.clone(),
                    None => "a character that is no longer linked".to_owned(),
                }
            }
            Source::Corporation(id) => {
                format!("the corporation {}", self.app.display_name(id).await?)
            }
        };
        Ok(if stockpile.count_ships {
            format!("{whose}, also what is in ships")
        } else {
            whose
        })
    }

    /// Suggests values while the user types an option.
    async fn autocomplete(&self, http: &Http, command: &CommandInteraction) {
        let Some(focused) = command.data.autocomplete() else {
            return;
        };
        let choices = self
            .choices(command, focused.name, focused.value.trim())
            .await
            .unwrap_or_else(|error| {
                warn!("could not load suggestions for {}: {error:#}", focused.name);
                Vec::new()
            });

        let typed = focused.value.trim().to_lowercase();
        let mut response = CreateAutocompleteResponse::new();
        let matches = choices
            .into_iter()
            .filter(|(label, _)| label.to_lowercase().contains(&typed))
            .take(MAX_CHOICES as usize);
        for (label, value) in matches {
            response = response.add_string_choice(cut(&label, MAX_CHOICE_LEN), value);
        }
        let response = CreateInteractionResponse::Autocomplete(response);
        if let Err(error) = command.create_response(http, response).await {
            warn!("could not answer autocomplete: {error}");
        }
    }

    /// The label and value of everything an option could be set to.
    async fn choices(
        &self,
        command: &CommandInteraction,
        option: &str,
        typed: &str,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let store = &self.app.store;
        let user = command.user.id;
        Ok(match option {
            "name" => store
                .stockpiles(user, command.guild_id)
                .await?
                .into_iter()
                .map(|stockpile| {
                    let shared = if stockpile.guild_id.is_some() {
                        " (shared)"
                    } else {
                        ""
                    };
                    (
                        format!("{}{shared}", stockpile.name),
                        format!("#{}", stockpile.id),
                    )
                })
                .collect(),
            "fit" => {
                let wanted = optional_text(command, "name").unwrap_or_default();
                let by_id = wanted
                    .strip_prefix('#')
                    .and_then(|id| id.parse::<i64>().ok());
                let stockpile = store
                    .stockpiles(user, command.guild_id)
                    .await?
                    .into_iter()
                    .find(|stockpile| match by_id {
                        Some(id) => stockpile.id == id,
                        None => stockpile.name.eq_ignore_ascii_case(wanted),
                    });
                match stockpile {
                    Some(stockpile) => store
                        .fits(stockpile.id)
                        .await?
                        .into_iter()
                        .map(|fit| (fit.name.clone(), fit.name))
                        .collect(),
                    None => Vec::new(),
                }
            }
            "character" => store
                .characters_of(user)
                .await?
                .into_iter()
                .map(|character| (character.name.clone(), character.name))
                .collect(),
            "item" => store
                .search_types(typed, MAX_CHOICES)
                .await?
                .into_iter()
                .map(|name| (name.clone(), name))
                .collect(),
            "location" => {
                let characters = store.characters_of(user).await?;
                let mut locations: Vec<(String, String)> = store
                    .locations(&owners_of(&characters))
                    .await?
                    .into_iter()
                    .filter(|&(id, _)| !assets::in_space(id))
                    .filter_map(|(id, name)| Some((name?, format!("#{id}"))))
                    .collect();
                locations.sort();
                locations
            }
            "assets" => {
                let characters = store.characters_of(user).await?;
                let mut sources = vec![("All my characters".to_owned(), "all".to_owned())];
                for character in &characters {
                    sources.push((
                        character.name.clone(),
                        format!("char:{}", character.character_id),
                    ));
                }
                for character in characters.iter().filter(|c| c.corp_assets) {
                    let id = character.corporation_id;
                    let value = format!("corp:{id}");
                    if id != 0 && !sources.iter().any(|(_, known)| *known == value) {
                        let name = self.app.display_name(id).await?;
                        sources.push((format!("Corporation: {name}"), value));
                    }
                }
                sources
            }
            _ => Vec::new(),
        })
    }
}

/// What comparing stockpiles with the assets came to.
struct Comparison {
    sections: Vec<Section>,
    /// How old the assets are and what went wrong reading them.
    footer: String,
    /// At least one stockpile is missing something.
    anything_short: bool,
}

/// Whether pasted EFT fits become fits to match ships against, or just the
/// items they are made of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FitsAs {
    Parts,
    Ships,
}

/// What to do with a pasted list.
#[derive(Debug, Clone, Copy)]
struct PasteJob {
    /// Want everything this many times.
    times: i64,
    import: Import,
    fits: FitsAs,
}

/// Reads the options of `/stockpile paste`.
fn paste_job(command: &CommandInteraction) -> PasteJob {
    PasteJob {
        times: integer(command, "times").unwrap_or(1),
        import: match optional_text(command, "mode") {
            Some("higher") => Import::KeepHigher,
            Some("replace") => Import::Replace,
            _ => Import::Add,
        },
        fits: match optional_text(command, "fits") {
            Some("ships") => FitsAs::Ships,
            _ => FitsAs::Parts,
        },
    }
}

/// What a stockpile wants, as text that `/stockpile paste` reads back: the
/// items first, then each fit with its count, its modules one per line
/// (with a loaded charge after a comma) and its drones and cargo as `Name x5`.
fn wanted_text(items: &[Item], fits: &[Fit]) -> String {
    let mut text = Format::Multibuy.render(items);
    for fit in fits {
        if !text.is_empty() {
            text.push('\n');
        }
        let _ = writeln!(text, "[{}, {}] x{}", fit.hull_name, fit.name, fit.wanted);
        // Which module a charge was loaded in is not kept, and does not
        // matter: each charge goes behind some module.
        let mut charges = fit
            .loaded
            .iter()
            .flat_map(|charge| (0..charge.quantity).map(move |_| &charge.name));
        for module in &fit.fitted {
            for _ in 0..module.quantity {
                match charges.next() {
                    Some(charge) => {
                        let _ = writeln!(text, "{}, {charge}", module.name);
                    }
                    None => {
                        let _ = writeln!(text, "{}", module.name);
                    }
                }
            }
        }
        text.push_str(&Format::Eft.render(&fit.carried));
    }
    text
}

/// How much a stockpile wants, in words.
fn wanted_summary(items: &[Item], fits: &[Fit]) -> String {
    match (items.len(), fits.len()) {
        (items, 0) => format!("{items} item types"),
        (0, fits) => format!("{fits} fits"),
        (items, fits) => format!("{fits} fits and {items} item types"),
    }
}

/// The sentence about names that were not found, or nothing.
fn skipped_note(unknown: &[String]) -> String {
    if unknown.is_empty() {
        return String::new();
    }
    let names: Vec<&str> = unknown
        .iter()
        .take(MAX_LISTED)
        .map(String::as_str)
        .collect();
    let more = match unknown.len().saturating_sub(names.len()) {
        0 => String::new(),
        more => format!(" and {more} more"),
    };
    format!(
        "\nEVE knows no item with these names, so I skipped them: {}{more}",
        names.join(", ")
    )
}

/// The window to paste items into. Its id carries what to do with the text.
/// `current` fills the field in, for editing what a stockpile wants.
fn paste_window(
    stockpile: &Stockpile,
    job: PasteJob,
    current: Option<String>,
) -> CreateInteractionResponse {
    let times = job.times;
    let fits = match job.fits {
        FitsAs::Parts => 'p',
        FitsAs::Ships => 's',
    };
    let mode = match job.import {
        Import::Add => 'a',
        Import::KeepHigher => 'h',
        Import::Replace => 'r',
    };
    let mut field =
        CreateInputText::new(InputTextStyle::Paragraph, "Items or EFT fits", PASTE_FIELD)
            .placeholder("Nanite Repair Paste 500\n[Ferox, Fleet fit]\n250mm Railgun II")
            .required(true);
    let title = if let Some(current) = current {
        field = field.value(current);
        format!("Edit {}", stockpile.name)
    } else {
        format!("Paste into {}", stockpile.name)
    };
    let modal = CreateModal::new(
        format!("paste:{}:{times}:{mode}:{fits}", stockpile.id),
        cut(&title, 45),
    )
    .components(vec![CreateActionRow::InputText(field)]);
    CreateInteractionResponse::Modal(modal)
}

/// A stockpile name as a file name: letters, digits, dashes and underscores.
fn file_stem(name: &str) -> String {
    let stem: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if stem.chars().any(char::is_alphanumeric) {
        stem
    } else {
        "stockpile".to_owned()
    }
}

/// Whether a server member can manage the server.
fn may_manage(member: Option<&Member>) -> bool {
    member
        .and_then(|member| member.permissions)
        .is_some_and(|permissions| permissions.administrator() || permissions.manage_guild())
}

/// A headline with, optionally, a list under it.
struct Section {
    header: String,
    /// Lines of `Name quantity`, or empty.
    list: String,
}

impl Section {
    fn plain(header: String) -> Self {
        Self {
            header,
            list: String::new(),
        }
    }
}

/// An answer made of sections. The lists go into code blocks, which copy
/// cleanly into the game. If that is too long for one message, they go into
/// an attached text file instead.
fn listing(sections: &[Section], file_name: &str, footer: &str) -> EditInteractionResponse {
    let mut message = String::new();
    let mut headers = String::new();
    let mut file = String::new();
    for section in sections {
        let _ = writeln!(headers, "{}", section.header);
        let _ = writeln!(message, "{}", section.header);
        if !section.list.is_empty() {
            let _ = writeln!(message, "```\n{}```", section.list);
            let _ = writeln!(
                file,
                "{}\n{}",
                section.header.replace("**", ""),
                section.list
            );
        }
    }

    if message.chars().count() + footer.chars().count() <= MESSAGE_LIMIT {
        return reply(format!("{message}{footer}"));
    }
    reply(format!(
        "{headers}The lists are in the attached file.\n{footer}"
    ))
    .new_attachment(CreateAttachment::bytes(file.into_bytes(), file_name))
}

/// A text answer that pings nobody, cut to what Discord takes.
fn reply(text: impl Into<String>) -> EditInteractionResponse {
    EditInteractionResponse::new()
        .content(cut(&text.into(), MESSAGE_LIMIT))
        .allowed_mentions(CreateAllowedMentions::new())
}

/// Cuts a text to at most `limit` characters, marking the cut.
fn cut(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut short: String = text.chars().take(limit.saturating_sub(1)).collect();
    short.push('…');
    short
}

/// Everything whose assets a user's characters can read.
fn owners_of(characters: &[Character]) -> Vec<Owner> {
    let mut owners: Vec<Owner> = characters
        .iter()
        .map(|c| Owner::Character(c.character_id))
        .collect();
    for character in characters.iter().filter(|c| c.corp_assets) {
        let owner = Owner::Corporation(character.corporation_id);
        if !owners.contains(&owner) {
            owners.push(owner);
        }
    }
    owners
}

/// Reads the `assets` option of `/stockpile create`. Suggestions send `all`,
/// `char:<id>` or `corp:<id>`; a typed character name works too.
fn source(command: &CommandInteraction, characters: &[Character]) -> Outcome<Source> {
    let Some(wanted) = optional_text(command, "assets") else {
        return Ok(Source::AllCharacters);
    };
    if wanted.eq_ignore_ascii_case("all") {
        return Ok(Source::AllCharacters);
    }
    let id_after = |prefix| {
        wanted
            .strip_prefix(prefix)
            .and_then(|id: &str| id.parse::<i64>().ok())
    };
    if let Some(id) = id_after("char:")
        && characters.iter().any(|c| c.character_id == id)
    {
        return Ok(Source::Character(id));
    }
    if let Some(id) = id_after("corp:")
        && characters
            .iter()
            .any(|c| c.corp_assets && c.corporation_id == id)
    {
        return Ok(Source::Corporation(id));
    }
    let wanted_lower = wanted.to_lowercase();
    if let Some(character) = characters
        .iter()
        .find(|c| c.name.to_lowercase() == wanted_lower)
    {
        return Ok(Source::Character(character.character_id));
    }
    say(
        "Pick one of the suggestions for `assets`: all your characters, one of them, or a corporation you linked with `/assets auth corporation:True`.",
    )
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

/// A text option, trimmed. `None` if it was left out or is blank.
fn optional_text<'a>(command: &'a CommandInteraction, name: &str) -> Option<&'a str> {
    option(command, name)
        .and_then(CommandDataOptionValue::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

/// A required text option, trimmed.
fn text<'a>(command: &'a CommandInteraction, name: &str) -> Outcome<&'a str> {
    match optional_text(command, name) {
        Some(text) => Ok(text),
        None => say(format!("`{name}` is missing.")),
    }
}

fn integer(command: &CommandInteraction, name: &str) -> Option<i64> {
    option(command, name).and_then(CommandDataOptionValue::as_i64)
}

/// A yes/no option. Left out counts as no.
fn flag(command: &CommandInteraction, name: &str) -> bool {
    option(command, name)
        .and_then(CommandDataOptionValue::as_bool)
        .unwrap_or(false)
}

fn commands() -> Vec<CreateCommand> {
    let sub = |name, description| {
        CreateCommandOption::new(CommandOptionType::SubCommand, name, description)
    };
    let string =
        |name, description| CreateCommandOption::new(CommandOptionType::String, name, description);
    let boolean =
        |name, description| CreateCommandOption::new(CommandOptionType::Boolean, name, description);
    let integer =
        |name, description| CreateCommandOption::new(CommandOptionType::Integer, name, description);
    let stockpile = |description| {
        string("name", description)
            .set_autocomplete(true)
            .required(true)
    };

    vec![
        CreateCommand::new("assets")
            .description("Your EVE characters whose assets I read")
            .add_option(
                sub("auth", "Link an EVE character so I can read its assets").add_sub_option(
                    boolean(
                        "corporation",
                        "Also read the assets of its corporation (needs the Director role)",
                    ),
                ),
            )
            .add_option(sub("characters", "List your linked EVE characters"))
            .add_option(
                sub("guide", "Post and pin a how-to message (needs Manage Server)").add_sub_option(
                    CreateCommandOption::new(
                        CommandOptionType::Channel,
                        "channel",
                        "Where to post it, default is this channel",
                    ),
                ),
            )
            .add_option(
                sub("unlink", "Remove one of your EVE characters").add_sub_option(
                    string("character", "Name of the character")
                        .set_autocomplete(true)
                        .required(true),
                ),
            ),
        CreateCommand::new("stockpile")
            .description("What you want to have where, and what is missing")
            .add_option(
                sub(
                    "create",
                    "Start a list of what you want at a station or structure",
                )
                .add_sub_option(string("name", "What to call the stockpile").required(true))
                .add_sub_option(
                    string("location", "Station or structure, by name")
                        .set_autocomplete(true)
                        .required(true),
                )
                .add_sub_option(
                    string(
                        "assets",
                        "Whose assets count: all your characters (default), one, or a corporation",
                    )
                    .set_autocomplete(true),
                )
                .add_sub_option(boolean(
                    "shared",
                    "Let everyone in this server see and check it",
                ))
                    .add_sub_option(boolean(
                        "ships",
                        "Also count what is fitted to or loaded in ships (default: only hulls and loose items)",
                    ))
                .add_sub_option(string(
                    "container",
                    "Only count what is in the container with this name",
                ))
                .add_sub_option(
                    integer("hangar", "Only count this corporation hangar, 1 to 7")
                        .min_int_value(1)
                        .max_int_value(7),
                ),
            )
            .add_option(sub("list", "List your stockpiles and the shared ones here"))
            .add_option(
                sub("show", "Show what a stockpile wants")
                    .add_sub_option(stockpile("The stockpile")),
            )
            .add_option(
                sub("paste", "Add items by pasting a list or EFT fits")
                    .add_sub_option(stockpile("The stockpile"))
                    .add_sub_option(
                        integer(
                            "times",
                            "Want everything you paste this many times, for example 10 fits",
                        )
                        .min_int_value(1)
                        .max_int_value(100_000),
                    )
                    .add_sub_option(
                        string("mode", "What happens to items the stockpile wants already")
                            .add_string_choice("Add on top: 5000 + 5000 wants 10000 (default)", "add")
                            .add_string_choice("Keep the higher number: 5000 + 5000 wants 5000", "higher")
                            .add_string_choice("Replace the whole list", "replace"),
                    )
                    .add_sub_option(
                        string("fits", "What an EFT fit in the list stands for")
                            .add_string_choice("As parts: just want the items of the fit (default)", "parts")
                            .add_string_choice("As fitted ships: want ships with the fit's modules, drones and cargo", "ships"),
                    )
                    .add_sub_option(CreateCommandOption::new(
                        CommandOptionType::Attachment,
                        "file",
                        "A text file with the list, for long ones",
                    )),
            )
            .add_option(
                sub("set", "Set how many of one item a stockpile wants")
                    .add_sub_option(stockpile("The stockpile"))
                    .add_sub_option(
                        string("item", "Exact name of the item")
                            .set_autocomplete(true)
                            .required(true),
                    )
                    .add_sub_option(
                        integer("quantity", "How many you want there, 0 to remove the item")
                            .min_int_value(0)
                            .required(true),
                    ),
            )
            .add_option(
                sub("fit", "Set how many ships of a fit a stockpile wants")
                    .add_sub_option(stockpile("The stockpile"))
                    .add_sub_option(
                        string("fit", "Name of the fit")
                            .set_autocomplete(true)
                            .required(true),
                    )
                    .add_sub_option(
                        integer("quantity", "How many ships, 0 to remove the fit")
                            .min_int_value(0)
                            .required(true),
                    ),
            )
            .add_option(
                sub(
                    "check",
                    "Compare with your assets and get the shopping list",
                )
                .add_sub_option(
                    string("name", "The stockpile, leave out to check all").set_autocomplete(true),
                )
                .add_sub_option(boolean(
                    "public",
                    "Post the answer in the channel instead of only to you",
                )),
            )
            .add_option(
                sub("copy", "Get what a stockpile wants as text or a file, to edit and send back")
                    .add_sub_option(stockpile("The stockpile")),
            )
            .add_option(
                sub("edit", "Change what a stockpile counts")
                    .add_sub_option(stockpile("The stockpile"))
                    .add_sub_option(boolean(
                        "ships",
                        "Also count what is fitted to or loaded in ships",
                    )),
            )
            .add_option(
                sub("delete", "Delete a stockpile").add_sub_option(stockpile("The stockpile")),
            ),
    ]
}

/// Turns what a command did into its answer, logging what broke.
fn answer(what: &str, outcome: Outcome<EditInteractionResponse>) -> EditInteractionResponse {
    match outcome {
        Ok(response) => response,
        Err(Failure::Say(text)) => reply(text),
        Err(Failure::Internal(error)) => {
            error!("{what} failed: {error:#}");
            reply("Something went wrong, try again in a bit.")
        }
    }
}

/// The private message for when a window could not be opened, logging what broke.
fn first_answer(what: &str, failure: Failure) -> CreateInteractionResponse {
    let text = match failure {
        Failure::Say(text) => text,
        Failure::Internal(error) => {
            error!("{what} failed: {error:#}");
            "Something went wrong, try again in a bit.".to_owned()
        }
    };
    CreateInteractionResponse::Message(
        CreateInteractionResponseMessage::new()
            .content(cut(&text, MESSAGE_LIMIT))
            .allowed_mentions(CreateAllowedMentions::new())
            .ephemeral(true),
    )
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        info!("connected to Discord as {}", ready.user.name);
        if let Err(error) = Command::set_global_commands(&ctx.http, commands()).await {
            error!("could not register the slash commands: {error}");
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let http = &ctx.http;
        match interaction {
            Interaction::Autocomplete(command) => self.autocomplete(http, &command).await,
            Interaction::Component(component) if component.data.custom_id.starts_with("edit:") => {
                let response = self
                    .edit_prompt(&component)
                    .await
                    .unwrap_or_else(|failure| first_answer("edit button", failure));
                if let Err(error) = component.create_response(http, response).await {
                    error!("could not answer the edit button: {error}");
                }
            }
            Interaction::Component(component) => {
                // Reading assets from ESI can take longer than Discord waits.
                if let Err(error) = component.defer_ephemeral(http).await {
                    error!("could not acknowledge a button: {error}");
                    return;
                }
                let response = answer("format button", self.reformat(&component).await);
                if let Err(error) = component.edit_response(http, response).await {
                    error!("could not answer a button: {error}");
                }
            }
            Interaction::Modal(modal) => {
                if let Err(error) = modal.defer_ephemeral(http).await {
                    error!("could not acknowledge the paste window: {error}");
                    return;
                }
                let response = answer("paste window", self.pasted(&modal).await);
                if let Err(error) = modal.edit_response(http, response).await {
                    error!("could not answer the paste window: {error}");
                }
            }
            Interaction::Command(command) => {
                let name = subcommand_name(&command);
                // The window to paste into can only be the first answer.
                if name == "paste" && option(&command, "file").is_none() {
                    let response = self
                        .paste_prompt(&command)
                        .await
                        .unwrap_or_else(|failure| first_answer("command paste", failure));
                    if let Err(error) = command.create_response(http, response).await {
                        error!("could not answer command paste: {error}");
                    }
                    return;
                }

                // Reading assets from ESI can take longer than Discord waits.
                let public = name == "check" && flag(&command, "public");
                let deferred = if public {
                    command.defer(http).await
                } else {
                    command.defer_ephemeral(http).await
                };
                if let Err(error) = deferred {
                    error!("could not acknowledge command {name}: {error}");
                    return;
                }
                let response = answer(name, self.command(http, &command).await);
                if let Err(error) = command.edit_response(http, response).await {
                    error!("could not answer command {name}: {error}");
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copied_text_reads_back_as_the_same_fits() {
        let item = |type_id, name: &str, quantity| Item {
            type_id,
            name: name.to_owned(),
            quantity,
        };
        let fit = Fit {
            name: "Fleet Ferox".to_owned(),
            hull_type_id: 16227,
            hull_name: "Ferox".to_owned(),
            wanted: 10,
            fitted: vec![item(3082, "250mm Railgun II", 7)],
            loaded: vec![item(230, "Antimatter Charge M", 7)],
            carried: vec![
                item(2488, "Hornet EC-300", 5),
                item(28668, "Nanite Repair Paste", 1),
            ],
        };
        let text = wanted_text(&[item(34, "Tritanium", 500)], &[fit]);
        let pasted = parse::parse_fits(&text);

        assert_eq!(pasted.loose.len(), 1);
        assert_eq!(pasted.loose[0].quantity, 500);
        let read = &pasted.fits[0];
        assert_eq!(
            (read.hull.as_str(), read.name.as_str(), read.count),
            ("Ferox", "Fleet Ferox", 10)
        );
        let count = |part| read.lines.iter().filter(|line| line.part == part).count();
        assert_eq!(count(Part::Fitted), 7, "one line per fitted module");
        assert_eq!(count(Part::Loaded), 7, "each with its charge");
        let extras: Vec<(&str, i64)> = read
            .lines
            .iter()
            .filter(|line| line.part == Part::Carried)
            .map(|line| (line.line.name.as_str(), line.line.quantity))
            .collect();
        assert_eq!(extras, [("Hornet EC-300", 5), ("Nanite Repair Paste", 1)]);
    }

    #[test]
    fn the_guide_fits_in_one_message() {
        let length = GUIDE.chars().count();
        assert!(length <= 2000, "the guide has {length} characters");
    }
}
