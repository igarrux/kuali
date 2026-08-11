//! Kuali's Discord integration.
//!
//! It follows a configured user into voice channels and extracts audio already
//! separated by participant.
//!
//! Separate streams eliminate diarization. Discord sends one RTP stream per
//! participant, and `SpeakingStateUpdate` identifies each owner, reducing
//! speaker attribution to a `HashMap` lookup.

mod audit;
pub mod bot;
pub mod identity;
mod installation;
pub mod receiver;
mod speech;

pub use bot::{start, DiscordError, DiscordHandle, DiscordSummaryState};
pub use installation::{installation_url, DiscordInstallationError};
// Source vocabulary lives in `kuali-core` so Meet can emit it without depending
// on Discord. Re-export it here to preserve existing consumers.
pub use identity::MemberResolver;
pub use kuali_core::{CallInfo, VoiceEvent};
pub use receiver::{SsrcMap, VoiceReceiver};
