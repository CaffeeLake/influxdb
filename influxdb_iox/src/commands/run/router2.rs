//! Implementation of command line option for running router2

use std::{collections::BTreeSet, iter, sync::Arc};

use clap_blocks::{
    catalog_dsn::CatalogDsnConfig, run_config::RunConfig, write_buffer::WriteBufferConfig,
};
use data_types::database_rules::{PartitionTemplate, TemplatePart};
use influxdb_ioxd::{
    self,
    server_type::{
        common_state::{CommonServerState, CommonServerStateError},
        router2::RouterServerType,
    },
};
use observability_deps::tracing::*;
use router2::{
    dml_handlers::{
        DmlHandlerChainExt, FanOutAdaptor, InstrumentationDecorator, NamespaceAutocreation,
        Partitioner, SchemaValidator, ShardedWriteBuffer,
    },
    namespace_cache::{metrics::InstrumentedCache, MemoryNamespaceCache, ShardedCache},
    sequencer::Sequencer,
    server::{http::HttpDelegate, RouterServer},
    sharder::JumpHash,
};
use thiserror::Error;
use trace::TraceCollector;
use write_buffer::core::WriteBufferError;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Run: {0}")]
    Run(#[from] influxdb_ioxd::Error),

    #[error("Invalid config: {0}")]
    InvalidConfig(#[from] CommonServerStateError),

    #[error("Catalog error: {0}")]
    Catalog(#[from] iox_catalog::interface::Error),

    #[error("failed to initialise write buffer connection: {0}")]
    WriteBuffer(#[from] WriteBufferError),

    #[error("Catalog DSN error: {0}")]
    CatalogDsn(#[from] clap_blocks::catalog_dsn::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, clap::Parser)]
#[clap(
    name = "run",
    about = "Runs in router2 mode",
    long_about = "Run the IOx router2 server.\n\nThe configuration options below can be \
    set either with the command line flags or with the specified environment \
    variable. If there is a file named '.env' in the current working directory, \
    it is sourced before loading the configuration.

Configuration is loaded from the following sources (highest precedence first):
        - command line arguments
        - user set environment variables
        - .env file contents
        - pre-configured default values"
)]
pub struct Config {
    #[clap(flatten)]
    pub(crate) run_config: RunConfig,

    #[clap(flatten)]
    pub(crate) catalog_dsn: CatalogDsnConfig,

    #[clap(flatten)]
    pub(crate) write_buffer_config: WriteBufferConfig,

    /// Query pool name to dispatch writes to.
    #[clap(
        long = "--query-pool",
        env = "INFLUXDB_IOX_QUERY_POOL_NAME",
        default_value = "iox-shared"
    )]
    pub(crate) query_pool_name: String,
}

pub async fn command(config: Config) -> Result<()> {
    let common_state = CommonServerState::from_config(config.run_config.clone())?;
    let metrics = Arc::new(metric::Registry::default());

    let catalog = config
        .catalog_dsn
        .get_catalog("router2", Arc::clone(&metrics))
        .await?;

    // Initialise the sharded write buffer and instrument it with DML handler
    // metrics.
    let write_buffer = init_write_buffer(
        &config,
        Arc::clone(&metrics),
        common_state.trace_collector(),
    )
    .await?;
    let write_buffer =
        InstrumentationDecorator::new("sharded_write_buffer", Arc::clone(&metrics), write_buffer);

    // Initialise an instrumented namespace cache to be shared with the schema
    // validator, and namespace auto-creator that reports cache hit/miss/update
    // metrics.
    let ns_cache = Arc::new(InstrumentedCache::new(
        Arc::new(ShardedCache::new(
            iter::repeat_with(|| Arc::new(MemoryNamespaceCache::default())).take(10),
        )),
        &*metrics,
    ));

    // Initialise and instrument the schema validator
    let schema_validator = SchemaValidator::new(Arc::clone(&catalog), Arc::clone(&ns_cache));
    let schema_validator =
        InstrumentationDecorator::new("schema_validator", Arc::clone(&metrics), schema_validator);

    // Add a write partitioner into the handler stack that splits by the date
    // portion of the write's timestamp.
    let partitioner = Partitioner::new(PartitionTemplate {
        parts: vec![TemplatePart::TimeFormat("%Y-%m-%d".to_owned())],
    });
    let partitioner =
        InstrumentationDecorator::new("partitioner", Arc::clone(&metrics), partitioner);

    ////////////////////////////////////////////////////////////////////////////
    //
    // THIS CODE IS FOR TESTING ONLY.
    //
    // The source of truth for the kafka topics & query pools will be read from
    // the DB, rather than CLI args for a prod deployment.
    //
    ////////////////////////////////////////////////////////////////////////////
    //
    // Look up the kafka topic ID needed to populate namespace creation
    // requests.
    //
    // This code / auto-creation is for architecture testing purposes only - a
    // prod deployment would expect namespaces to be explicitly created and this
    // layer would be removed.
    let mut txn = catalog.start_transaction().await?;
    let topic_id = txn
        .kafka_topics()
        .get_by_name(config.write_buffer_config.topic())
        .await?
        .map(|v| v.id)
        .unwrap_or_else(|| {
            panic!(
                "no kafka topic named {} in catalog",
                config.write_buffer_config.topic()
            )
        });
    let query_id = txn
        .query_pools()
        .create_or_get(&config.query_pool_name)
        .await
        .map(|v| v.id)
        .unwrap_or_else(|e| {
            panic!(
                "failed to upsert query pool {} in catalog: {}",
                config.write_buffer_config.topic(),
                e
            )
        });
    txn.commit().await?;

    let ns_creator = NamespaceAutocreation::new(
        catalog,
        ns_cache,
        topic_id,
        query_id,
        iox_catalog::INFINITE_RETENTION_POLICY.to_owned(),
    );
    //
    ////////////////////////////////////////////////////////////////////////////

    // Build the chain of DML handlers that forms the request processing
    // pipeline, starting with the namespace creator (for testing purposes) and
    // write partitioner that yields a set of partitioned batches.
    let handler_stack = ns_creator
        .and_then(schema_validator)
        .and_then(partitioner)
        // Once writes have been partitioned, they are processed in parallel.
        //
        // This block initialises a fan-out adaptor that parallelises partitioned
        // writes into the handler chain it decorates (schema validation, and then
        // into the sharded write buffer), and instruments the parallelised
        // operation.
        .and_then(InstrumentationDecorator::new(
            "parallel_write",
            Arc::clone(&metrics),
            FanOutAdaptor::new(write_buffer),
        ));

    // Record the overall request handling latency
    let handler_stack =
        InstrumentationDecorator::new("request", Arc::clone(&metrics), handler_stack);

    let http = HttpDelegate::new(
        config.run_config.max_http_request_size,
        handler_stack,
        &metrics,
    );
    let router_server = RouterServer::new(
        http,
        Default::default(),
        metrics,
        common_state.trace_collector(),
    );
    let server_type = Arc::new(RouterServerType::new(router_server, &common_state));

    info!("starting router2");

    Ok(influxdb_ioxd::main(common_state, server_type).await?)
}

/// Initialise the [`ShardedWriteBuffer`] with one shard per Kafka partition,
/// using [`JumpHash`] to shard operations by their destination namespace &
/// table name.
async fn init_write_buffer(
    config: &Config,
    metrics: Arc<metric::Registry>,
    trace_collector: Option<Arc<dyn TraceCollector>>,
) -> Result<ShardedWriteBuffer<JumpHash<Arc<Sequencer>>>> {
    let write_buffer = Arc::new(
        config
            .write_buffer_config
            .writing(Arc::clone(&metrics), trace_collector)
            .await?,
    );

    // Construct the (ordered) set of sequencers.
    //
    // The sort order must be deterministic in order for all nodes to shard to
    // the same sequencers, therefore we type assert the returned set is of the
    // ordered variety.
    let shards: BTreeSet<_> = write_buffer.sequencer_ids();
    //          ^ don't change this to an unordered set

    info!(
        topic = config.write_buffer_config.topic(),
        shards = shards.len(),
        "connected to write buffer topic",
    );

    Ok(ShardedWriteBuffer::new(
        shards
            .into_iter()
            .map(|id| Sequencer::new(id as _, Arc::clone(&write_buffer), &metrics))
            .map(Arc::new)
            .collect::<JumpHash<_>>(),
    ))
}
