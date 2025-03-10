//! The raw node of the raft module.
//!
//! This module contains the value types for the node and it's connection to other
//! nodes but not the raft consensus itself. Generally, you'll interact with the
//! RawNode first and use it to access the inner workings of the consensus protocol.
// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

// Copyright 2015 The etcd Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::mem;

use prost::Message as ProstMsg;

use crate::config::Config;
use crate::default_logger;
use crate::eraftpb::{
    ConfChange, ConfChangeType, ConfState, Entry, EntryType, HardState, Message, MessageType,
    Snapshot,
};
use crate::errors::{Error, Result};
use crate::read_only::ReadState;
use crate::{Raft, SoftState, Status, Storage, INVALID_ID};
use slog::Logger;

/// Represents a Peer node in the cluster.
#[derive(Debug, Default)]
pub struct Peer {
    /// The ID of the peer.
    pub id: u64,
    /// If there is context associated with the peer (like connection information), it can be
    /// serialized and stored here.
    pub context: Option<Vec<u8>>,
}

/// The status of the snapshot.
#[derive(Debug, PartialEq, Copy, Clone)]
pub enum SnapshotStatus {
    /// Represents that the snapshot is finished being created.
    Finish,
    /// Indicates that the snapshot failed to build or is not ready.
    Failure,
}

fn is_local_msg(t: MessageType) -> bool {
    match t {
        MessageType::MsgHup
        | MessageType::MsgBeat
        | MessageType::MsgUnreachable
        | MessageType::MsgSnapStatus
        | MessageType::MsgCheckQuorum => true,
        _ => false,
    }
}

fn is_response_msg(t: MessageType) -> bool {
    match t {
        MessageType::MsgAppendResponse
        | MessageType::MsgRequestVoteResponse
        | MessageType::MsgHeartbeatResponse
        | MessageType::MsgUnreachable
        | MessageType::MsgRequestPreVoteResponse => true,
        _ => false,
    }
}

/// For a given snapshot, determine if it's empty or not.
pub fn is_empty_snap(s: &Snapshot) -> bool {
    s.get_metadata().index == 0
}

/// Ready encapsulates the entries and messages that are ready to read,
/// be saved to stable storage, committed or sent to other peers.
/// All fields in Ready are read-only.
#[derive(Default, Debug, PartialEq)]
pub struct Ready {
    ss: Option<SoftState>,

    hs: Option<HardState>,

    read_states: Vec<ReadState>,

    entries: Vec<Entry>,

    snapshot: Snapshot,

    /// CommittedEntries specifies entries to be committed to a
    /// store/state-machine. These have previously been committed to stable
    /// store.
    pub committed_entries: Option<Vec<Entry>>,

    /// Messages specifies outbound messages to be sent AFTER Entries are
    /// committed to stable storage.
    /// If it contains a MsgSnap message, the application MUST report back to raft
    /// when the snapshot has been received or has failed by calling ReportSnapshot.
    pub messages: Vec<Message>,

    must_sync: bool,
}

impl Ready {
    fn new<T: Storage>(
        raft: &mut Raft<T>,
        prev_ss: &SoftState,
        prev_hs: &HardState,
        since_idx: Option<u64>,
    ) -> Ready {
        let mut rd = Ready {
            entries: raft.raft_log.unstable_entries().unwrap_or(&[]).to_vec(),
            ..Default::default()
        };
        if !raft.msgs.is_empty() {
            mem::swap(&mut raft.msgs, &mut rd.messages);
        }
        rd.committed_entries = Some(
            (match since_idx {
                None => raft.raft_log.next_entries(),
                Some(idx) => raft.raft_log.next_entries_since(idx),
            })
            .unwrap_or_else(Vec::new),
        );
        let ss = raft.soft_state();
        if &ss != prev_ss {
            rd.ss = Some(ss);
        }
        let hs = raft.hard_state();
        if &hs != prev_hs {
            if hs.vote != prev_hs.vote || hs.term != prev_hs.term {
                rd.must_sync = true;
            }
            rd.hs = Some(hs);
        }
        if raft.raft_log.unstable.snapshot.is_some() {
            rd.snapshot = raft.raft_log.unstable.snapshot.clone().unwrap();
        }
        if !raft.read_states.is_empty() {
            rd.read_states = raft.read_states.clone();
        }
        rd
    }

    /// The current volatile state of a Node.
    /// SoftState will be nil if there is no update.
    /// It is not required to consume or store SoftState.
    #[inline]
    pub fn ss(&self) -> Option<&SoftState> {
        self.ss.as_ref()
    }

