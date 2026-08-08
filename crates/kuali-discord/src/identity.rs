//! Resolves SSRCs into participant identities.
//!
//! Audio arrives with an RTP stream number. This module resolves it to a human
//! identity for readable transcripts and assigned action items.
//!
//! Only speakers are resolved, avoiding a Discord API request for silent members.

use std::collections::HashSet;
use std::sync::Arc;

use kuali_core::{color_for, DiscordUserId, Speaker};
use parking_lot::Mutex;
use serenity::all::{GuildId, Http, UserId};
use tokio::sync::mpsc::UnboundedSender;

use kuali_core::VoiceEvent;

pub struct MemberResolver {
    http: Arc<Http>,
    guild_id: GuildId,
    tx: UnboundedSender<VoiceEvent>,
    /// Already-resolved users, preventing duplicate lookups.
    seen: Mutex<HashSet<DiscordUserId>>,
}

impl MemberResolver {
    pub fn new(http: Arc<Http>, guild_id: GuildId, tx: UnboundedSender<VoiceEvent>) -> Self {
        Self {
            http,
            guild_id,
            tx,
            seen: Mutex::new(HashSet::new()),
        }
    }

    /// Resolves a user on first sight without blocking. The request runs in a
    /// separate task because 20 ms audio delivery cannot wait for HTTP.
    pub fn resolve(self: &Arc<Self>, user_id: DiscordUserId) {
        if !self.seen.lock().insert(user_id) {
            return;
        }

        let this = Arc::clone(self);
        tokio::spawn(async move {
            let speaker = this.fetch(user_id).await;
            let _ = this.tx.send(VoiceEvent::ParticipantPresent(speaker));
        });
    }

    async fn fetch(&self, user_id: DiscordUserId) -> Speaker {
        match self.guild_id.member(&self.http, UserId::new(user_id)).await {
            Ok(member) => Speaker {
                user_id,
                source_id: None,
                audio_kind: None,
                // `display_name` already applies the correct precedence: guild
                // nickname, global name, then account handle.
                display_name: member.display_name().to_string(),
                username: member.user.name.clone(),
                avatar_url: Some(member.user.face()),
                color: color_for(user_id).to_string(),
                is_bot: member.user.bot,
            },
            // If permissions, rate limits, or a departed user prevent lookup,
            // keep a placeholder. Missing a name is preferable to losing speech.
            Err(_) => Speaker::unknown(user_id),
        }
    }

    /// Forgets a user so a changed nickname can be resolved after rejoining.
    pub fn forget(&self, user_id: DiscordUserId) {
        self.seen.lock().remove(&user_id);
    }
}

#[cfg(test)]
mod tests {
    use kuali_core::Speaker;

    #[test]
    fn an_unresolvable_user_still_gets_a_usable_speaker() {
        let speaker = Speaker::unknown(4242);
        assert_eq!(speaker.user_id, 4242);
        assert!(speaker.display_name.contains("4242"));
        assert!(!speaker.color.is_empty());
        assert!(!speaker.is_bot);
    }
}
