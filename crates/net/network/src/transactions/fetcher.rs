use crate::{
    cache::{LruCache, LruMap},
    message::PeerRequest,
};
use futures::{stream::FuturesUnordered, Future, FutureExt, Stream, StreamExt};
use pin_project::pin_project;
use reth_eth_wire::{
    GetPooledTransactions, HandleAnnouncement, RequestTxHashes, ValidAnnouncementData,
};
use reth_interfaces::p2p::error::{RequestError, RequestResult};
use reth_primitives::{PeerId, PooledTransactionsElement, TxHash};
use schnellru::{ByLength, Unlimited};
use std::{
    collections::HashMap,
    num::NonZeroUsize,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::sync::{mpsc::error::TrySendError, oneshot, oneshot::error::RecvError};
use tracing::{debug, trace};

use super::{
    constants::*, AnnouncementFilter, Peer, PooledTransactions, TransactionsManagerMetrics,
    DEFAULT_SOFT_LIMIT_BYTE_SIZE_POOLED_TRANSACTIONS_RESPONSE,
};

/// The type responsible for fetching missing transactions from peers.
///
/// This will keep track of unique transaction hashes that are currently being fetched and submits
/// new requests on announced hashes.
#[derive(Debug)]
#[pin_project]
pub(super) struct TransactionFetcher {
    /// All peers with an `inflight_requests`.
    pub(super) active_peers: LruMap<PeerId, u8, ByLength>,
    /// All currently active requests for pooled transactions.
    #[pin]
    pub(super) inflight_requests: FuturesUnordered<GetPooledTxRequestFut>,
    /// Hashes that are awaiting an idle peer so they can be fetched.
    pub(super) hashes_pending_fetch: LruCache<TxHash>,
    /// Tracks all hashes in the transaction fetcher.
    pub(super) hashes_unknown_to_pool: LruMap<TxHash, TxFetchMetadata, Unlimited>,
    /// Filter for valid eth68 announcements.
    pub(super) filter_valid_hashes: AnnouncementFilter,
    /// Info on capacity of the transaction fetcher.
    tx_fetcher_info: TransactionFetcherInfo,
}

// === impl TransactionFetcher ===

impl TransactionFetcher {
    /// Removes the specified hashes from inflight tracking.
    #[inline]
    fn remove_from_unknown_hashes<I>(&mut self, hashes: I)
    where
        I: IntoIterator<Item = TxHash>,
    {
        for hash in hashes {
            self.hashes_unknown_to_pool.remove(&hash);
            self.hashes_pending_fetch.remove(&hash);
        }
    }

    /// Updates peer's activity status upon a resolved [`GetPooledTxRequest`].
    fn decrement_inflight_request_count_for(&mut self, peer_id: &PeerId) {
        let remove = || -> bool {
            if let Some(inflight_count) = self.active_peers.get(peer_id) {
                if *inflight_count <= DEFAULT_MAX_COUNT_CONCURRENT_REQUESTS_PER_PEER {
                    return true
                }
                *inflight_count -= 1;
            }
            false
        }();

        if remove {
            self.active_peers.remove(peer_id);
        }
    }

    /// Returns `true` if peer is idle with respect to `self.inflight_requests`.
    pub(super) fn is_idle(&self, peer_id: &PeerId) -> bool {
        let Some(inflight_count) = self.active_peers.peek(peer_id) else { return true };
        if *inflight_count < DEFAULT_MAX_COUNT_CONCURRENT_REQUESTS_PER_PEER {
            return true
        }
        false
    }

    /// Returns any idle peer for the given hash.
    pub(super) fn get_idle_peer_for(
        &self,
        hash: TxHash,
        is_session_active: impl Fn(&PeerId) -> bool,
    ) -> Option<&PeerId> {
        let TxFetchMetadata { fallback_peers, .. } = self.hashes_unknown_to_pool.peek(&hash)?;

        for peer_id in fallback_peers.iter() {
            if self.is_idle(peer_id) && is_session_active(peer_id) {
                return Some(peer_id)
            }
        }

        None
    }

    /// Returns any idle peer for any hash pending fetch. If one is found, the corresponding
    /// hash is written to the request buffer that is passed as parameter.
    ///
    /// Loops through the hashes pending fetch in lru order until one is found with an idle
    /// fallback peer, or the budget passed as parameter is depleted, whatever happens first.
    pub(super) fn find_any_idle_fallback_peer_for_any_pending_hash(
        &mut self,
        hashes_to_request: &mut RequestTxHashes,
        is_session_active: impl Fn(&PeerId) -> bool,
        mut budget: Option<usize>, // try to find `budget` idle peers before giving up
    ) -> Option<PeerId> {
        let mut hashes_pending_fetch_iter = self.hashes_pending_fetch.iter();

        let idle_peer = loop {
            let &hash = hashes_pending_fetch_iter.next()?;

            let idle_peer = self.get_idle_peer_for(hash, &is_session_active);

            if idle_peer.is_some() {
                hashes_to_request.push(hash);
                break idle_peer.copied()
            }

            if let Some(ref mut bud) = budget {
                *bud = bud.saturating_sub(1);
                if *bud == 0 {
                    return None
                }
            }
        };
        let hash = hashes_to_request.first()?;

        // pop hash that is loaded in request buffer from cache of hashes pending fetch
        drop(hashes_pending_fetch_iter);
        _ = self.hashes_pending_fetch.remove(hash);

        idle_peer
    }

    /// Packages hashes for a [`GetPooledTxRequest`] up to limit. Returns left over hashes. Takes
    /// a [`RequestTxHashes`] buffer as parameter for filling with hashes to request.
    ///
    /// Returns left over hashes.
    pub(super) fn pack_hashes(
        &mut self,
        hashes_to_request: &mut RequestTxHashes,
        hashes_from_announcement: ValidAnnouncementData,
    ) -> RequestTxHashes {
        if hashes_from_announcement.msg_version().is_eth68() {
            return self.pack_hashes_eth68(hashes_to_request, hashes_from_announcement)
        }
        self.pack_hashes_eth66(hashes_to_request, hashes_from_announcement)
    }

    /// Packages hashes for a [`GetPooledTxRequest`] from an
    /// [`Eth68`](reth_eth_wire::EthVersion::Eth68) announcement up to limit as defined by protocol
    /// version 68. Takes a [`RequestTxHashes`] buffer as parameter for filling with hashes to
    /// request.
    ///
    /// Returns left over hashes.
    ///
    /// Loops through hashes passed as parameter and checks if a hash fits in the expected
    /// response. If no, it's added to surplus hashes. If yes, it's added to hashes to the request
    /// and expected response size is accumulated.
    pub(super) fn pack_hashes_eth68(
        &mut self,
        hashes_to_request: &mut RequestTxHashes,
        hashes_from_announcement: ValidAnnouncementData,
    ) -> RequestTxHashes {
        let mut acc_size_response = 0;
        let hashes_from_announcement_len = hashes_from_announcement.len();

        let mut hashes_from_announcement_iter = hashes_from_announcement.into_iter();

        if let Some((hash, Some((_ty, size)))) = hashes_from_announcement_iter.next() {
            hashes_to_request.push(hash);

            // tx is really big, pack request with single tx
            if size >= DEFAULT_SOFT_LIMIT_BYTE_SIZE_POOLED_TRANSACTIONS_RESPONSE {
                return hashes_from_announcement_iter.collect::<RequestTxHashes>()
            } else {
                acc_size_response = size;
            }
        }

        let mut surplus_hashes = RequestTxHashes::with_capacity(hashes_from_announcement_len - 1);

        'fold_size: loop {
            let Some((hash, metadata)) = hashes_from_announcement_iter.next() else { break };

            let Some((_ty, size)) = metadata else {
                unreachable!("this method is called upon reception of an eth68 announcement")
            };

            let next_acc_size = acc_size_response + size;

            if next_acc_size <= DEFAULT_SOFT_LIMIT_BYTE_SIZE_POOLED_TRANSACTIONS_RESPONSE {
                // only update accumulated size of tx response if tx will fit in without exceeding
                // soft limit
                acc_size_response = next_acc_size;
                hashes_to_request.push(hash)
            } else {
                surplus_hashes.push(hash)
            }

            let free_space =
                DEFAULT_SOFT_LIMIT_BYTE_SIZE_POOLED_TRANSACTIONS_RESPONSE - acc_size_response;

            if free_space < MEDIAN_BYTE_SIZE_SMALL_LEGACY_TX_ENCODED {
                break 'fold_size
            }
        }

        surplus_hashes.extend(hashes_from_announcement_iter.map(|(hash, _metadata)| hash));
        surplus_hashes.shrink_to_fit();

        surplus_hashes
    }

    /// Packages hashes for a [`GetPooledTxRequest`] from an
    /// [`Eth66`](reth_eth_wire::EthVersion::Eth66) announcement up to limit as defined by
    /// protocol version 66. Takes a [`RequestTxHashes`] buffer as parameter for filling with
    /// hashes to request.
    ///
    /// Returns left over hashes.
    pub(super) fn pack_hashes_eth66(
        &mut self,
        hashes_to_request: &mut RequestTxHashes,
        hashes_from_announcement: ValidAnnouncementData,
    ) -> RequestTxHashes {
        let (mut request_hashes, _version) = hashes_from_announcement.into_request_hashes();
        if request_hashes.len() <= SOFT_LIMIT_COUNT_HASHES_GET_POOLED_TRANSACTIONS_REQUEST {
            *hashes_to_request = request_hashes;

            RequestTxHashes::default()
        } else {
            let surplus_hashes = request_hashes
                .split_off(SOFT_LIMIT_COUNT_HASHES_GET_POOLED_TRANSACTIONS_REQUEST - 1);

            *hashes_to_request = request_hashes;

            RequestTxHashes::new(surplus_hashes)
        }
    }

    /// Tries to buffer hashes for retry.
    pub(super) fn buffer_hashes_for_retry(
        &mut self,
        mut hashes: RequestTxHashes,
        peer_failed_to_serve: &PeerId,
    ) {
        // It could be that the txns have been received over broadcast in the time being. Remove
        // the peer as fallback peer so it isn't request again for these hashes.
        hashes.retain(|hash| {
            if let Some(entry) = self.hashes_unknown_to_pool.get(hash) {
                entry.fallback_peers_mut().remove(peer_failed_to_serve);
                return true
            }
            // tx has been seen over broadcast in the time it took for the request to resolve
            false
        });

        self.buffer_hashes(hashes, None)
    }

    /// Buffers hashes. Note: Only peers that haven't yet tried to request the hashes should be
    /// passed as `fallback_peer` parameter! For re-buffering hashes on failed request, use
    /// [`TransactionFetcher::buffer_hashes_for_retry`]. Hashes that have been re-requested
    /// [`DEFAULT_MAX_RETRIES`], are dropped.
    pub(super) fn buffer_hashes(&mut self, hashes: RequestTxHashes, fallback_peer: Option<PeerId>) {
        let mut max_retried_and_evicted_hashes = vec![];

        for hash in hashes.into_iter() {
            debug_assert!(
                self.hashes_unknown_to_pool.peek(&hash).is_some(),
                "`%hash` in `@buffered_hashes` that's not in `@unknown_hashes`, `@buffered_hashes` should be a subset of keys in `@unknown_hashes`, broken invariant `@buffered_hashes` and `@unknown_hashes`,
`%hash`: {hash},
`@self`: {self:?}",
            );

            let Some(TxFetchMetadata { retries, fallback_peers, .. }) =
                self.hashes_unknown_to_pool.get(&hash)
            else {
                return
            };

            if let Some(peer_id) = fallback_peer {
                // peer has not yet requested hash
                fallback_peers.insert(peer_id);
            } else {
                if *retries >= DEFAULT_MAX_RETRIES {
                    debug!(target: "net::tx",
                        hash=%hash,
                        retries=retries,
                        "retry limit for `GetPooledTransactions` requests reached for hash, dropping hash"
                    );

                    max_retried_and_evicted_hashes.push(hash);
                    continue;
                }
                *retries += 1;
            }
            if let (_, Some(evicted_hash)) = self.hashes_pending_fetch.insert_and_get_evicted(hash)
            {
                max_retried_and_evicted_hashes.push(evicted_hash);
            }
        }

        self.remove_from_unknown_hashes(max_retried_and_evicted_hashes);
    }

    /// Tries to request hashes pending fetch.
    ///
    /// Finds the first buffered hash with a fallback peer that is idle, if any. Fills the rest of
    /// the request by checking the transactions seen by the peer against the buffer.
    pub(super) fn on_fetch_pending_hashes(
        &mut self,
        peers: &HashMap<PeerId, Peer>,
        metrics: &TransactionsManagerMetrics,
        budget_find_idle_fallback_peer: Option<usize>,
    ) {
        let mut hashes_to_request = RequestTxHashes::with_capacity(32);
        let is_session_active = |peer_id: &PeerId| peers.contains_key(peer_id);

        // budget to look for an idle peer before giving up
        let Some(peer_id) = self.find_any_idle_fallback_peer_for_any_pending_hash(
            &mut hashes_to_request,
            is_session_active,
            budget_find_idle_fallback_peer,
        ) else {
            // no peers are idle or budget is depleted
            return
        };
        let Some(peer) = peers.get(&peer_id) else { return };
        let conn_eth_version = peer.version;

        // fill the request with more hashes pending fetch that have been announced by the peer.
        // the search for more hashes is done with respect to the given budget, which determines
        // how many hashes to loop through before giving up and sending a request with the single
        // hash that was taken out of the cache above.

        self.fill_request_from_hashes_pending_fetch(
            &mut hashes_to_request,
            peer.seen_transactions.maybe_pending_transaction_hashes(),
            self.search_breadth_budget_find_intersection_pending_hashes_and_hashes_seen_by_peer(),
        );

        // free unused memory
        hashes_to_request.shrink_to_fit();

        trace!(target: "net::tx",
            peer_id=format!("{peer_id:#}"),
            hashes=?*hashes_to_request,
            conn_eth_version=%conn_eth_version,
            "requesting hashes that were stored pending fetch from peer"
        );

        // request the buffered missing transactions
        if let Some(failed_to_request_hashes) =
            self.request_transactions_from_peer(hashes_to_request, peer, || {
                metrics.egress_peer_channel_full.increment(1)
            })
        {
            debug!(target: "net::tx",
                peer_id=format!("{peer_id:#}"),
                failed_to_request_hashes=?failed_to_request_hashes,
                conn_eth_version=%conn_eth_version,
                "failed sending request to peer's session, buffering hashes"
            );

            self.buffer_hashes(failed_to_request_hashes, Some(peer_id));
        }
    }

    /// Removes the provided transaction hashes from the inflight requests set.
    ///
    /// This is called when we receive full transactions that are currently scheduled for fetching.
    #[inline]
    pub(super) fn on_received_full_transactions_broadcast(
        &mut self,
        hashes: impl IntoIterator<Item = TxHash>,
    ) {
        self.remove_from_unknown_hashes(hashes)
    }

    /// Filters out hashes that have been seen before. For hashes that have already been seen, the
    /// peer is added as fallback peer.
    pub(super) fn filter_unseen_and_pending_hashes(
        &mut self,
        new_announced_hashes: &mut ValidAnnouncementData,
        peer_id: &PeerId,
        is_session_active: impl Fn(PeerId) -> bool,
        client_version: &str,
    ) {
        #[cfg(not(debug_assertions))]
        let mut previously_unseen_hashes_count = 0;
        #[cfg(debug_assertions)]
        let mut previously_unseen_hashes = Vec::with_capacity(new_announced_hashes.len() / 4);

        let msg_version = new_announced_hashes.msg_version();

        // filter out inflight hashes, and register the peer as fallback for all inflight hashes
        new_announced_hashes.retain(|hash, metadata| {
            // occupied entry
            if let Some(TxFetchMetadata{ref mut fallback_peers, tx_encoded_length: ref mut previously_seen_size, ..}) = self.hashes_unknown_to_pool.peek_mut(hash) {
                // update size metadata if available
                if let Some((_ty, size)) = metadata {
                    if let Some(prev_size) = previously_seen_size {
                        // check if this peer is announcing a different size than a previous peer
                        if size != prev_size {
                            debug!(target: "net::tx",
                                peer_id=format!("{peer_id:#}"),
                                hash=%hash,
                                size=size,
                                previously_seen_size=previously_seen_size,
                                client_version=%client_version,
                                "peer announced a different size for tx, this is especially worrying if one size is much bigger..."
                            );
                        }
                    }
                    // believe the most recent peer to announce tx
                    *previously_seen_size = Some(*size);
                }

                // hash has been seen but is not inflight
                if self.hashes_pending_fetch.remove(hash) {
                    return true
                }
                // hash has been seen and is in flight. store peer as fallback peer.
                //
                // remove any ended sessions, so that in case of a full cache, alive peers aren't 
                // removed in favour of lru dead peers
                let mut ended_sessions = vec!();
                for &peer_id in fallback_peers.iter() {
                    if is_session_active(peer_id) {
                        ended_sessions.push(peer_id);
                    }
                }
                for peer_id in ended_sessions {
                    fallback_peers.remove(&peer_id);
                }

                return false
            }

            // vacant entry

            #[cfg(not(debug_assertions))]
            {
                previously_unseen_hashes_count += 1;
            }
            #[cfg(debug_assertions)]
            previously_unseen_hashes.push(*hash);

            trace!(target: "net::tx",
                peer_id=format!("{peer_id:#}"),
                hash=%hash,
                msg_version=%msg_version,
                client_version=%client_version,
                "new hash seen in announcement by peer"
            );

            // todo: allow `MAX_ALTERNATIVE_PEERS_PER_TX` to be zero
            let limit = NonZeroUsize::new(DEFAULT_MAX_COUNT_FALLBACK_PEERS.into()).expect("MAX_ALTERNATIVE_PEERS_PER_TX should be non-zero");

            if self.hashes_unknown_to_pool.get_or_insert(*hash, ||
                TxFetchMetadata{retries: 0, fallback_peers: LruCache::new(limit), tx_encoded_length: None}
            ).is_none() {

                debug!(target: "net::tx",
                    peer_id=format!("{peer_id:#}"),
                    hash=%hash,
                    msg_version=%msg_version,
                    client_version=%client_version,
                    "failed to cache new announced hash from peer in schnellru::LruMap, dropping hash"
                );

                return false
            }
            true
        });

        #[cfg(not(debug_assertions))]
        trace!(target: "net::tx",
            peer_id=format!("{peer_id:#}"),
            previously_unseen_hashes_count=previously_unseen_hashes_count,
            msg_version=%msg_version,
            client_version=%client_version,
            "received previously unseen hashes in announcement from peer"
        );

        #[cfg(debug_assertions)]
        trace!(target: "net::tx",
            peer_id=format!("{peer_id:#}"),
            msg_version=%msg_version,
            client_version=%client_version,
            "received previously unseen hashes in announcement from peer"
        );
    }

    /// Requests the missing transactions from the previously unseen announced hashes of the peer.
    /// Returns the requested hashes if the request concurrency limit is reached or if the request
    /// fails to send over the channel to the peer's session task.
    ///
    /// This filters all announced hashes that are already in flight, and requests the missing,
    /// while marking the given peer as an alternative peer for the hashes that are already in
    /// flight.
    pub(super) fn request_transactions_from_peer(
        &mut self,
        new_announced_hashes: RequestTxHashes,
        peer: &Peer,
        metrics_increment_egress_peer_channel_full: impl FnOnce(),
    ) -> Option<RequestTxHashes> {
        let peer_id: PeerId = peer.request_tx.peer_id;
        let conn_eth_version = peer.version;

        if self.active_peers.len() >= self.tx_fetcher_info.max_inflight_transaction_requests {
            debug!(target: "net::tx",
                peer_id=format!("{peer_id:#}"),
                new_announced_hashes=?*new_announced_hashes,
                conn_eth_version=%conn_eth_version,
                max_inflight_transaction_requests=self.tx_fetcher_info.max_inflight_transaction_requests,
                "limit for concurrent `GetPooledTransactions` requests reached, dropping request for hashes to peer"
            );
            return Some(new_announced_hashes)
        }

        let Some(inflight_count) = self.active_peers.get_or_insert(peer_id, || 0) else {
            debug!(target: "net::tx",
                peer_id=format!("{peer_id:#}"),
                new_announced_hashes=?*new_announced_hashes,
                conn_eth_version=%conn_eth_version,
                "failed to cache active peer in schnellru::LruMap, dropping request to peer"
            );
            return Some(new_announced_hashes)
        };

        if *inflight_count >= DEFAULT_MAX_COUNT_CONCURRENT_REQUESTS_PER_PEER {
            debug!(target: "net::tx",
                peer_id=format!("{peer_id:#}"),
                new_announced_hashes=?*new_announced_hashes,
                conn_eth_version=%conn_eth_version,
                MAX_CONCURRENT_TX_REQUESTS_PER_PEER=DEFAULT_MAX_COUNT_CONCURRENT_REQUESTS_PER_PEER,
                "limit for concurrent `GetPooledTransactions` requests per peer reached"
            );
            return Some(new_announced_hashes)
        }

        *inflight_count += 1;

        debug_assert!(
            || -> bool {
                for hash in new_announced_hashes.iter() {
                    if self.hashes_pending_fetch.contains(hash) {
                        return false
                    }
                }
                true
            }(),
            "`%new_announced_hashes` should been taken out of buffer before packing in a request, breaks invariant `@buffered_hashes` and `@inflight_requests`,
`%new_announced_hashes`: {:?}, 
`@self`: {:?}",
            new_announced_hashes, self
        );

        let (response, rx) = oneshot::channel();
        let req: PeerRequest = PeerRequest::GetPooledTransactions {
            request: GetPooledTransactions(new_announced_hashes.clone()),
            response,
        };

        // try to send the request to the peer
        if let Err(err) = peer.request_tx.try_send(req) {
            // peer channel is full
            match err {
                TrySendError::Full(req) | TrySendError::Closed(req) => {
                    // need to do some cleanup so
                    let req = req.into_get_pooled_transactions().expect("is get pooled tx");

                    metrics_increment_egress_peer_channel_full();
                    return Some(RequestTxHashes::new(req.0))
                }
            }
        } else {
            // stores a new request future for the request
            self.inflight_requests.push(GetPooledTxRequestFut::new(
                peer_id,
                new_announced_hashes,
                rx,
            ))
        }

        None
    }

    /// Tries to fill request with hashes pending fetch so that the expected [`PooledTransactions`]
    /// response is full enough. A mutable reference to a list of hashes to request is passed as
    /// parameter. A budget is passed as parameter, this ensures that the node stops searching
    /// for more hashes after the budget is depleted. Under bad network conditions, the cache of
    /// hashes pending fetch may become very full for a while. As the node recovers, the hashes
    /// pending fetch cache should get smaller. The budget should aim to be big enough to loop
    /// through all buffered hashes in good network conditions.
    ///
    /// The request hashes buffer is filled as if it's an eth68 request, i.e. smartly assemble
    /// the request based on expected response size. For any hash missing size metadata, it is
    /// guessed at [`AVERAGE_BYTE_SIZE_TX_ENCODED`].

    /// Loops through hashes pending fetch and does:
    ///
    /// 1. Check if a hash pending fetch is seen by peer.
    /// 2. Optimistically include the hash in the request.
    /// 3. Accumulate expected total response size.
    /// 4. Check if acc size and hashes count is at limit, if so stop looping.
    /// 5. Remove hashes to request from cache of hashes pending fetch.
    pub(super) fn fill_request_from_hashes_pending_fetch(
        &mut self,
        hashes_to_request: &mut RequestTxHashes,
        seen_hashes: &LruCache<TxHash>,
        mut budget: Option<usize>, // only check `budget` lru pending hashes
    ) {
        let Some(hash) = hashes_to_request.first() else { return };

        let mut acc_size_response = self
            .hashes_unknown_to_pool
            .get(hash)
            .and_then(|entry| entry.tx_encoded_len())
            .unwrap_or(AVERAGE_BYTE_SIZE_TX_ENCODED);

        // if request full enough already, we're satisfied, send request for single tx
        if acc_size_response >=
            DEFAULT_SOFT_LIMIT_BYTE_SIZE_POOLED_TRANSACTIONS_RESPONSE_ON_FETCH_PENDING_HASHES
        {
            return
        }

        // try to fill request by checking if any other hashes pending fetch (in lru order) are
        // also seen by peer
        for hash in self.hashes_pending_fetch.iter() {
            // 1. Check if a hash pending fetch is seen by peer.
            if !seen_hashes.contains(hash) {
                continue;
            };

            // 2. Optimistically include the hash in the request.
            hashes_to_request.push(*hash);

            // 3. Accumulate expected total response size.
            let size = self
                .hashes_unknown_to_pool
                .get(hash)
                .and_then(|entry| entry.tx_encoded_len())
                .unwrap_or(AVERAGE_BYTE_SIZE_TX_ENCODED);

            acc_size_response += size;

            // 4. Check if acc size or hashes count is at limit, if so stop looping.
            // if expected response is full enough or the number of hashes in the request is
            // enough, we're satisfied
            if acc_size_response >=
                DEFAULT_SOFT_LIMIT_BYTE_SIZE_POOLED_TRANSACTIONS_RESPONSE_ON_FETCH_PENDING_HASHES ||
                hashes_to_request.len() >
                    DEFAULT_SOFT_LIMIT_COUNT_HASHES_IN_GET_POOLED_TRANSACTIONS_REQUEST_ON_FETCH_PENDING_HASHES
            {
                break
            }

            if let Some(ref mut bud) = budget {
                *bud = bud.saturating_sub(1);
                if *bud == 0 {
                    return
                }
            }
        }

        // 5. Remove hashes to request from cache of hashes pending fetch.
        for hash in hashes_to_request.iter() {
            self.hashes_pending_fetch.remove(hash);
        }
    }

    /// Returns `true` if [`TransactionFetcher`] has capacity to request pending hashes. Returns  
    /// `false` if [`TransactionFetcher`] is operating close to full capacity, or if too many
    /// hashes are pending fetch.
    pub(super) fn has_capacity_for_fetching_pending_hashes(&self) -> bool {
        let info = &self.tx_fetcher_info;

        self.inflight_requests.len() <= info.max_inflight_transaction_requests ||
            self.hashes_pending_fetch.len() >= info.max_hashes_pending_fetch
    }

    /// Returns the limit to enforce when looking for any pending hash with an idle fallback peer.
    ///
    /// Returns `Some(limit)` if [`TransactionFetcher`] is operating close to full capacity.
    /// Returns `None`, unlimited, if [`TransactionFetcher`] is not that busy or too many hashes
    /// are pending.
    pub(super) fn search_breadth_budget_find_idle_fallback_peer(&self) -> Option<usize> {
        if self.has_capacity_for_fetching_pending_hashes() {
            // unlimited search breadth
            None
        } else {
            // limited breadth of search for idle peer
            trace!(target: "net::tx",
                inflight_requests=self.inflight_requests.len(),
                max_inflight_transaction_requests=self.tx_fetcher_info.max_inflight_transaction_requests,
                hashes_pending_fetch=self.hashes_pending_fetch.len(),
                max_hashes_pending_fetch=self.tx_fetcher_info.max_hashes_pending_fetch,
                "search breadth limited in search for idle fallback peer for some hash pending fetch"
            );

            Some(DEFAULT_BUDGET_FIND_IDLE_FALLBACK_PEER)
        }
    }

    /// Returns the limit to enforce when looking for any pending hash with an idle fallback peer.
    ///
    /// Returns `Some(limit)` if [`TransactionFetcher`] is operating close to full capacity.
    /// Returns `None`, unlimited, if [`TransactionFetcher`] is not that busy or too many hashes
    /// are pending.
    pub(super) fn search_breadth_budget_find_intersection_pending_hashes_and_hashes_seen_by_peer(
        &self,
    ) -> Option<usize> {
        if self.has_capacity_for_fetching_pending_hashes() {
            // unlimited search breadth
            None
        } else {
            // limited breadth of search for idle peer
            trace!(target: "net::tx",
                inflight_requests=self.inflight_requests.len(),
                max_inflight_transaction_requests=self.tx_fetcher_info.max_inflight_transaction_requests,
                hashes_pending_fetch=self.hashes_pending_fetch.len(),
                max_hashes_pending_fetch=self.tx_fetcher_info.max_hashes_pending_fetch,
                "search breadth limited in search for idle fallback peer for some hash pending fetch"
            );

            Some(DEFAULT_BUDGET_FIND_INTERSECTION_ANNOUNCED_BY_PEER_AND_PENDING_FETCH)
        }
    }
}

