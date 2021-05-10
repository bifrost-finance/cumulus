// Copyright 2019-2021 Parity Technologies (UK) Ltd.
// This file is part of Cumulus.

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus.  If not, see <http://www.gnu.org/licenses/>.

use sc_client_api::{
	Backend, BlockBackend, BlockImportNotification, BlockchainEvents, Finalizer, UsageProvider,
};
use sp_api::ProvideRuntimeApi;
use sp_blockchain::{Error as ClientError, Result as ClientResult};
use sp_consensus::{BlockImport, BlockImportParams, BlockOrigin, BlockStatus, ForkChoiceStrategy};
use sp_runtime::{
	generic::BlockId,
	traits::{Block as BlockT, Header as HeaderT, NumberFor},
};

use polkadot_primitives::v1::{
	Block as PBlock, CandidateReceipt, CommittedCandidateReceipt, Id as ParaId,
	OccupiedCoreAssumption, ParachainHost, SessionIndex,
};
use polkadot_overseer::OverseerHandler;
use polkadot_node_subsystem::messages::AvailabilityRecoveryMessage;
use polkadot_node_primitives::AvailableData;

use codec::Decode;
use futures::{future, select, stream::FuturesUnordered, FutureExt, Stream, StreamExt, channel::oneshot};
use futures_timer::Delay;
use rand::{thread_rng, Rng};

use std::{pin::Pin, sync::Arc, time::Duration};

/// Helper for the relay chain client. This is expected to be a lightweight handle like an `Arc`.
pub trait RelaychainClient: Clone + 'static {
	/// The error type for interacting with the Polkadot client.
	type Error: std::fmt::Debug + Send;

	/// A stream that yields head-data for a parachain.
	type HeadStream: Stream<Item = Vec<u8>> + Send + Unpin;

	/// A stream that yields pending candidates for a parachain.
	type PendingCandidateStream: Stream<Item = (CommittedCandidateReceipt, SessionIndex)> + Send + Unpin;

	/// Get a stream of new best heads for the given parachain.
	fn new_best_heads(&self, para_id: ParaId) -> Self::HeadStream;

	/// Get a stream of finalized heads for the given parachain.
	fn finalized_heads(&self, para_id: ParaId) -> Self::HeadStream;

	/// Returns the parachain head for the given `para_id` at the given block id.
	fn parachain_head_at(
		&self,
		at: &BlockId<PBlock>,
		para_id: ParaId,
	) -> ClientResult<Option<Vec<u8>>>;

	/// Returns a stream of pending candidates for the given `para_id`.
	fn pending_candidates(&self, para_id: ParaId) -> Self::PendingCandidateStream;
}

struct PendingCandidate<Block: BlockT> {
	receipt: CandidateReceipt,
	session_index: SessionIndex,
	block_number: NumberFor<Block>,
}

struct CandidateRecovery<Block: BlockT> {
	pending_candidates: HashMap<Block::Hash, PendingCandidate<Block>>,
	next_candidate_to_recover: FuturesUnordered<Box<dyn Future<Item = Block::Hash> + Send + Sync>>,
	active_candidate_recovery: FuturesUnordered<Box<dyn Future<Item = AvailableData> + Send + Sync>>,
	recovering_candidates: HashSet<Block::Hash>,
	relay_chain_slot_duration: Duration,
	overseer_handler: OverseerHandler,
}

impl<Block: BlockT> CandidateRecovery<Block> {
	fn insert_pending_candidate(
		&mut self,
		hash: Block::Hash,
		block_number: NumberFor<Block>,
		receipt: CandidateReceipt,
		session_index: SessionIndex,
	) {
		if self
			.pending_candidates
			.insert(
				hash,
				PendingCandidate {
					block_number,
					receipt,
					session_index,
				},
			)
			.is_some()
		{
			return;
		}

		// Wait some random time, with the maximum being the slot duration of the relay chain
		// before we start to recover the candidate.
		let delay = Delay::new(self.relay_chain_slot_duration.mul_f64(thread_rng().gen()));
		self.next_candidate_to_recover.push(
			async move {
				delay.await;
				hash
			}
			.boxed(),
		);
	}

	/// Inform about an imported block.
	fn block_imported(&mut self, hash: &Block::Hash) {
		self.pending_candidates.remove(&hash);
	}

	// Inform about a finalized block with the given `block_number`.
	fn block_finalized(&mut self, block_number: NumberFor<Block>) {
		self.pending_candidates
			.retain(|pc| pc.block_number > block_number);
	}