    /// The current state of a Node to be saved to stable storage BEFORE
    /// Messages are sent.
    /// HardState will be equal to empty state if there is no update.
    #[inline]
    pub fn hs(&self) -> Option<&HardState> {
        self.hs.as_ref()
    }

    /// States can be used for node to serve linearizable read requests locally
    /// when its applied index is greater than the index in ReadState.
    /// Note that the read_state will be returned when raft receives MsgReadIndex.
    /// The returned is only valid for the request that requested to read.
    #[inline]
    pub fn read_states(&self) -> &[ReadState] {
        &self.read_states
    }

    /// Entries specifies entries to be saved to stable storage BEFORE
    /// Messages are sent.
    #[inline]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Snapshot specifies the snapshot to be saved to stable storage.
    #[inline]
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// MustSync indicates whether the HardState and Entries must be synchronously
    /// written to disk or if an asynchronous write is permissible.
    #[inline]
    pub fn must_sync(&self) -> bool {
        self.must_sync
    }
}

/// RawNode is a thread-unsafe Node.
/// The methods of this struct correspond to the methods of Node and are described
/// more fully there.
pub struct RawNode<T: Storage> {
    /// The internal raft state.
    pub raft: Raft<T>,
    prev_ss: SoftState,
    prev_hs: HardState,
    logger: Logger,
}

impl<T: Storage> RawNode<T> {
    #[allow(clippy::new_ret_no_self)]
    /// Create a new RawNode given some [`Config`](../struct.Config.html).
    pub fn new(config: &Config, store: T) -> Result<RawNode<T>> {
        let logger = default_logger().new(o!());
        assert_ne!(config.id, 0, "config.id must not be zero");
        let r = Raft::new(config, store)?;
        let mut rn = RawNode {
            raft: r,
            prev_hs: Default::default(),
            prev_ss: Default::default(),
            logger,
        };
        rn.prev_hs = rn.raft.hard_state();
        rn.prev_ss = rn.raft.soft_state();
        info!(rn.logger, "RawNode created with id {id}.", id = rn.raft.id);
        Ok(rn)
    }

    /// Set a logger.
    #[inline(always)]
    pub fn with_logger(mut self, logger: &Logger) -> Self {
        self.logger = logger.new(o!());
        self.raft = self.raft.with_logger(logger);
        self
    }

    fn commit_ready(&mut self, rd: Ready) {
        if rd.ss.is_some() {
            self.prev_ss = rd.ss.unwrap();
        }
        if let Some(e) = rd.hs {
            if e != HardState::default() {
                self.prev_hs = e;
            }
        }
        if !rd.entries.is_empty() {
            let e = rd.entries.last().unwrap();
            self.raft.raft_log.stable_to(e.index, e.term);
        }
        if rd.snapshot != Snapshot::default() {
            self.raft
                .raft_log
                .stable_snap_to(rd.snapshot.get_metadata().index);
        }
        if !rd.read_states.is_empty() {
            self.raft.read_states.clear();
        }
    }

    fn commit_apply(&mut self, applied: u64) {
        self.raft.commit_apply(applied);
    }

    /// Tick advances the internal logical clock by a single tick.
    ///
    /// Returns true to indicate that there will probably be some readiness which
    /// needs to be handled.
    pub fn tick(&mut self) -> bool {
        self.raft.tick()
    }

    /// Campaign causes this RawNode to transition to candidate state.
    pub fn campaign(&mut self) -> Result<()> {
        let mut m = Message::default();
        m.set_msg_type(MessageType::MsgHup);
        self.raft.step(m)
    }

    /// Propose proposes data be appended to the raft log.
    pub fn propose(&mut self, context: Vec<u8>, data: Vec<u8>) -> Result<()> {
        let mut m = Message::default();
        m.set_msg_type(MessageType::MsgPropose);
        m.set_from(self.raft.id);
        let mut e = Entry::default();
        e.set_data(data);
        e.set_context(context);
        m.set_entries(vec![e]);
        self.raft.step(m)
    }

    /// Broadcast heartbeats to all the followers.
    ///
    /// If it's not leader, nothing will happen.
    pub fn ping(&mut self) {
        self.raft.ping()
    }