impl Stream for TransactionFetcher {
    type Item = FetchEvent;

    /// Advances all inflight requests and returns the next event.
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.as_mut().project();
        let res = this.inflight_requests.poll_next_unpin(cx);

        if let Poll::Ready(Some(response)) = res {
            // update peer activity, requests for buffered hashes can only be made to idle
            // fallback peers
            let GetPooledTxResponse { peer_id, mut requested_hashes, result } = response;

            debug_assert!(
                self.active_peers.get(&peer_id).is_some(),
                "`%peer_id` has been removed from `@active_peers` before inflight request(s) resolved, broken invariant `@active_peers` and `@inflight_requests`,
`%peer_id`: {},
`@self`: {:?}",
                peer_id, self
            );

            self.decrement_inflight_request_count_for(&peer_id);

            return match result {
                Ok(Ok(transactions)) => {
                    // clear received hashes
                    let mut fetched = Vec::with_capacity(transactions.hashes().count());
                    requested_hashes.retain(|requested_hash| {
                        if transactions.hashes().any(|hash| hash == requested_hash) {
                            // hash is now known, stop tracking
                            fetched.push(*requested_hash);
                            return false
                        }
                        true
                    });

                    self.remove_from_unknown_hashes(fetched);
                    // buffer left over hashes
                    self.buffer_hashes_for_retry(requested_hashes, &peer_id);

                    Poll::Ready(Some(FetchEvent::TransactionsFetched {
                        peer_id,
                        transactions: transactions.0,
                    }))
                }
                Ok(Err(req_err)) => {
                    self.buffer_hashes_for_retry(requested_hashes, &peer_id);
                    Poll::Ready(Some(FetchEvent::FetchError { peer_id, error: req_err }))
                }
                Err(_) => {
                    self.buffer_hashes_for_retry(requested_hashes, &peer_id);
                    // request channel closed/dropped
                    Poll::Ready(Some(FetchEvent::FetchError {
                        peer_id,
                        error: RequestError::ChannelClosed,
                    }))
                }
            }
        }

