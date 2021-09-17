//! Service and ServiceFactory implementation. Specialized wrapper over substrate service.
#[cfg(all(feature = "automata", feature = "contextfree"))]
compile_error!("Feature 1 and 2 are mutually exclusive and cannot be enabled together");

#[cfg(feature = "automata")]
use automata_rpc as runtime_rpc;
#[cfg(feature = "automata")]
pub use automata_runtime::{self as runtime, opaque::Block, RuntimeApi};
#[cfg(feature = "contextfree")]
use contextfree_rpc as runtime_rpc;
#[cfg(feature = "contextfree")]
pub use contextfree_runtime::{self as runtime, opaque::Block, RuntimeApi};
use fc_consensus::FrontierBlockImport;
use fc_mapping_sync::{MappingSyncWorker, SyncStrategy};
use fc_rpc_core::types::PendingTransactions;
use futures::StreamExt;
use sc_cli::SubstrateCli;
use sc_client_api::{BlockchainEvents, ExecutorProvider, RemoteBackend};
use sc_executor::native_executor_instance;
pub use sc_executor::NativeExecutionDispatch;
pub use sc_executor::NativeExecutor;
use sc_finality_grandpa::FinalityProofProvider;
use sc_finality_grandpa::SharedVoterState;
use sc_keystore::LocalKeystore;
use sc_network::Event;
use sc_service::{error::Error as ServiceError, BasePath, ChainSpec, Configuration, TaskManager};
use sc_telemetry::TelemetryWorker;
pub use sp_api::ConstructRuntimeApi;
use sp_consensus_babe::inherents::InherentDataProvider;
use std::time::Duration;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

// Our native executor instance.
native_executor_instance!(
    pub Executor,
    runtime::api::dispatch,
    runtime::native_version,
    frame_benchmarking::benchmarking::HostFunctions,
);

type FullClient = sc_service::TFullClient<Block, RuntimeApi, Executor>;
type FullBackend = sc_service::TFullBackend<Block>;
type FullSelectChain = sc_consensus::LongestChain<FullBackend, Block>;

pub type ConsensusResult = (
    sc_consensus_babe::BabeBlockImport<
        Block,
        FullClient,
        FrontierBlockImport<
            Block,
            sc_finality_grandpa::GrandpaBlockImport<
                FullBackend,
                Block,
                FullClient,
                FullSelectChain,
            >,
            FullClient,
        >,
    >,
    sc_finality_grandpa::LinkHalf<Block, FullClient, FullSelectChain>,
    sc_consensus_babe::BabeLink<Block>,
);

pub fn open_frontier_backend(config: &Configuration) -> Result<Arc<fc_db::Backend<Block>>, String> {
    let config_dir = config
        .base_path
        .as_ref()
        .map(|base_path| base_path.config_dir(config.chain_spec.id()))
        .unwrap_or_else(|| {
            BasePath::from_project("", "", &crate::cli::Cli::executable_name())
                .config_dir(config.chain_spec.id())
        });
    let database_dir = config_dir.join("frontier").join("db");

    Ok(Arc::new(fc_db::Backend::<Block>::new(
        &fc_db::DatabaseSettings {
            source: fc_db::DatabaseSettingsSrc::RocksDb {
                path: database_dir,
                cache_size: 0,
            },
        },
    )?))
}

pub trait IdentifyVariant {
    /// Returns if this is a configuration for the `Automata` network.
    fn is_automata(&self) -> bool;

    /// Returns if this is a configuration for the `ContextFree` network.
    fn is_contextfree(&self) -> bool;
}

impl IdentifyVariant for Box<dyn ChainSpec> {
    fn is_automata(&self) -> bool {
        self.id().starts_with("automata")
    }
    fn is_contextfree(&self) -> bool {
        self.id().starts_with("contextfree")
    }
}

pub fn new_partial(
    config: &Configuration,
) -> Result<
    sc_service::PartialComponents<
        FullClient,
        FullBackend,
        FullSelectChain,
        sc_consensus::DefaultImportQueue<Block, FullClient>,
        sc_transaction_pool::FullPool<Block, FullClient>,
        (
            ConsensusResult,
            PendingTransactions,
            Arc<fc_db::Backend<Block>>,
        ),
    >,
    ServiceError,
