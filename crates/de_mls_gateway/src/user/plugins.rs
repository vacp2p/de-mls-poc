//! User-level plugin bundle.
//!
//! [`UserPlugins`] holds all User-level plugin state: the per-conversation
//! factory, the consensus backend, and the default configs cloned into new
//! conversations. Grouping these here keeps the `User` definition surfacing
//! registry + transport at top level.

use std::time::Duration;

use de_mls::{ConsensusPlugin, ConversationConfig, ScoringConfig};

use crate::mls::DefaultConversationPluginsFactory;

/// Bundle of all User-level plugin state. One factory plus its seed
/// configs, owned outright.
pub struct UserPlugins<P: ConsensusPlugin> {
    /// Builds per-conversation plug-in instances (scoring), supplies the
    /// OpenMLS provider + credential the library seeds the MLS service from,
    /// and mints key packages for joiners.
    pub conversation_plugins: DefaultConversationPluginsFactory,
    /// Consensus backend handed by reference into every
    /// [`Conversation::create`](de_mls::Conversation::create) /
    /// [`Conversation::join`](de_mls::Conversation::join). One instance backs
    /// all conversations — its storage is shared, scope-keyed.
    pub consensus: P,
    /// Seed config copied into newly-created `Conversation`s. The steward-list
    /// config rides inside as [`ConversationConfig::steward_list`].
    pub default_conversation_config: ConversationConfig,
    /// Seed config for the per-conversation peer-scoring plug-in.
    pub default_scoring_config: ScoringConfig,
    /// The app's commit-inactivity delay (RFC §Inactivity Timer #1) — how long
    /// the epoch steward may sit on approved work before the liveness policy
    /// drives `commit_now`. de-mls no longer owns this liveness timing, so the
    /// gateway carries it; a backup's commit takeover window derives from it
    /// plus `recovery_takeover`.
    pub commit_inactivity: Duration,
    /// The app's silent-steward window (RFC §Inactivity Timer #3) — the short ~Δ
    /// delay a backup waits before covering a silent steward's in-epoch work:
    /// proposing a granted update, re-sending a sync (work the primary should
    /// have done immediately). Much smaller than `commit_inactivity`: the work is
    /// already visible to all, so there's no commit cycle to wait out.
    pub silent_steward_window: Duration,
    /// The app's recovery-takeover window — the extra wait a backup adds before
    /// forcing a commit round for a silent primary. de-mls owns reelection and
    /// its round timing now; this is purely the app's liveness delay.
    pub recovery_takeover: Duration,
}