        Poll::Pending
    }
}

impl Default for TransactionFetcher {
    fn default() -> Self {
        Self {
            active_peers: LruMap::new(DEFAULT_MAX_COUNT_CONCURRENT_REQUESTS),
            inflight_requests: Default::default(),
            hashes_pending_fetch: LruCache::new(
                NonZeroUsize::new(DEFAULT_MAX_CAPACITY_CACHE_PENDING_FETCH)
                    .expect("buffered cache limit should be non-zero"),
            ),
            hashes_unknown_to_pool: LruMap::new_unlimited(),
            filter_valid_hashes: Default::default(),
            tx_fetcher_info: TransactionFetcherInfo::default(),
        }
    }
}

/// Metadata of a transaction hash that is yet to be fetched.
#[derive(Debug)]
pub(super) struct TxFetchMetadata {
    /// The number of times a request attempt has been made for the hash.
    retries: u8,
    /// Peers that have announced the hash, but to which a request attempt has not yet been made.
    fallback_peers: LruCache<PeerId>,
    /// Size metadata of the transaction if it has been seen in an eth68 announcement.
    // todo: store all seen sizes as a `(size, peer_id)` tuple to catch peers that respond with
    // another size tx than they announced. alt enter in request (won't catch peers announcing
    // wrong size for requests assembled from hashes pending fetch if stored in request fut)
    tx_encoded_length: Option<usize>,
}