    /// ProposeConfChange proposes a config change.
    #[cfg_attr(feature = "cargo-clippy", allow(clippy::needless_pass_by_value))]
    pub fn propose_conf_change(&mut self, context: Vec<u8>, cc: ConfChange) -> Result<()> {
        let mut data = Vec::with_capacity(ProstMsg::encoded_len(&cc));
        cc.encode(&mut data)?;
        let mut m = Message::default();
        m.set_msg_type(MessageType::MsgPropose);
        let mut e = Entry::default();
        e.set_entry_type(EntryType::EntryConfChange);
        e.set_data(data);
        e.set_context(context);
        m.set_entries(vec![e]);
        self.raft.step(m)
    }

    /// Takes the conf change and applies it.
    ///
    /// # Panics
    ///
    /// In the case of `BeginMembershipChange` or `FinalizeConfChange` returning errors this will panic.
    ///
    /// For a safe interface for these directly call `this.raft.begin_membership_change(entry)` or
    /// `this.raft.finalize_membership_change(entry)` respectively.
    pub fn apply_conf_change(&mut self, cc: &ConfChange) -> Result<ConfState> {
        if cc.node_id == INVALID_ID && cc.change_type() != ConfChangeType::BeginMembershipChange {
            let mut cs = ConfState::default();
            cs.set_nodes(self.raft.prs().voter_ids().iter().cloned().collect());
            cs.set_learners(self.raft.prs().learner_ids().iter().cloned().collect());
            return Ok(cs);
        }
        let nid = cc.node_id;
        match cc.change_type() {
            ConfChangeType::AddNode => self.raft.add_node(nid)?,
            ConfChangeType::AddLearnerNode => self.raft.add_learner(nid)?,
            ConfChangeType::RemoveNode => self.raft.remove_node(nid)?,
            ConfChangeType::BeginMembershipChange => self.raft.begin_membership_change(cc)?,
            ConfChangeType::FinalizeMembershipChange => {
                self.raft.mut_prs().finalize_membership_change()?
            }
        };

        Ok(self.raft.prs().configuration().clone().into())
    }

    /// Step advances the state machine using the given message.
    pub fn step(&mut self, m: Message) -> Result<()> {
        // ignore unexpected local messages receiving over network
        if is_local_msg(m.msg_type()) {
            return Err(Error::StepLocalMsg);
        }
        if self.raft.prs().get(m.from).is_some() || !is_response_msg(m.msg_type()) {
            return self.raft.step(m);
        }
        Err(Error::StepPeerNotFound)
    }

    /// Given an index, creates a new Ready value from that index.
    pub fn ready_since(&mut self, applied_idx: u64) -> Ready {
        Ready::new(
            &mut self.raft,
            &self.prev_ss,
            &self.prev_hs,
            Some(applied_idx),
        )
    }

    /// Ready returns the current point-in-time state of this RawNode.
    pub fn ready(&mut self) -> Ready {
        Ready::new(&mut self.raft, &self.prev_ss, &self.prev_hs, None)
    }

    /// Given an index, can determine if there is a ready state from that time.
    pub fn has_ready_since(&self, applied_idx: Option<u64>) -> bool {
        let raft = &self.raft;
        if !raft.msgs.is_empty() || raft.raft_log.unstable_entries().is_some() {
            return true;
        }
        if !raft.read_states.is_empty() {
            return true;
        }
        if self.get_snap().map_or(false, |s| !is_empty_snap(s)) {
            return true;
        }
        let has_unapplied_entries = match applied_idx {
            None => raft.raft_log.has_next_entries(),
            Some(idx) => raft.raft_log.has_next_entries_since(idx),
        };
        if has_unapplied_entries {
            return true;
        }
        if raft.soft_state() != self.prev_ss {
            return true;
        }
        let hs = raft.hard_state();
        if hs != HardState::default() && hs != self.prev_hs {
            return true;
        }
        false
    }

    /// HasReady called when RawNode user need to check if any Ready pending.
    /// Checking logic in this method should be consistent with Ready.containsUpdates().
    #[inline]
    pub fn has_ready(&self) -> bool {
        self.has_ready_since(None)
    }

    /// Grabs the snapshot from the raft if available.
    #[inline]
    pub fn get_snap(&self) -> Option<&Snapshot> {
        self.raft.get_snap()
    }