	async fn wait_for_recovery(&mut self) {
		loop {
		select! {
			recover = self.next_candidate_to_recover.next() => {
				let pending_candidate = match self.pending_candidates.remove(&recover) {
					Some(pending_candidate) => pending_candidate,
					None => continue,
				};

				let (tx, rx) = oneshot::channel();

				if let Err(e) = self.overseer_handler.send_message(AvailabilityRecoveryMessage::RecoverAvailableData(
					pending_candidate.receipt,
					pending_candidate.session_index,
					None,
					tx,
				)).await {
					tracing::warn!(
						target: "cumulus-consensus",
						error = ?e,
						"Failed to start availability recovery",
					);
					continue
				}


			},

		}
		}
	}
}

/// Follow the finalized head of the given parachain.
///
/// For every finalized block of the relay chain, it will get the included parachain header
/// corresponding to `para_id` and will finalize it in the parachain.
async fn follow_finalized_head<P, Block, B, R>(
	para_id: ParaId,
	parachain: Arc<P>,
	relay_chain: R,
) -> ClientResult<()>
where
	Block: BlockT,
	P: Finalizer<Block, B> + UsageProvider<Block>,
	R: RelaychainClient,
	B: Backend<Block>,
{
	let mut finalized_heads = relay_chain.finalized_heads(para_id)?;

	loop {
		let finalized_head = if let Some(h) = finalized_heads.next().await {
			h
		} else {
			tracing::debug!(target: "cumulus-consensus", "Stopping following finalized head.");
			return Ok(());
		};

		let header = match Block::Header::decode(&mut &finalized_head[..]) {
			Ok(header) => header,
			Err(err) => {
				tracing::warn!(
					target: "cumulus-consensus",
					error = ?err,
					"Could not decode parachain header while following finalized heads.",
				);
				continue;
			}
		};

		let hash = header.hash();

		// don't finalize the same block multiple times.
		if parachain.usage_info().chain.finalized_hash != hash {
			if let Err(e) = parachain.finalize_block(BlockId::hash(hash), None, true) {
				match e {
					ClientError::UnknownBlock(_) => tracing::debug!(
						target: "cumulus-consensus",
						block_hash = ?hash,
						"Could not finalize block because it is unknown.",
					),
					_ => tracing::warn!(
						target: "cumulus-consensus",
						error = ?e,
						block_hash = ?hash,
						"Failed to finalize block",
					),
				}
			}
		}
	}
}

/// Run the parachain consensus.
///
/// This will follow the given `relay_chain` to act as consesus for the parachain that corresponds
/// to the given `para_id`. It will set the new best block of the parachain as it gets aware of it.
/// The same happens for the finalized block.
///
/// # Note
///
/// This will access the backend of the parachain and thus, this future should be spawned as blocking
/// task.
pub async fn run_parachain_consensus<P, R, Block, B>(
	para_id: ParaId,
	parachain: Arc<P>,
	relay_chain: R,
	announce_block: Arc<dyn Fn(Block::Hash, Option<Vec<u8>>) + Send + Sync>,
) -> ClientResult<()>
where
	Block: BlockT,
	P: Finalizer<Block, B>
		+ UsageProvider<Block>
		+ Send
		+ Sync
		+ BlockBackend<Block>
		+ BlockchainEvents<Block>,
	for<'a> &'a P: BlockImport<Block>,
	R: RelaychainClient,
	B: Backend<Block>,
{
	let follow_new_best = follow_new_best(
		para_id,
		parachain.clone(),
		relay_chain.clone(),
		announce_block,
	);
	let follow_finalized_head = follow_finalized_head(para_id, parachain, relay_chain);
	select! {
		r = follow_new_best.fuse() => r,
		r = follow_finalized_head.fuse() => r,
	}
}