impl TxFetchMetadata {
    pub fn fallback_peers_mut(&mut self) -> &mut LruCache<PeerId> {
        &mut self.fallback_peers
    }

    pub fn tx_encoded_len(&self) -> Option<usize> {
        self.tx_encoded_length
    }

    #[cfg(test)]
    pub fn new(
        retries: u8,
        fallback_peers: LruCache<PeerId>,
        tx_encoded_length: Option<usize>,
    ) -> Self {
        Self { retries, fallback_peers, tx_encoded_length }
    }
}

/// Represents possible events from fetching transactions.
#[derive(Debug)]
pub(super) enum FetchEvent {
    /// Triggered when transactions are successfully fetched.
    TransactionsFetched {
        /// The ID of the peer from which transactions were fetched.
        peer_id: PeerId,
        /// The transactions that were fetched, if available.
        transactions: Vec<PooledTransactionsElement>,
    },
    /// Triggered when there is an error in fetching transactions.
    FetchError {
        /// The ID of the peer from which an attempt to fetch transactions resulted in an error.
        peer_id: PeerId,
        /// The specific error that occurred while fetching.
        error: RequestError,
    },
}

/// An inflight request for [`PooledTransactions`] from a peer
pub(super) struct GetPooledTxRequest {
    peer_id: PeerId,
    /// Transaction hashes that were requested, for cleanup purposes
    requested_hashes: RequestTxHashes,
    response: oneshot::Receiver<RequestResult<PooledTransactions>>,
}

