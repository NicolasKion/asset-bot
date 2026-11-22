//! Watches the EVE logins and tells their owners on Discord when one stops working.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use serenity::all::{CreateMessage, Http, UserId};
use tracing::{info, warn};

use crate::app::App;
use crate::sso::RefreshError;
use crate::store::Character;

/// How long to wait between two rounds of trying every login. A login only
/// breaks when the player revokes it, changes their password or moves the
/// character, so there is no hurry.
const CHECK_INTERVAL: Duration = Duration::from_hours(6);

/// Pause between two logins in a round, to go easy on EVE SSO.
const BETWEEN_LOGINS: Duration = Duration::from_millis(500);

pub struct Monitor {
    app: Arc<App>,
    http: Arc<Http>,
}

impl Monitor {
    pub fn new(app: Arc<App>, http: Arc<Http>) -> Self {
        Self { app, http }
    }

    /// Runs until the process ends.
    pub async fn run(self) {
        // The first round starts right away, which also reports logins that
        // broke while the bot was down.
        let mut next_check = Instant::now();
        loop {
            if Instant::now() >= next_check {
                if let Err(error) = self.check_logins().await {
                    warn!("could not check the EVE logins: {error:#}");
                }
                // Counted from the end of the round, so rounds never pile up.
                next_check = Instant::now() + CHECK_INTERVAL;
            }
            if let Err(error) = self.report().await {
                warn!("could not report broken EVE logins: {error:#}");
            }

            let until_check = next_check.saturating_duration_since(Instant::now());
            tokio::select! {
                () = tokio::time::sleep(until_check) => {}
                // A command just ran into a broken login.
                () = self.app.login_broke.notified() => {}
            }
        }
    }

    /// Tries every login once. Getting a token marks a revoked login as broken.
    async fn check_logins(&self) -> Result<()> {
        for character in self.app.store.characters_with_login().await? {
            match self.app.access_token(&character).await {
                Ok(_) | Err(RefreshError::Revoked) => {}
                // EVE SSO being down says nothing about the login.
                Err(RefreshError::Other(error)) => {
                    warn!("could not check the login of {}: {error:#}", character.name);
                }
            }
            tokio::time::sleep(BETWEEN_LOGINS).await;
        }
        Ok(())
    }

    /// Sends each owner one message about their newly broken logins.
    async fn report(&self) -> Result<()> {
        let mut by_owner: BTreeMap<UserId, Vec<Character>> = BTreeMap::new();
        for character in self.app.store.broken_logins_to_report().await? {
            by_owner
                .entry(character.discord_user_id)
                .or_default()
                .push(character);
        }

        for (owner, broken) in by_owner {
            let characters = self.app.store.characters_of(owner).await?;
            let any_works = characters.iter().any(|c| !c.login_broken());
            let text = message(&broken, any_works);
            match self.send(owner, &text).await {
                Ok(()) => info!("told {owner} about {} broken EVE logins", broken.len()),
                // Most likely their direct messages are closed. Asking again
                // would not help, and `/stockpile check` says it too.
                Err(error) => warn!("could not message {owner}: {error:#}"),
            }
            for character in &broken {
                self.app
                    .store
                    .set_login_notified(character.character_id)
                    .await?;
            }
        }
        Ok(())
    }

    async fn send(&self, owner: UserId, text: &str) -> Result<()> {
        let channel = owner
            .create_dm_channel(&self.http)
            .await
            .context("opening the direct message channel")?;
        channel
            .send_message(&self.http, CreateMessage::new().content(text))
            .await
            .context("sending the direct message")?;
        Ok(())
    }
}

/// The direct message about logins that stopped working. `any_works` says
/// whether the owner has a character left whose login still works.
fn message(broken: &[Character], any_works: bool) -> String {
    let mut text = match broken {
        [one] => format!("The EVE login of **{}** stopped working", one.name),
        many => {
            let mut text = "The EVE logins of these characters stopped working".to_owned();
            for character in many {
                let _ = write!(text, "\n- **{}**", character.name);
            }
            text.push('\n');
            text
        }
    };
    text.push_str(if broken.len() == 1 { ", so" } else { "So" });
    text.push_str(" I can no longer read ");
    text.push_str(if broken.len() == 1 { "its" } else { "their" });
    text.push_str(" assets. That happens when you change your EVE password, remove the app from your account or move the character. Link again with `/assets auth`.");
    if !any_works {
        text.push_str(
            "\nNone of your characters has a working login now, so your stockpiles cannot be checked until you do.",
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn character(name: &str) -> Character {
        Character {
            character_id: 1,
            name: name.to_owned(),
            discord_user_id: UserId::new(1),
            refresh_token: String::new(),
            corporation_id: 0,
            corp_assets: false,
        }
    }

    #[test]
    fn one_broken_login_with_others_left() {
        let text = message(&[character("Pilot One")], true);
        assert!(text.starts_with(
            "The EVE login of **Pilot One** stopped working, so I can no longer read its assets."
        ));
        assert!(!text.contains("None of your characters"));
    }

    #[test]
    fn several_broken_logins_and_none_left() {
        let text = message(&[character("Pilot One"), character("Pilot Two")], false);
        assert!(
            text.contains(
                "\n- **Pilot One**\n- **Pilot Two**\nSo I can no longer read their assets."
            )
        );
        assert!(text.ends_with("cannot be checked until you do."));
    }
}
