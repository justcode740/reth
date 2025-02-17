//! Optimism Node types config.

use crate::{OptimismEngineTypes, OptimismEvmConfig};
use reth_basic_payload_builder::{BasicPayloadJobGenerator, BasicPayloadJobGeneratorConfig};
use reth_network::NetworkHandle;
use reth_node_builder::{
    components::{ComponentsBuilder, NetworkBuilder, PayloadServiceBuilder, PoolBuilder},
    node::{FullNodeTypes, NodeTypes},
    BuilderContext, PayloadBuilderConfig,
};
use reth_payload_builder::{PayloadBuilderHandle, PayloadBuilderService};
use reth_provider::CanonStateSubscriptions;
use reth_tracing::tracing::{debug, info};
use reth_transaction_pool::{
    blobstore::DiskFileBlobStore, EthTransactionPool, TransactionPool,
    TransactionValidationTaskExecutor,
};

/// Type configuration for a regular Optimism node.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct OptimismNode;
// TODO make this stateful with evm config

impl OptimismNode {
    /// Returns a [`ComponentsBuilder`] configured for a regular Ethereum node.
    pub fn components<Node>(
    ) -> ComponentsBuilder<Node, OptimismPoolBuilder, OptimismPayloadBuilder, OptimismNetwork>
    where
        Node: FullNodeTypes<Engine = OptimismEngineTypes>,
    {
        ComponentsBuilder::default()
            .node_types::<Node>()
            .pool(OptimismPoolBuilder::default())
            .payload(OptimismPayloadBuilder::default())
            .network(OptimismNetwork)
    }
}

impl NodeTypes for OptimismNode {
    type Primitives = ();
    type Engine = OptimismEngineTypes;
    type Evm = OptimismEvmConfig;

    fn evm_config(&self) -> Self::Evm {
        todo!()
    }
}

/// A basic optimism transaction pool.
///
/// This contains various settings that can be configured and take precedence over the node's
/// config.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct OptimismPoolBuilder {
    // TODO add options for txpool args
}

impl<Node> PoolBuilder<Node> for OptimismPoolBuilder
where
    Node: FullNodeTypes,
{
    type Pool = EthTransactionPool<Node::Provider, DiskFileBlobStore>;

    fn build_pool(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Pool> {
        let data_dir = ctx.data_dir();
        let blob_store = DiskFileBlobStore::open(data_dir.blobstore_path(), Default::default())?;
        let validator = TransactionValidationTaskExecutor::eth_builder(ctx.chain_spec())
            .with_head_timestamp(ctx.head().timestamp)
            .kzg_settings(ctx.kzg_settings()?)
            .with_additional_tasks(1)
            .build_with_tasks(
                ctx.provider().clone(),
                ctx.task_executor().clone(),
                blob_store.clone(),
            );

        let transaction_pool =
            reth_transaction_pool::Pool::eth_pool(validator, blob_store, ctx.pool_config());
        info!(target: "reth::cli", "Transaction pool initialized");
        let transactions_path = data_dir.txpool_transactions_path();

        // spawn txpool maintenance task
        {
            let pool = transaction_pool.clone();
            let chain_events = ctx.provider().canonical_state_stream();
            let client = ctx.provider().clone();
            let transactions_backup_config =
                reth_transaction_pool::maintain::LocalTransactionBackupConfig::with_local_txs_backup(transactions_path);

            ctx.task_executor().spawn_critical_with_graceful_shutdown_signal(
                "local transactions backup task",
                |shutdown| {
                    reth_transaction_pool::maintain::backup_local_transactions_task(
                        shutdown,
                        pool.clone(),
                        transactions_backup_config,
                    )
                },
            );

            // spawn the maintenance task
            ctx.task_executor().spawn_critical(
                "txpool maintenance task",
                reth_transaction_pool::maintain::maintain_transaction_pool_future(
                    client,
                    pool,
                    chain_events,
                    ctx.task_executor().clone(),
                    Default::default(),
                ),
            );
            debug!(target: "reth::cli", "Spawned txpool maintenance task");
        }

        Ok(transaction_pool)
    }
}

/// A basic optimism payload service.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct OptimismPayloadBuilder;

impl<Node, Pool> PayloadServiceBuilder<Node, Pool> for OptimismPayloadBuilder
where
    Node: FullNodeTypes<Engine = OptimismEngineTypes>,
    Pool: TransactionPool + Unpin + 'static,
{
    fn spawn_payload_service(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<PayloadBuilderHandle<Node::Engine>> {
        let payload_builder = reth_optimism_payload_builder::OptimismPayloadBuilder::default();
        let conf = ctx.payload_builder_config();

        let payload_job_config = BasicPayloadJobGeneratorConfig::default()
            .interval(conf.interval())
            .deadline(conf.deadline())
            .max_payload_tasks(conf.max_payload_tasks())
            .extradata(conf.extradata_rlp_bytes())
            .max_gas_limit(conf.max_gas_limit());

        let payload_generator = BasicPayloadJobGenerator::with_builder(
            ctx.provider().clone(),
            pool,
            ctx.task_executor().clone(),
            payload_job_config,
            ctx.chain_spec(),
            payload_builder,
        );
        let (payload_service, payload_builder) =
            PayloadBuilderService::new(payload_generator, ctx.provider().canonical_state_stream());

        ctx.task_executor().spawn_critical("payload builder service", Box::pin(payload_service));

        Ok(payload_builder)
    }
}

/// A basic ethereum payload service.
#[derive(Debug, Default, Clone, Copy)]
pub struct OptimismNetwork;

impl<Node, Pool> NetworkBuilder<Node, Pool> for OptimismNetwork
where
    Node: FullNodeTypes,
    Pool: TransactionPool + Unpin + 'static,
{
    fn build_network(self, ctx: &BuilderContext<Node>, pool: Pool) -> eyre::Result<NetworkHandle> {
        let network = ctx.network_builder_blocking()?;
        let handle = ctx.start_network(network, pool);

        Ok(handle)
    }
}