> {
    if config.keystore_remote.is_some() {
        return Err(ServiceError::Other(format!(
            "Remote Keystores are not supported."
        )));
    }

    let telemetry_worker_handle = None;
    let telemetry = config
		.telemetry_endpoints
		.clone()
		.filter(|x| !x.is_empty())
		.map(move |endpoints| -> Result<_, sc_telemetry::Error> {
			let (worker, mut worker_handle) = if let Some(worker_handle) = telemetry_worker_handle {
				(None, worker_handle)
			} else {
				let worker = TelemetryWorker::new(16)?;
				let worker_handle = worker.handle();
				(Some(worker), worker_handle)
			};
			let telemetry = worker_handle.new_telemetry(endpoints);
			Ok((worker, telemetry))
		})
		.transpose()?;

    let (client, backend, keystore_container, task_manager) =
        sc_service::new_full_parts::<Block, RuntimeApi, Executor>(
            &config,
            telemetry.as_ref().map(|(_, telemetry)| telemetry.handle()),
        )?;
    let client = Arc::new(client);

    let select_chain = sc_consensus::LongestChain::new(backend.clone());

    let transaction_pool = sc_transaction_pool::BasicPool::new_full(
        config.transaction_pool.clone(),
        config.role.is_authority().into(),
        config.prometheus_registry(),
        task_manager.spawn_essential_handle(),
        client.clone(),
    );

    let pending_transactions: PendingTransactions = Some(Arc::new(Mutex::new(HashMap::new())));

    let frontier_backend = open_frontier_backend(config)?;

    let telemetry = telemetry.map(|(worker, telemetry)| {
		if let Some(worker) = worker {
			task_manager.spawn_handle().spawn("telemetry", worker.run());
		}
		telemetry
	});

    let (grandpa_block_import, grandpa_link) = sc_finality_grandpa::block_import(
        client.clone(),
        &(client.clone() as Arc<_>),
        select_chain.clone(),
        telemetry.as_ref().map(|x| x.handle()),
    )?;

    let frontier_block_import = FrontierBlockImport::new(
        grandpa_block_import.clone(),
        client.clone(),
        frontier_backend.clone(),
    );

    let (babe_block_import, babe_link) = sc_consensus_babe::block_import(
        sc_consensus_babe::Config::get_or_compute(&*client)?,
        frontier_block_import,
        client.clone(),
    )?;

    let slot_duration = babe_link.config().slot_duration();
    let import_queue = sc_consensus_babe::import_queue(
        babe_link.clone(),
        babe_block_import.clone(),
        Some(Box::new(grandpa_block_import)),
        client.clone(),
        select_chain.clone(),
        move |_, ()| async move {
			let timestamp = sp_timestamp::InherentDataProvider::from_system_time();

			let slot =
				sp_consensus_babe::inherents::InherentDataProvider::from_timestamp_and_duration(
					*timestamp,
					slot_duration,
				);

			Ok((timestamp, slot))
		},
        &task_manager.spawn_essential_handle(),
        config.prometheus_registry(),
        sp_consensus::CanAuthorWithNativeVersion::new(client.executor().clone()),
        telemetry.as_ref().map(|x| x.handle()),
    )?;

    Ok(sc_service::PartialComponents {
        client,
        backend,
        task_manager,
        import_queue,
        keystore_container,
        select_chain,
        transaction_pool,
        other: (
            (babe_block_import, grandpa_link, babe_link),
            pending_transactions,
            frontier_backend,
        ),
    })
}

fn remote_keystore(_url: &String) -> Result<Arc<LocalKeystore>, &'static str> {
    // FIXME: here would the concrete keystore be built,
    //        must return a concrete type (NOT `LocalKeystore`) that
    //        implements `CryptoStore` and `SyncCryptoStore`
    Err("Remote Keystore not supported.")
}

