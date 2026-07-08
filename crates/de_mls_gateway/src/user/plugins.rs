//! User-level plugin bundle.
//!
//! [`UserPlugins`] holds all User-level plugin state: the per-conversation
//! factory, the consensus backend, and the default configs cloned into new
//! conversations. Grouping these here keeps the `User` definition surfacing
//! registry + transport at top level.

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
}
