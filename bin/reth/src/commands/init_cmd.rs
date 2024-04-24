//! Command that initializes the node from a genesis file.

use crate::args::{
    utils::{chain_help, genesis_value_parser, SUPPORTED_CHAINS},
    DatabaseArgs, DatadirArgs,
};
use clap::Parser;
use reth_db::init_db;
use reth_node_core::init::init_genesis;
use reth_primitives::ChainSpec;
use reth_provider::ProviderFactory;
use std::sync::Arc;
use tracing::info;

/// Initializes the database with the genesis block.
#[derive(Debug, Parser)]
pub struct InitCommand {
    /// Configure data storage locations
    #[command(flatten)]
    datadir_args: DatadirArgs,

    /// The chain this node is running.
    ///
    /// Possible values are either a built-in chain or the path to a chain specification file.
    #[arg(
        long,
        value_name = "CHAIN_OR_PATH",
        long_help = chain_help(),
        default_value = SUPPORTED_CHAINS[0],
        value_parser = genesis_value_parser
    )]
    chain: Arc<ChainSpec>,

    #[command(flatten)]
    db: DatabaseArgs,
}

impl InitCommand {
    /// Execute the `init` command
    pub async fn execute(self) -> eyre::Result<()> {
        info!(target: "reth::cli", "reth init starting");

        // add network name to data dir
        let data_dir = self
            .datadir_args
            .datadir
            .unwrap_or_chain_default(self.chain.chain, self.datadir_args.clone());
        let db_path = data_dir.db_path();
        info!(target: "reth::cli", path = ?db_path, "Opening database");
        let db = Arc::new(init_db(&db_path, self.db.database_args())?);
        info!(target: "reth::cli", "Database opened");

        let provider_factory = ProviderFactory::new(db, self.chain, data_dir.static_files_path())?;

        info!(target: "reth::cli", "Writing genesis block");

        let hash = init_genesis(provider_factory)?;

        info!(target: "reth::cli", hash = ?hash, "Genesis block written");
        Ok(())
    }
}