pub(super) struct GetPooledTxResponse {
    peer_id: PeerId,
    /// Transaction hashes that were requested, for cleanup purposes, since peer may only return a
    /// subset of requested hashes.
    requested_hashes: RequestTxHashes,
    result: Result<RequestResult<PooledTransactions>, RecvError>,
}

#[must_use = "futures do nothing unless polled"]
#[pin_project::pin_project]
pub(super) struct GetPooledTxRequestFut {
    #[pin]
    inner: Option<GetPooledTxRequest>,
}

impl GetPooledTxRequestFut {
    #[inline]
    fn new(
        peer_id: PeerId,
        requested_hashes: RequestTxHashes,
        response: oneshot::Receiver<RequestResult<PooledTransactions>>,
    ) -> Self {
        Self { inner: Some(GetPooledTxRequest { peer_id, requested_hashes, response }) }
    }
}

impl Future for GetPooledTxRequestFut {
    type Output = GetPooledTxResponse;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut req = self.as_mut().project().inner.take().expect("polled after completion");
        match req.response.poll_unpin(cx) {
            Poll::Ready(result) => Poll::Ready(GetPooledTxResponse {
                peer_id: req.peer_id,
                requested_hashes: req.requested_hashes,
                result,
            }),
            Poll::Pending => {
                self.project().inner.set(Some(req));
                Poll::Pending
            }
        }
    }
}

