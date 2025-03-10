use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use hyper::Uri;
use libsql_replication::injector::LibsqlInjector;
use libsql_replication::replicator::Replicator;
use libsql_replication::rpc::replication::replication_log_client::ReplicationLogClient;
use libsql_sys::name::NamespaceResolver;
use libsql_wal::io::StdIO;
use libsql_wal::registry::WalRegistry;
use libsql_wal::replication::injector::Injector;
use libsql_wal::wal::LibsqlWalManager;
use tokio::task::JoinSet;
use tonic::transport::Channel;

use crate::connection::config::DatabaseConfig;
use crate::connection::libsql::{MakeLibsqlConnection, MakeLibsqlConnectionInner};
use crate::connection::write_proxy::MakeWriteProxyConn;
use crate::connection::MakeConnection;
use crate::database::{Database, LibsqlReplicaDatabase};
use crate::namespace::broadcasters::BroadcasterHandle;
use crate::namespace::configurator::helpers::{make_stats, run_storage_monitor};
use crate::namespace::meta_store::MetaStoreHandle;
use crate::namespace::{
    Namespace, NamespaceBottomlessDbIdInit, NamespaceName, NamespaceStore, ResetCb, ResetOp,
    ResolveNamespacePathFn, RestoreOption,
};
use crate::replication::replicator_client::WalImpl;
use crate::{SqldStorage, DB_CREATE_TIMEOUT};

use super::helpers::cleanup_libsql;
use super::{BaseNamespaceConfig, ConfigureNamespace};

pub struct LibsqlReplicaConfigurator {
    base: BaseNamespaceConfig,
    registry: Arc<WalRegistry<StdIO, SqldStorage>>,
    uri: Uri,
    channel: Channel,
    namespace_resolver: Arc<dyn NamespaceResolver>,
}

impl LibsqlReplicaConfigurator {
    pub fn new(
        base: BaseNamespaceConfig,
        registry: Arc<WalRegistry<StdIO, SqldStorage>>,
        uri: Uri,
        channel: Channel,
        namespace_resolver: Arc<dyn NamespaceResolver>,
    ) -> Self {
        Self {
            base,
            registry,
            uri,
            channel,
            namespace_resolver,
        }
    }
}