/// Follow the relay chain new best head, to update the Parachain new best head.
async fn follow_new_best<P, R, Block, B>(
	para_id: ParaId,
	parachain: Arc<P>,
	relay_chain: R,
	announce_block: Arc<dyn Fn(Block::Hash, Option<Vec<u8>>) + Send + Sync>,
) where
	Block: BlockT,
	P: Finalizer<Block, B>
		+ UsageProvider<Block>
		+ Send
		+ Sync
		+ BlockBackend<Block>
		+ BlockchainEvents<Block>,
	for<'a> &'a P: BlockImport<Block>,
	R: RelaychainClient,
	B: Backend<Block>,
{
	let mut new_best_heads = relay_chain.new_best_heads(para_id).fuse();
	let mut imported_blocks = parachain.import_notification_stream().fuse();
	let mut pending_candidates = relay_chain.pending_candidates(para_id).fuse();
	// The unset best header of the parachain. Will be `Some(_)` when we have imported a relay chain
	// block before the parachain block it included. In this case we need to wait for this block to
	// be imported to set it as new best.
	let mut unset_best_header = None;

	loop {
		select! {
			h = new_best_heads.next() => {
				match h {
					Some(h) => handle_new_best_parachain_head(
						h,
						&*parachain,
						&mut unset_best_header,
					).await,
					None => {
						tracing::debug!(
							target: "cumulus-consensus",
							"Stopping following new best.",
						);
						return
					}
				}
			},
			i = imported_blocks.next() => {
				match i {
					Some(i) => handle_new_block_imported(
						i,
						&mut unset_best_header,
						&*parachain,
						&*announce_block,
					).await,
					None => {
						tracing::debug!(
							target: "cumulus-consensus",
							"Stopping following imported blocks.",
						);
						return
					}
				}
			},
			c = pending_candidates.next() => {
				match c {
					Some((pending_candidate, session_index)) => handle_pending_candidate(pending_candidate, session_index, &*parachain).await,
					None => {
						tracing::debug!(
							target: "cumulus-consensus",
							"Stopping following pending candidates.",
						);
						return
					}
				}
			},
		}
	}
}

/// Handle a new pending candidate of our parachain.
async fn handle_pending_candidate<Block, P>(
	pending_candidate: CommittedCandidateReceipt,
	session_index: SessionIndex,
	parachain: &P,
) where
	Block: BlockT,
	P: UsageProvider<Block> + Send + Sync + BlockBackend<Block>,
{
	let header = match Block::Header::decode(&mut pending_candidate.commitments.head_data[..]) {
		Ok(header) => header,
		Err(e) => {
			tracing::warn!(
				target: "cumulus-consensus",
				error = ?e,
				"Failed to decode parachain header from pending candidate",
			)
		}
	};

	let hash = header.hash();
	match parachain.block_status(&BlockId::Hash(hash)) {
		Ok(BlockStatus::Unknown) => (),
		Ok(_) => return,
		Err(e) => {
			tracing::debug!(
				target: "cumulus-consensus",
				error = ?e,
				block_hash = ?hash,
				"Failed to get block status",
			);
			return;
		}
	}
}

/// Handle a new import block of the parachain.
async fn handle_new_block_imported<Block, P>(
	notification: BlockImportNotification<Block>,
	unset_best_header_opt: &mut Option<Block::Header>,
	parachain: &P,
	announce_block: &(dyn Fn(Block::Hash, Option<Vec<u8>>) + Send + Sync),
) where
	Block: BlockT,
	P: UsageProvider<Block> + Send + Sync + BlockBackend<Block>,
	for<'a> &'a P: BlockImport<Block>,
{
	// HACK
	//
	// Remove after https://github.com/paritytech/substrate/pull/8052 or similar is merged
	if notification.origin != BlockOrigin::Own {
		announce_block(notification.hash, None);
	}

	let unset_best_header = match (notification.is_new_best, &unset_best_header_opt) {
		// If this is the new best block or we don't have any unset block, we can end it here.
		(true, _) | (_, None) => return,
		(false, Some(ref u)) => u,
	};

	let unset_hash = if notification.header.number() < unset_best_header.number() {
		return;
	} else if notification.header.number() == unset_best_header.number() {
		let unset_hash = unset_best_header.hash();

		if unset_hash != notification.hash {
			return;
		} else {
			unset_hash
		}
	} else {
		unset_best_header.hash()
	};

	match parachain.block_status(&BlockId::Hash(unset_hash)) {
		Ok(BlockStatus::InChainWithState) => {
			drop(unset_best_header);
			let unset_best_header = unset_best_header_opt
				.take()
				.expect("We checked above that the value is set; qed");

			import_block_as_new_best(unset_hash, unset_best_header, parachain).await;
		}
		state => tracing::debug!(
			target: "cumulus-consensus",
			?unset_best_header,
			?notification.header,
			?state,
			"Unexpected state for unset best header.",
		),
	}
}

