//! Gateway-side MLS defaults: the concrete plug-in helper the reference
//! integrator wires into [`User`](crate::user::User).
//!
//! de-mls names no concrete MLS backend — `Conversation` takes an
//! `OpenMlsProvider` by reference on every driving call and builds the MLS
//! service from a credential + group config + provider. This module supplies
//! the integrator half: the OpenMLS reference provider (`OpenMlsRustCrypto`),
//! credential generation from a member id, key-package minting, and the
//! concrete scoring instances the `User` passes into
//! [`Conversation::create`](de_mls::Conversation::create) /
//! [`Conversation::join`](de_mls::Conversation::join).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use de_mls::{
    PeerScoringService, ScoringConfig, WallClock, default_score_deltas,
    defaults::{DefaultPeerScoring, InMemoryPeerScoreStorage},
    protos::de_mls::messages::v1::MemberInvite,
};
use openmls::{
    credentials::{BasicCredential, CredentialWithKey},
    group::MlsGroupCreateConfig,
    key_packages::KeyPackage,
    prelude::{Ciphersuite, CryptoError, tls_codec::Serialize as _},
};
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;
use prost::Message;

/// Ciphersuite the gateway pins for every conversation it creates or joins.
pub const GATEWAY_SUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;

/// OpenMLS provider the gateway runs: the reference RustCrypto backend.
pub type GatewayProvider = OpenMlsRustCrypto;

/// Production [`WallClock`]: real time from [`std::time::SystemTime`]. The
/// library owns no time source — every conversation deadline is measured
/// against the clock the integrator moves in at construction. Readings are
/// clamped so they never run backwards (the trait's one requirement) even if
/// the system clock steps back, e.g. under NTP correction.
#[derive(Debug, Default)]
pub struct SystemClock {
    last_nanos: AtomicU64,
}

impl WallClock for SystemClock {
    fn now(&self) -> Duration {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .min(u64::MAX as u128) as u64;
        let prev = self.last_nanos.fetch_max(now, Ordering::AcqRel);
        Duration::from_nanos(prev.max(now))
    }
}

/// Errors from the gateway-side MLS setup helpers (credential and key-package
/// minting). The library's own `MlsError` no longer covers these — credential
/// and key-package creation are integrator concerns.
#[derive(Debug, thiserror::Error)]
pub enum MlsSetupError {
    #[error("crypto error: {0}")]
    Crypto(#[from] CryptoError),

    #[error("key package build failed: {0}")]
    KeyPackageBuild(String),

    #[error("key package TLS codec error: {0}")]
    KeyPackageTls(#[from] openmls::prelude::tls_codec::Error),
}

/// A minted key package plus the owner's `member_id` — the (bytes, id) bundle
/// the integrator keeps for itself now that de-mls takes both as raw bytes.
#[derive(Debug, Clone)]
pub struct MintedKeyPackage {
    pub bytes: Vec<u8>,
    pub member_id: Vec<u8>,
}

impl MintedKeyPackage {
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn member_id(&self) -> &[u8] {
        &self.member_id
    }
}

/// Encode a key package and its owner's `member_id` into the wire format used
/// for KP announcements. Returns the prost-encoded `MemberInvite` bytes ready
/// for broadcast on the welcome subtopic.
pub fn build_key_package_announcement(key_package: &MintedKeyPackage) -> Vec<u8> {
    MemberInvite {
        key_package_bytes: key_package.bytes.clone(),
        member_id: key_package.member_id.clone(),
    }
    .encode_to_vec()
}

/// Build a fresh MLS credential + signing keypair for `member_id`. The
/// member-id bytes become the credential's serialized content, so the MLS
/// leaf and the protocol's "who is this?" checks agree.
pub fn build_credential(
    member_id: &[u8],
) -> Result<(CredentialWithKey, SignatureKeyPair), MlsSetupError> {
    let signer = SignatureKeyPair::new(GATEWAY_SUITE.signature_algorithm())?;
    let credential = CredentialWithKey {
        credential: BasicCredential::new(member_id.to_vec()).into(),
        signature_key: signer.to_public_vec().into(),
    };
    Ok((credential, signer))
}

/// Reference plug-in helper over `OpenMlsRustCrypto`. Holds the member's
/// credential + signer, owns the single OpenMLS provider every conversation
/// borrows, mints key packages, and builds the per-conversation scoring
/// instances the `User` feeds into the de-mls constructors. One per `User`.
pub struct DefaultConversationPluginsFactory {
    credential: CredentialWithKey,
    signer: SignatureKeyPair,
    /// The one OpenMLS provider for this `User`, borrowed on every driving
    /// call. The creator seeds its group into it; a joiner mints its key
    /// package into it, and the matching welcome opens against the same
    /// provider — the key package's private keys are already there.
    provider: OpenMlsRustCrypto,
    /// Group create-config handed to [`de_mls::Conversation::create`]:
    /// the gateway ciphersuite with the ratchet-tree extension enabled so
    /// joiners need no out-of-band tree.
    group_config: MlsGroupCreateConfig,
}

impl DefaultConversationPluginsFactory {
    pub fn new(credential: CredentialWithKey, signer: SignatureKeyPair) -> Self {
        Self {
            credential,
            signer,
            provider: OpenMlsRustCrypto::default(),
            group_config: MlsGroupCreateConfig::builder()
                .ciphersuite(GATEWAY_SUITE)
                .use_ratchet_tree_extension(true)
                .build(),
        }
    }

    /// The member's MLS credential, handed to [`de_mls::Conversation::create`]
    /// on the creator path.
    pub fn credential(&self) -> CredentialWithKey {
        self.credential.clone()
    }

    /// The User's OpenMLS provider, borrowed into every `Conversation` call.
    pub fn provider(&self) -> &OpenMlsRustCrypto {
        &self.provider
    }

    /// The group create-config for [`de_mls::Conversation::create`].
    pub fn group_config(&self) -> &MlsGroupCreateConfig {
        &self.group_config
    }

    /// Mint a single-use key package into the User's provider so a later join
    /// can open the welcome with the key package's private keys.
    pub fn generate_key_package(&self) -> Result<MintedKeyPackage, MlsSetupError> {
        let member_id = self.credential.credential.serialized_content().to_vec();
        let bundle = KeyPackage::builder()
            .build(
                GATEWAY_SUITE,
                &self.provider,
                &self.signer,
                self.credential.clone(),
            )
            .map_err(|e| MlsSetupError::KeyPackageBuild(e.to_string()))?;
        let bytes = bundle.key_package().tls_serialize_detached()?;
        Ok(MintedKeyPackage { bytes, member_id })
    }

    /// Build a fresh peer-scoring plug-in.
    pub fn make_scoring(&self, config: &ScoringConfig) -> DefaultPeerScoring {
        PeerScoringService::new(
            InMemoryPeerScoreStorage::default(),
            default_score_deltas(),
            config.clone(),
        )
    }
}
