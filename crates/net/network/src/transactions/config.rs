use super::{
    DEFAULT_SOFT_LIMIT_BYTE_SIZE_POOLED_TRANSACTIONS_RESPONSE_ON_PACK_GET_POOLED_TRANSACTIONS_REQUEST,
    SOFT_LIMIT_BYTE_SIZE_POOLED_TRANSACTIONS_RESPONSE,
};

/// Configuration for managing transactions within the network.
#[derive(Debug, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TransactionsManagerConfig {
    /// Configuration for fetching transactions.
    pub transaction_fetcher_config: TransactionFetcherConfig,
}

/// Configuration for fetching transactions.
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TransactionFetcherConfig {
    ///  Soft limit for the byte size of a
    /// [`PooledTransactions`](reth_eth_wire::PooledTransactions) response on assembling a
    /// [`GetPooledTransactions`](reth_eth_wire::GetPooledTransactions) request. Spec'd at 2
    /// MiB.
    pub soft_limit_byte_size_pooled_transactions_response: usize,
    /// Soft limit for the byte size of the expected
    /// [`PooledTransactions`](reth_eth_wire::PooledTransactions) response on packing a
    /// [`GetPooledTransactions`] request with hashes.
    pub soft_limit_byte_size_pooled_transactions_response_on_pack_request: usize,
}

impl TransactionFetcherConfig {
    /// Instantiate a new TransactionFetcherConfig
    pub fn new(
        soft_limit_byte_size_pooled_transactions_response: usize,
        soft_limit_byte_size_pooled_transactions_response_on_pack_request: usize,
    ) -> Self {
        Self {
            soft_limit_byte_size_pooled_transactions_response,
            soft_limit_byte_size_pooled_transactions_response_on_pack_request,
        }
    }
}

impl Default for TransactionFetcherConfig {
    fn default() -> Self {
        Self { soft_limit_byte_size_pooled_transactions_response: SOFT_LIMIT_BYTE_SIZE_POOLED_TRANSACTIONS_RESPONSE, soft_limit_byte_size_pooled_transactions_response_on_pack_request: DEFAULT_SOFT_LIMIT_BYTE_SIZE_POOLED_TRANSACTIONS_RESPONSE_ON_PACK_GET_POOLED_TRANSACTIONS_REQUEST
        }
    }
}
