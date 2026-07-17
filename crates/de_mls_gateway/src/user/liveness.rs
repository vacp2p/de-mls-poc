//! Reference liveness policy.
//!
//! de-mls no longer keeps any liveness timer: it exposes condition queries and
//! manual triggers, and the integrator decides *when* to act. This is the
//! gateway's policy — the same shape the de-mls test harness drives — replaying
//! the old timer behavior on the app's clock so the group keeps making progress.
//! It is deliberately mechanical; the interesting judgment (use real presence /
//! chat / transport signals instead of blind delays) layers on top of it.

use std::time::{Duration, Instant};

use de_mls::ConsensusPlugin;
use openmls_traits::signatures::Signer;

use crate::user::{LockExt, User, UserError};

/// Per-conversation anchors for the takeover/commit policy. Each records when
/// its condition was first seen so the gateway can wait a delay before acting,
/// the way de-mls's internal timers used to. Stored per conversation on the
/// [`User`] and updated by [`User::drive_liveness_policy`] each tick.
#[derive(Default)]
pub struct LivenessAnchors {
    commit: Option<Instant>,
    reelection: Option<Instant>,
    sync_resend: Option<Instant>,
    buffered: Option<Instant>,
    /// When approved work first appeared uncommitted, for the manual-mode stall
    /// warning (distinct from `commit`, which resets when a commit fires).
    stall_since: Option<Instant>,
    /// Whether the current stall was already surfaced, so we warn once per stall.
    stall_warned: bool,
}

/// Per-node liveness policy: each lever independently auto (timer-driven) or
/// manual (off — driven by hand, e.g. a UI button). de-mls keeps no timers, so
/// every lever is the app's to schedule. Defaults reproduce the old automatic
/// behavior except recovery, which starts manual so a demo can trigger it.
#[derive(Debug, Clone, Copy)]
pub struct LivenessPolicy {
    /// Epoch steward auto-commits its approved batch on the commit timer.
    pub auto_commit: bool,
    /// A backup auto-proposes a silent steward's buffered joins/removes.
    pub auto_propose: bool,
    /// A backup auto-answers an unanswered ConversationSync request.
    pub auto_sync: bool,
    /// A backup auto-starts recovery/reelection when the steward is silent.
    pub auto_recover: bool,
}

impl Default for LivenessPolicy {
    fn default() -> Self {
        Self {
            auto_commit: true,
            auto_propose: true,
            auto_sync: true,
            auto_recover: false,
        }
    }
}

/// Arm `anchor` the first tick `active` holds and report whether `delay` has
/// since elapsed; clear it when `active` goes false.
fn window_elapsed(
    anchor: &mut Option<Instant>,
    now: Instant,
    active: bool,
    delay: Duration,
) -> bool {
    if !active {
        *anchor = None;
        return false;
    }
    let start = *anchor.get_or_insert(now);
    now.duration_since(start) >= delay
}