    /// Advance notifies the RawNode that the application has applied and saved progress in the
    /// last Ready results.
    pub fn advance(&mut self, rd: Ready) {
        self.advance_append(rd);
        let commit_idx = self.prev_hs.commit;
        if commit_idx != 0 {
            // In most cases, prevHardSt and rd.HardState will be the same
            // because when there are new entries to apply we just sent a
            // HardState with an updated Commit value. However, on initial
            // startup the two are different because we don't send a HardState
            // until something changes, but we do send any un-applied but
            // committed entries (and previously-committed entries may be
            // incorporated into the snapshot, even if rd.CommittedEntries is
            // empty). Therefore we mark all committed entries as applied
            // whether they were included in rd.HardState or not.
            self.advance_apply(commit_idx);
        }
    }

    /// Appends and commits the ready value.
    #[inline]
    pub fn advance_append(&mut self, rd: Ready) {
        self.commit_ready(rd);
    }

    /// Advance apply to the passed index.
    #[inline]
    pub fn advance_apply(&mut self, applied: u64) {
        self.commit_apply(applied);
    }

    /// Status returns the current status of the given group.
    #[inline]
    pub fn status(&self) -> Status {
        Status::new(&self.raft)
    }

    /// ReportUnreachable reports the given node is not reachable for the last send.
    pub fn report_unreachable(&mut self, id: u64) {
        let mut m = Message::default();
        m.set_msg_type(MessageType::MsgUnreachable);
        m.set_from(id);
        // we don't care if it is ok actually
        let _ = self.raft.step(m);
    }

    /// ReportSnapshot reports the status of the sent snapshot.
    pub fn report_snapshot(&mut self, id: u64, status: SnapshotStatus) {
        let rej = status == SnapshotStatus::Failure;
        let mut m = Message::default();
        m.set_msg_type(MessageType::MsgSnapStatus);
        m.set_from(id);
        m.set_reject(rej);
        // we don't care if it is ok actually
        let _ = self.raft.step(m);
    }

    /// TransferLeader tries to transfer leadership to the given transferee.
    pub fn transfer_leader(&mut self, transferee: u64) {
        let mut m = Message::default();
        m.set_msg_type(MessageType::MsgTransferLeader);
        m.set_from(transferee);
        let _ = self.raft.step(m);
    }

    /// ReadIndex requests a read state. The read state will be set in ready.
    /// Read State has a read index. Once the application advances further than the read
    /// index, any linearizable read requests issued before the read request can be
    /// processed safely. The read state will have the same rctx attached.
    pub fn read_index(&mut self, rctx: Vec<u8>) {
        let mut m = Message::default();
        m.set_msg_type(MessageType::MsgReadIndex);
        let mut e = Entry::default();
        e.set_data(rctx);
        m.set_entries(vec![e]);
        let _ = self.raft.step(m);
    }

    /// Returns the store as an immutable reference.
    #[inline]
    pub fn get_store(&self) -> &T {
        self.raft.get_store()
    }

    /// Returns the store as a mutable reference.
    #[inline]
    pub fn mut_store(&mut self) -> &mut T {
        self.raft.mut_store()
    }

    /// Set whether skip broadcast empty commit messages at runtime.
    #[inline]
    pub fn skip_bcast_commit(&mut self, skip: bool) {
        self.raft.skip_bcast_commit(skip)
    }

    /// Set whether to batch append msg at runtime.
    #[inline]
    pub fn set_batch_append(&mut self, batch_append: bool) {
        self.raft.set_batch_append(batch_append)
    }
}

#[cfg(test)]
mod test {
    use crate::eraftpb::MessageType;

    use super::is_local_msg;

    #[test]
    fn test_is_local_msg() {
        let tests = vec![
            (MessageType::MsgHup, true),
            (MessageType::MsgBeat, true),
            (MessageType::MsgUnreachable, true),
            (MessageType::MsgSnapStatus, true),
            (MessageType::MsgCheckQuorum, true),
            (MessageType::MsgPropose, false),
            (MessageType::MsgAppend, false),
            (MessageType::MsgAppendResponse, false),
            (MessageType::MsgRequestVote, false),
            (MessageType::MsgRequestVoteResponse, false),
            (MessageType::MsgSnapshot, false),
            (MessageType::MsgHeartbeat, false),
            (MessageType::MsgHeartbeatResponse, false),
            (MessageType::MsgTransferLeader, false),
            (MessageType::MsgTimeoutNow, false),
            (MessageType::MsgReadIndex, false),
            (MessageType::MsgReadIndexResp, false),
            (MessageType::MsgRequestPreVote, false),
            (MessageType::MsgRequestPreVoteResponse, false),
        ];
        for (msg_type, result) in tests {
            assert_eq!(is_local_msg(msg_type), result);
        }
    }
}