/// Builds a new service for a full client.
pub fn new_full(mut config: Configuration) -> Result<TaskManager, ServiceError> {
    let sc_service::PartialComponents {
        client,
        backend,
        mut task_manager,
        import_queue,
        mut keystore_container,
        select_chain,
        transaction_pool,
        other:
            ((babe_block_import, grandpa_link, babe_link), _pending_transactions, frontier_backend),
    } = new_partial(&config)?;

    if let Some(url) = &config.keystore_remote {
        match remote_keystore(url) {
            Ok(k) => keystore_container.set_remote_keystore(k),
            Err(e) => {
                return Err(ServiceError::Other(format!(
                    "Error hooking up remote keystore for {}: {}",
                    url, e
                )))
            }
        };
    }

    config
        .network
        .extra_sets
        .push(sc_finality_grandpa::grandpa_peers_set_config());

    let warp_sync = Arc::new(sc_finality_grandpa::warp_proof::NetworkProvider::new(
        backend.clone(),
        grandpa_link.shared_authority_set().clone(),
    ));

    let (network, system_rpc_tx, network_starter) =
        sc_service::build_network(sc_service::BuildNetworkParams {
            config: &config,
            client: client.clone(),
            transaction_pool: transaction_pool.clone(),
            spawn_handle: task_manager.spawn_handle(),
            import_queue,
            on_demand: None,
            block_announce_validator_builder: None,
            warp_sync: Some(warp_sync),
        })?;

    if config.offchain_worker.enabled {
        sc_service::build_offchain_workers(
            &config,
            task_manager.spawn_handle(),
            client.clone(),
            network.clone(),
        );
    }

    let role = config.role.clone();
    let force_authoring = config.force_authoring;
    let backoff_authoring_blocks: Option<()> = None;
    let name = config.network.node_name.clone();
    let enable_grandpa = !config.disable_grandpa;
    let prometheus_registry = config.prometheus_registry().cloned();
    let rpc_network = network.clone();
    let pending_transactions: PendingTransactions = Some(Arc::new(Mutex::new(HashMap::new())));
    let is_authority = role.is_authority();
    let subscription_task_executor =
        sc_rpc::SubscriptionTaskExecutor::new(task_manager.spawn_handle());
    let babe_config = babe_link.config().clone();
    let shared_epoch_changes = babe_link.epoch_changes().clone();
    let justification_stream = grandpa_link.justification_stream();
    let shared_authority_set = grandpa_link.shared_authority_set().clone();
    let shared_voter_state = sc_finality_grandpa::SharedVoterState::empty();
    let finality_proof_provider =
        FinalityProofProvider::new_for_service(backend.clone(), Some(shared_authority_set.clone()));

    let rpc_extensions_builder = {
        let client = client.clone();
        let pool = transaction_pool.clone();
        let keystore = keystore_container.sync_keystore();
        let select_chain = select_chain.clone();
        let _pending = pending_transactions.clone();
        let frontier_backend = frontier_backend.clone();
        let max_past_logs = 10000u32; // default 10000 logs

        move |deny_unsafe, subscription_executor| {
            let pending = pending_transactions.clone();
            let deps = runtime_rpc::FullDeps {
                client: client.clone(),
                pool: pool.clone(),
                deny_unsafe,
                enable_dev_signer: false, //TODO
                network: rpc_network.clone(),
                pending_transactions: pending.clone(),
                backend: frontier_backend.clone(),
                is_authority,
                select_chain: select_chain.clone(),
                babe: runtime_rpc::BabeDeps {
                    babe_config: babe_config.clone(),
                    shared_epoch_changes: shared_epoch_changes.clone(),
                    keystore: keystore.clone(),
                },
                grandpa: runtime_rpc::GrandpaDeps {
                    shared_voter_state: shared_voter_state.clone(),
                    shared_authority_set: shared_authority_set.clone(),
                    justification_stream: justification_stream.clone(),
                    subscription_executor,
                    finality_provider: finality_proof_provider.clone(),
                },
                max_past_logs,
            };

            runtime_rpc::create_full(deps, subscription_task_executor.clone()).map_err(Into::into)
        }
    };

    task_manager.spawn_essential_handle().spawn(
        "frontier-mapping-sync-worker",
        MappingSyncWorker::new(
            client.import_notification_stream(),
            Duration::new(6, 0),
            client.clone(),
            backend.clone(),
            frontier_backend.clone(),
            SyncStrategy::Normal,
        )
        .for_each(|()| futures::future::ready(())),
    );

    let telemetry_worker_handle = None;
    let telemetry = config
		.telemetry_endpoints
		.clone()
		.filter(|x| !x.is_empty())
		.map(move |endpoints| -> Result<_, sc_telemetry::Error> {
			let (worker, mut worker_handle) = if let Some(worker_handle) = telemetry_worker_handle {
				(None, worker_handle)
			} else {
				let worker = TelemetryWorker::new(16)?;
				let worker_handle = worker.handle();
				(Some(worker), worker_handle)
			};
			let telemetry = worker_handle.new_telemetry(endpoints);
			Ok((worker, telemetry))
		})
		.transpose()?;

    let mut telemetry = telemetry.map(|(worker, telemetry)| {
        if let Some(worker) = worker {
            task_manager.spawn_handle().spawn("telemetry", worker.run());
        }
        telemetry
    });

    // Spawn authority discovery module.
    if role.is_authority() {
        let authority_discovery_role =
            sc_authority_discovery::Role::PublishAndDiscover(keystore_container.keystore());
        let dht_event_stream =
            network
                .event_stream("authority-discovery")
                .filter_map(|e| async move {
                    match e {
                        Event::Dht(e) => Some(e),
                        _ => None,
                    }
                });
        let (authority_discovery_worker, _service) = sc_authority_discovery::new_worker_and_service(
            client.clone(),
            network.clone(),
            Box::pin(dht_event_stream),
            authority_discovery_role,
            prometheus_registry.clone(),
        );

        task_manager.spawn_handle().spawn(
            "authority-discovery-worker",
            authority_discovery_worker.run(),
        );
    }
    
    if role.is_authority() {
        let proposer_factory = sc_basic_authorship::ProposerFactory::new(
            task_manager.spawn_handle(),
            client.clone(),
            transaction_pool.clone(),
            prometheus_registry.as_ref(),
            telemetry.as_ref().map(|x| x.handle()),
        );

        let can_author_with =
            sp_consensus::CanAuthorWithNativeVersion::new(client.executor().clone());

        let slot_duration = babe_link.config().slot_duration();
        let babe_config = sc_consensus_babe::BabeParams {
            keystore: keystore_container.sync_keystore(),
            client: client.clone(),
            select_chain,
            env: proposer_factory,
            block_import: babe_block_import,
            sync_oracle: network.clone(),
            justification_sync_link: network.clone(),
            create_inherent_data_providers: move |_, ()| async move {
                let timestamp = sp_timestamp::InherentDataProvider::from_system_time();
    
                let slot =
                    sp_consensus_babe::inherents::InherentDataProvider::from_timestamp_and_duration(
                        *timestamp,
                        slot_duration,
                    );
    
                Ok((timestamp, slot))
            },
            force_authoring,
            backoff_authoring_blocks,
            babe_link,
            can_author_with,
            block_proposal_slot_portion: sc_consensus_babe::SlotProportion::new(2f32 / 3f32),
            max_block_proposal_slot_portion: None,
            telemetry: telemetry.as_ref().map(|x| x.handle()),
        };

        let babe = sc_consensus_babe::start_babe(babe_config)?;

        task_manager
            .spawn_essential_handle()
            .spawn_blocking("babe", babe);
    }

    // if the node isn't actively participating in consensus then it doesn't
    // need a keystore, regardless of which protocol we use below.
    let keystore = if role.is_authority() {
        Some(keystore_container.sync_keystore())
    } else {
        None
    };

    let grandpa_config = sc_finality_grandpa::Config {
        // FIXME #1578 make this available through chainspec
        gossip_duration: Duration::from_millis(333),
        justification_period: 512,
        name: Some(name),
        observer_enabled: false,
        keystore,
        telemetry: telemetry.as_ref().map(|x| x.handle()),
        local_role: config.role.clone(),
    };

    let _rpc_handlers =
        sc_service::spawn_tasks(sc_service::SpawnTasksParams {
            network: network.clone(),
            client: client.clone(),
            keystore: keystore_container.sync_keystore(),
            task_manager: &mut task_manager,
            transaction_pool: transaction_pool.clone(),
            rpc_extensions_builder: Box::new(rpc_extensions_builder),
            on_demand: None,
            remote_blockchain: None,
            backend,
            telemetry: telemetry.as_mut(),
            system_rpc_tx,
            config: config,
        })?;

    if enable_grandpa {
        // start the full GRANDPA voter
        // NOTE: non-authorities could run the GRANDPA observer protocol, but at
        // this point the full voter should provide better guarantees of block
        // and vote data availability than the observer. The observer has not
        // been tested extensively yet and having most nodes in a network run it
        // could lead to finality stalls.
        let grandpa_config = sc_finality_grandpa::GrandpaParams {
            config: grandpa_config,
            link: grandpa_link,
            network,
            voting_rule: sc_finality_grandpa::VotingRulesBuilder::default().build(),
            prometheus_registry,
            shared_voter_state: SharedVoterState::empty(),
            telemetry: telemetry.as_ref().map(|x| x.handle()),
        };

        // the GRANDPA voter task is considered infallible, i.e.
        // if it fails we take down the service with it.
        task_manager.spawn_essential_handle().spawn_blocking(
            "grandpa-voter",
            sc_finality_grandpa::run_grandpa_voter(grandpa_config)?,
        );
    }

    network_starter.start_network();
    Ok(task_manager)
}