impl<P: ConsensusPlugin, Sig: Signer + Clone> User<P, Sig> {
    /// Drive one liveness-policy pass on `conversation_id`, after
    /// [`User::poll_conversation`]. Reads de-mls's condition queries and pulls
    /// the matching manual trigger once its delay has elapsed: commit the
    /// approved batch, take over a silent primary's buffered proposals / sync
    /// re-send, advance a stalled reelection, and mint in open recovery. A no-op
    /// for a pending-join slot. Best-effort — a trigger error is logged and
    /// retried next tick, mirroring the poll steps this replaces.
    pub fn drive_liveness_policy(
        &self,
        conversation_id: &str,
    ) -> Result<Option<String>, UserError> {
        let Some(entry) = self.lookup_entry(conversation_id)? else {
            return Ok(None);
        };
        let now = Instant::now();
        let cfg = &self.plugins.default_conversation_config;
        let commit_delay = cfg.commit_inactivity_duration;
        let recovery_delay = cfg.recovery_inactivity_duration;
        let window = cfg.voting_inactivity_window();
        let provider = self.plugins.conversation_plugins.provider();
        let signer = &self.signer;
        let policy = self.liveness_policy();

        let mut anchor_map = self
            .liveness_anchors
            .lock()
            .map_err(|_| UserError::LockPoisoned("liveness anchors"))?;
        let anchors = anchor_map.entry(conversation_id.to_string()).or_default();
        let mut stall_warning = None;

        {
            let mut slot = entry.write_or_err("conversation")?;
            // A pending-join slot has no live conversation to drive yet.
            let Ok(convo) = slot.live_mut() else {
                return Ok(None);
            };

            // Commit-inactivity: the epoch steward leads at `commit_inactivity`;
            // everyone else waits an extra recovery window and only steps in for
            // a silent primary. A recovery continuation commits on the short
            // window. Non-stewards entering the freeze produce a NoCandidate that
            // escalates to reelection — the path a group recovers by.
            let lead = convo.is_epoch_steward().unwrap_or(false);
            let has_work = convo.pending_commit_work().is_some();
            // The epoch steward's own commit is the `Commit` lever; a backup
            // stepping in for a silent primary (which escalates to reelection)
            // is the `Recover` lever.
            let should_commit = if lead {
                policy.auto_commit
            } else {
                policy.auto_recover
            };
            if has_work && should_commit {
                let delay = if convo.in_recovery_posture() {
                    recovery_delay
                } else if lead {
                    commit_delay
                } else {
                    commit_delay + recovery_delay
                };
                if window_elapsed(&mut anchors.commit, now, true, delay) {
                    anchors.commit = None;
                    if let Err(e) = convo.commit_now(provider, signer) {
                        tracing::warn!(group = %conversation_id, error = %e, "commit_now failed");
                    }
                }
            } else {
                anchors.commit = None;
            }

            // Reelection-silence: a full silent window counts the round rejected,
            // rotating proposer authority off an unresponsive steward. Always on,
            // even in manual mode: the group only enters Reelection once a member
            // starts the takeover (via Recover), and this is what drives that
            // takeover to a new steward.
            if window_elapsed(
                &mut anchors.reelection,
                now,
                convo.reelection_stalled(),
                window,
            ) {
                anchors.reelection = None;
                if let Err(e) = convo.advance_election_retry(provider, signer) {
                    tracing::warn!(group = %conversation_id, error = %e, "advance_election_retry failed");
                }
            }

            // Sync-resend (`Sync` lever): a backup covers a silent epoch steward.
            let sync_active = policy.auto_sync && convo.awaiting_sync_resend();
            if window_elapsed(&mut anchors.sync_resend, now, sync_active, window) {
                anchors.sync_resend = None;
                if let Err(e) = convo.share_conversation_sync(provider, signer) {
                    tracing::warn!(group = %conversation_id, error = %e, "share_conversation_sync failed");
                }
            }

            // Buffered-propose: the epoch steward proposes immediately (normal
            // operation); a backup takes over a silent primary after the window
            // (the `Propose` lever).
            let buffered = convo.pending_buffered_updates() > 0;
            let is_steward = convo.is_steward();
            let is_epoch = convo.is_epoch_steward().unwrap_or(false);
            let buffered_ready = if buffered && is_epoch {
                anchors.buffered = None;
                true
            } else {
                let active = policy.auto_propose && buffered && is_steward;
                window_elapsed(&mut anchors.buffered, now, active, window)
            };
            if buffered_ready {
                anchors.buffered = None;
                if let Err(e) = convo.propose_buffered_updates(provider, signer) {
                    tracing::warn!(group = %conversation_id, error = %e, "propose_buffered_updates failed");
                }
            }

            // Layer-3 recovery: mint each tick recovery is open (idempotent once
            // a local candidate exists).
            if convo.is_in_recovery_mode()
                && let Err(e) = convo.commit_in_recovery(provider, signer)
            {
                tracing::warn!(group = %conversation_id, error = %e, "commit_in_recovery failed");
            }

            // Stall detection: when recovery is manual, a backup that sees
            // approved work sit uncommitted past the point a commit should have
            // landed warns the user once — the cue to press Recover. Not in
            // recovery (that path is moving).
            if has_work && !policy.auto_recover && !lead && !convo.is_in_recovery_mode() {
                let start = *anchors.stall_since.get_or_insert(now);
                let stalled = now.duration_since(start) >= commit_delay + recovery_delay;
                if stalled && !anchors.stall_warned {
                    anchors.stall_warned = true;
                    stall_warning = Some(
                        "No commit is landing — the epoch steward may be offline. \
                         Press Recover to recover the group."
                            .to_string(),
                    );
                }
            } else {
                anchors.stall_since = None;
                anchors.stall_warned = false;
            }
        }

        self.flush(&entry)?;
        Ok(stall_warning)
    }
}