impl ConfigureNamespace for LibsqlReplicaConfigurator {
    fn setup<'a>(
        &'a self,
        db_config: MetaStoreHandle,
        restore_option: RestoreOption,
        name: &'a NamespaceName,
        reset: ResetCb,
        resolve_attach_path: ResolveNamespacePathFn,
        store: NamespaceStore,
        broadcaster: BroadcasterHandle,
    ) -> Pin<Box<dyn Future<Output = crate::Result<Namespace>> + Send + 'a>> {
        Box::pin(async move {
            tracing::debug!("creating replica namespace");
            let mut join_set = JoinSet::new();
            let db_path = self.base.base_path.join("dbs").join(name.as_str());
            tokio::fs::create_dir_all(&db_path).await?;
            let channel = self.channel.clone();
            let uri = self.uri.clone();
            let rpc_client = ReplicationLogClient::with_origin(channel.clone(), uri.clone());
            let shared = {
                let registry = self.registry.clone();
                let ns = name.clone().into();
                let db_path = db_path.join("data");
                tokio::task::spawn_blocking(move || registry.open(&db_path, &ns))
                    .await
                    .unwrap()?
            };

            let client = crate::replication::replicator_client::Client::new(
                name.clone(),
                rpc_client,
                db_config.clone(),
                store.clone(),
                WalImpl::new_libsql(shared.clone()),
            )
            .await?;
            let stats = make_stats(
                &db_path,
                &mut join_set,
                db_config.clone(),
                self.base.stats_sender.clone(),
                name.clone(),
            )
            .await?;

            join_set.spawn({
                let stats = stats.clone();
                let mut rcv = shared.new_frame_notifier();
                async move {
                    let _ = rcv
                        .wait_for(move |fno| {
                            stats.set_current_frame_no(*fno);
                            false
                        })
                        .await;
                    Ok(())
                }
            });

            let get_current_frame_no = Arc::new({
                let rcv = shared.new_frame_notifier();
                move || Some(*rcv.borrow())
            });

            let read_connection_maker = MakeLibsqlConnection {
                inner: Arc::new(MakeLibsqlConnectionInner {
                    db_path: db_path.clone().into(),
                    stats: stats.clone(),
                    broadcaster: broadcaster.clone(),
                    config_store: db_config.clone(),
                    extensions: self.base.extensions.clone(),
                    max_response_size: self.base.max_response_size,
                    max_total_response_size: self.base.max_total_response_size,
                    auto_checkpoint: 0,
                    get_current_frame_no: get_current_frame_no.clone(),
                    encryption_config: self.base.encryption_config.clone(),
                    block_writes: Arc::new(true.into()),
                    resolve_attach_path: resolve_attach_path.clone(),
                    wal_manager: LibsqlWalManager::new(
                        self.registry.clone(),
                        self.namespace_resolver.clone(),
                    ),
                }),
            };

            let rcv = shared.new_frame_notifier();
            let wait_for_frame_no = Arc::new(
                move |frame_no| -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
                    let mut rcv = rcv.clone();
                    Box::pin(async move {
                        let _ = rcv.wait_for(|x| *x == frame_no).await;
                    })
                },
            );

            let connection_maker = Arc::new(
                MakeWriteProxyConn::new(
                    channel.clone(),
                    uri.clone(),
                    stats.clone(),
                    wait_for_frame_no,
                    self.base.max_response_size,
                    self.base.max_total_response_size,
                    // FIXME: we need to fetch the primary index before
                    None,
                    self.base.encryption_config.clone(),
                    read_connection_maker,
                    get_current_frame_no,
                )
                .throttled(
                    self.base.max_concurrent_connections.clone(),
                    self.base
                        .connection_creation_timeout
                        .or(Some(DB_CREATE_TIMEOUT)),
                    self.base.max_total_response_size,
                    self.base.max_concurrent_requests,
                    self.base.disable_intelligent_throttling,
                ),
            );

            join_set.spawn(run_storage_monitor(
                Arc::downgrade(&stats),
                connection_maker.clone(),
            ));

            // FIXME: hack, this is necessary for the registry to open the SharedWal
            let _ = connection_maker.create().await?;
            let injector = Injector::new(shared, 10).unwrap();
            let injector = LibsqlInjector::new(injector);
            let mut replicator = Replicator::new(client, injector);

            tracing::debug!("try perform handshake");
            // force a handshake now, to retrieve the primary's current replication index
            match replicator.try_perform_handshake().await {
                Err(libsql_replication::replicator::Error::Meta(
                    libsql_replication::meta::Error::LogIncompatible,
                )) => {
                    tracing::error!(
                        "trying to replicate incompatible logs, reseting replica and nuking db dir"
                    );
                    std::fs::remove_dir_all(&db_path).unwrap();
                    return self
                        .setup(
                            db_config,
                            restore_option,
                            name,
                            reset,
                            resolve_attach_path,
                            store,
                            broadcaster,
                        )
                        .await;
                }
                Err(e) => Err(e)?,
                Ok(_) => (),
            }

            tracing::debug!("done performing handshake");

            let namespace = name.clone();
            let mut retries = 0;
            join_set.spawn(async move {
                use libsql_replication::replicator::Error;
                loop {
                    match replicator.run().await {
                        err if retries > 10 => Err(err)?,
                        err @ Error::Fatal(_) => Err(err)?,
                        err @ Error::NamespaceDoesntExist => {
                            tracing::error!("namespace {namespace} doesn't exist, destroying...");
                            (reset)(ResetOp::Destroy(namespace.clone()));
                            Err(err)?;
                        }
                        e @ Error::Injector(_) => {
                            tracing::error!("potential corruption detected while replicating, reseting  replica: {e}");
                            (reset)(ResetOp::Reset(namespace.clone()));
                            Err(e)?;
                        },
                        Error::Meta(err) => {
                            use libsql_replication::meta::Error;
                            match err {
                                Error::LogIncompatible => {
                                    tracing::error!("trying to replicate incompatible logs, reseting replica");
                                    (reset)(ResetOp::Reset(namespace.clone()));
                                    Err(err)?;
                                }
                                Error::InvalidMetaFile
                                    | Error::Io(_)
                                    | Error::InvalidLogId
                                    | Error::FailedToCommit(_)
                                    | Error::InvalidReplicationPath
                                    | Error::RequiresCleanDatabase => {
                                        // We retry from last frame index?
                                        tracing::warn!("non-fatal replication error, retrying from last commit index: {err}");
                                    },
                            }
                        }
                        e @ (Error::Internal(_)
                            | Error::Client(_)
                            | Error::PrimaryHandshakeTimeout
                            | Error::NeedSnapshot) => {
                            tracing::warn!("non-fatal replication error, retrying from last commit index: {e}");
                        },
                        Error::NoHandshake => {
                            // not strictly necessary, but in case the handshake error goes uncaught,
                            // we reset the client state.
                            replicator.client_mut().reset_token();
                        }
                        Error::SnapshotPending => unreachable!(),
                    }

                    tokio::time::sleep(Duration::from_millis(500) * 2u32.pow(retries)).await;
                    retries += 1;
                }
            });

            Ok(Namespace {
                tasks: join_set,
                db: Database::LibsqlReplica(LibsqlReplicaDatabase { connection_maker }),
                name: name.clone(),
                stats,
                db_config_store: db_config,
                path: db_path.into(),
            })
        })
    }

    fn cleanup<'a>(
        &'a self,
        namespace: &'a NamespaceName,
        _db_config: &DatabaseConfig,
        _prune_all: bool,
        _bottomless_db_id_init: NamespaceBottomlessDbIdInit,
    ) -> Pin<Box<dyn Future<Output = crate::Result<()>> + Send + 'a>> {
        Box::pin(cleanup_libsql(
            namespace,
            &self.registry,
            &self.base.base_path,
        ))
    }

    fn fork<'a>(
        &'a self,
        _from_ns: &'a Namespace,
        _from_config: MetaStoreHandle,
        _to_ns: NamespaceName,
        _to_config: MetaStoreHandle,
        _timestamp: Option<chrono::prelude::NaiveDateTime>,
        _store: NamespaceStore,
    ) -> Pin<Box<dyn Future<Output = crate::Result<Namespace>> + Send + 'a>> {
        Box::pin(std::future::ready(Err(crate::Error::Fork(
            super::fork::ForkError::ForkReplica,
        ))))
    }
}