/// Builds a new service for a light client.
pub fn new_light(mut config: Configuration) -> Result<TaskManager, ServiceError> {
    let telemetry = config
        .telemetry_endpoints
        .clone()
        .filter(|x| !x.is_empty())
        .map(|endpoints| -> Result<_, sc_telemetry::Error> {
            let worker = TelemetryWorker::new(16)?;
            let telemetry = worker.handle().new_telemetry(endpoints);
            Ok((worker, telemetry))
        })
        .transpose()?;

    let (client, backend, keystore_container, mut task_manager, on_demand) =
        sc_service::new_light_parts::<Block, RuntimeApi, Executor>(
            &config,
            telemetry.as_ref().map(|(_, telemetry)| telemetry.handle()),
        )?;

    config
        .network
        .extra_sets
        .push(sc_finality_grandpa::grandpa_peers_set_config());

    let select_chain = sc_consensus::LongestChain::new(backend.clone());

    let transaction_pool = Arc::new(sc_transaction_pool::BasicPool::new_light(
        config.transaction_pool.clone(),
        config.prometheus_registry(),
        task_manager.spawn_essential_handle(),
        client.clone(),
        on_demand.clone(),
    ));

    let mut telemetry = telemetry.map(|(worker, telemetry)| {
		task_manager.spawn_handle().spawn("telemetry", worker.run());
		telemetry
	});

    let (grandpa_block_import, grandpa_link) = sc_finality_grandpa::block_import(
        client.clone(),
        &(client.clone() as Arc<_>),
        select_chain.clone(),
        telemetry.as_ref().map(|x| x.handle()),
    )?;

    let grandpa_block_import_clone = grandpa_block_import.clone();
    let (block_import, babe_link) = sc_consensus_babe::block_import(
        sc_consensus_babe::Config::get_or_compute(&*client)?,
        grandpa_block_import,
        client.clone(),
    )?;

    let slot_duration = babe_link.config().slot_duration();
    let import_queue = sc_consensus_babe::import_queue(
        babe_link,
        block_import,
        Some(Box::new(grandpa_block_import_clone)),
        client.clone(),
        select_chain,
        move |_, ()| async move {
			let timestamp = sp_timestamp::InherentDataProvider::from_system_time();

			let slot =
				sp_consensus_babe::inherents::InherentDataProvider::from_timestamp_and_duration(
					*timestamp,
					slot_duration,
				);

			Ok((timestamp, slot))
		},
        &task_manager.spawn_essential_handle(),
        config.prometheus_registry(),
        sp_consensus::NeverCanAuthor,
        telemetry.as_ref().map(|x| x.handle()),
    )?;

    let warp_sync = Arc::new(sc_finality_grandpa::warp_proof::NetworkProvider::new(
        backend.clone(),
        grandpa_link.shared_authority_set().clone(),
    ));

    let (network, system_rpc_tx, network_starter) =
        sc_service::build_network(sc_service::BuildNetworkParams {
            config: &config,
            client: client.clone(),
            transaction_pool: transaction_pool.clone(),
            spawn_handle: task_manager.spawn_handle(),
            import_queue,
            on_demand: Some(on_demand.clone()),
            block_announce_validator_builder: None,
            warp_sync: Some(warp_sync),
        })?;

    if config.offchain_worker.enabled {
        sc_service::build_offchain_workers(
            &config,            
            task_manager.spawn_handle(),
            client.clone(),
            network.clone(),
        );
    }

    sc_service::spawn_tasks(sc_service::SpawnTasksParams {
        remote_blockchain: Some(backend.remote_blockchain()),
        transaction_pool,
        task_manager: &mut task_manager,
        on_demand: Some(on_demand),
        rpc_extensions_builder: Box::new(sc_service::NoopRpcExtensionBuilder(jsonrpc_core::IoHandler::default())),
        config,
        client,
        keystore: keystore_container.sync_keystore(),
        backend,
        network,
        telemetry: telemetry.as_mut(),
        system_rpc_tx,
    })?;

    network_starter.start_network();

    Ok(task_manager)
}