/// Handle the new best parachain head as extracted from the new best relay chain.
async fn handle_new_best_parachain_head<Block, P>(
	head: Vec<u8>,
	parachain: &P,
	unset_best_header: &mut Option<Block::Header>,
) where
	Block: BlockT,
	P: UsageProvider<Block> + Send + Sync + BlockBackend<Block>,
	for<'a> &'a P: BlockImport<Block>,
{
	let parachain_head = match <<Block as BlockT>::Header>::decode(&mut &head[..]) {
		Ok(header) => header,
		Err(err) => {
			tracing::warn!(
				target: "cumulus-consensus",
				error = ?err,
				"Could not decode Parachain header while following best heads.",
			);
			return;
		}
	};

	let hash = parachain_head.hash();

	if parachain.usage_info().chain.best_hash == hash {
		tracing::debug!(
			target: "cumulus-consensus",
			block_hash = ?hash,
			"Skipping set new best block, because block is already the best.",
		)
	} else {
		// Make sure the block is already known or otherwise we skip setting new best.
		match parachain.block_status(&BlockId::Hash(hash)) {
			Ok(BlockStatus::InChainWithState) => {
				unset_best_header.take();

				import_block_as_new_best(hash, parachain_head, parachain).await;
			}
			Ok(BlockStatus::InChainPruned) => {
				tracing::error!(
					target: "cumulus-collator",
					block_hash = ?hash,
					"Trying to set pruned block as new best!",
				);
			}
			Ok(BlockStatus::Unknown) => {
				*unset_best_header = Some(parachain_head);

				tracing::debug!(
					target: "cumulus-collator",
					block_hash = ?hash,
					"Parachain block not yet imported, waiting for import to enact as best block.",
				);
			}
			Err(e) => {
				tracing::error!(
					target: "cumulus-collator",
					block_hash = ?hash,
					error = ?e,
					"Failed to get block status of block.",
				);
			}
			_ => {}
		}
	}
}

async fn import_block_as_new_best<Block, P>(hash: Block::Hash, header: Block::Header, parachain: &P)
where
	Block: BlockT,
	P: UsageProvider<Block> + Send + Sync + BlockBackend<Block>,
	for<'a> &'a P: BlockImport<Block>,
{
	// Make it the new best block
	let mut block_import_params = BlockImportParams::new(BlockOrigin::ConsensusBroadcast, header);
	block_import_params.fork_choice = Some(ForkChoiceStrategy::Custom(true));
	block_import_params.import_existing = true;

	if let Err(err) = (&*parachain)
		.import_block(block_import_params, Default::default())
		.await
	{
		tracing::warn!(
			target: "cumulus-consensus",
			block_hash = ?hash,
			error = ?err,
			"Failed to set new best block.",
		);
	}
}

impl<T> RelaychainClient for Arc<T>
where
	T: sc_client_api::BlockchainEvents<PBlock> + ProvideRuntimeApi<PBlock> + 'static + Send + Sync,
	<T as ProvideRuntimeApi<PBlock>>::Api: ParachainHost<PBlock>,
{
	type Error = ClientError;

	type HeadStream = Pin<Box<dyn Stream<Item = Vec<u8>> + Send>>;

	type PendingCandidateStream = Pin<Box<dyn Stream<Item = (CommittedCandidateReceipt, SessionIndex)> + Send>>;

	fn new_best_heads(&self, para_id: ParaId) -> Self::HeadStream {
		let relay_chain = self.clone();

		self.import_notification_stream()
			.filter_map(move |n| {
				future::ready(if n.is_new_best {
					relay_chain
						.parachain_head_at(&BlockId::hash(n.hash), para_id)
						.ok()
						.flatten()
				} else {
					None
				})
			})
			.boxed()
	}

	fn finalized_heads(&self, para_id: ParaId) -> Self::HeadStream {
		let relay_chain = self.clone();

		self.finality_notification_stream()
			.filter_map(move |n| {
				future::ready(
					relay_chain
						.parachain_head_at(&BlockId::hash(n.hash), para_id)
						.ok()
						.flatten(),
				)
			})
			.boxed()
	}

	fn parachain_head_at(
		&self,
		at: &BlockId<PBlock>,
		para_id: ParaId,
	) -> ClientResult<Option<Vec<u8>>> {
		self.runtime_api()
			.persisted_validation_data(at, para_id, OccupiedCoreAssumption::TimedOut)
			.map(|s| s.map(|s| s.parent_head.0))
			.map_err(Into::into)
	}

	fn pending_candidates(&self, para_id: ParaId) -> Self::PendingCandidateStream {
		let relay_chain = self.clone();

		self.import_notification_stream()
			.filter_map(move |n| {
				let runtime_api = relay_chain
					.runtime_api();
				future::ready(
					runtime_api
						.candidate_pending_availability(&BlockId::hash(n.hash))
						.and_then(|pa| runtime_api.session_index(&BlockId::hash(n.hash)).map(|v| (pa, v)))
						.map_err(
							|e| tracing::error!(target: "cumulus-consensus", error = ?e, "Failed fetch pending candidates."),
						)
						.ok()
						.flatten(),
				)
			})
			.boxed()
	}
}