/// Tracks stats about the [`TransactionFetcher`].
#[derive(Debug)]
pub struct TransactionFetcherInfo {
    /// Currently active outgoing [`GetPooledTransactions`] requests.
    max_inflight_transaction_requests: usize,
    /// Number of pending hashes.
    max_hashes_pending_fetch: usize,
}

impl TransactionFetcherInfo {
    pub fn new(max_inflight_transaction_requests: usize, max_hashes_pending_fetch: usize) -> Self {
        Self { max_inflight_transaction_requests, max_hashes_pending_fetch }
    }
}

impl Default for TransactionFetcherInfo {
    fn default() -> Self {
        Self::new(
            DEFAULT_MAX_INFLIGHT_REQUESTS_ON_FETCH_PENDING_HASHES,
            DEFAULT_MAX_COUNT_PENDING_FETCH,
        )
    }
}

#[cfg(test)]
mod test {
    use std::collections::HashSet;

    use reth_primitives::B256;

    use super::*;

    #[test]
    fn pack_eth68_request() {
        reth_tracing::init_test_tracing();

        let tx_fetcher = &mut TransactionFetcher::default();

        let eth68_hashes = [
            B256::from_slice(&[1; 32]),
            B256::from_slice(&[2; 32]),
            B256::from_slice(&[3; 32]),
            B256::from_slice(&[4; 32]),
            B256::from_slice(&[5; 32]),
        ];
        let eth68_hashes_sizes = [
            DEFAULT_SOFT_LIMIT_BYTE_SIZE_POOLED_TRANSACTIONS_RESPONSE - 2,
            DEFAULT_SOFT_LIMIT_BYTE_SIZE_POOLED_TRANSACTIONS_RESPONSE, /* this one will
                                                                        * not fit */
            2,
            9, // this one won't
            2,
        ];

        // possible included index combinations are
        // (i) 0 and 2
        // (ii) 0 and 4
        // (iii) 1
        // (iv) 2, 3 and 4
        let possible_outcome_1 =
            [eth68_hashes[0], eth68_hashes[2]].into_iter().collect::<HashSet<_>>();
        let possible_outcome_2 =
            [eth68_hashes[0], eth68_hashes[4]].into_iter().collect::<HashSet<_>>();
        let possible_outcome_3 = [eth68_hashes[1]].into_iter().collect::<HashSet<_>>();
        let possible_outcome_4 =
            [eth68_hashes[2], eth68_hashes[3], eth68_hashes[4]].into_iter().collect::<HashSet<_>>();

        let possible_outcomes =
            [possible_outcome_1, possible_outcome_2, possible_outcome_3, possible_outcome_4];

        let mut eth68_hashes_to_request = RequestTxHashes::with_capacity(3);
        let mut valid_announcement_data = ValidAnnouncementData::empty_eth68();
        for i in 0..eth68_hashes.len() {
            valid_announcement_data.insert(eth68_hashes[i], Some((0, eth68_hashes_sizes[i])));
        }
        let surplus_eth68_hashes =
            tx_fetcher.pack_hashes_eth68(&mut eth68_hashes_to_request, valid_announcement_data);

        let combo_surplus_hashes = surplus_eth68_hashes.into_iter().collect::<HashSet<_>>();
        for combo in possible_outcomes.clone() {
            assert_ne!(combo, combo_surplus_hashes)
        }

        let combo_hashes_to_request = eth68_hashes_to_request.into_iter().collect::<HashSet<_>>();

        let mut combo_match = false;
        for combo in possible_outcomes {
            if combo == combo_hashes_to_request {
                combo_match = true;
            }
        }

        assert!(combo_match)
    }
}
