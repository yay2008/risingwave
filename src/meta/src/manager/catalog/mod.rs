// Copyright 2024 RisingWave Labs
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

mod database;
mod fragment;
mod user;
mod utils;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::iter;
use std::mem::take;
use std::sync::Arc;

use anyhow::{anyhow, Context};
pub use database::*;
pub use fragment::*;
use itertools::Itertools;
use risingwave_common::catalog::{
    valid_table_name, TableId as StreamingJobId, TableOption, DEFAULT_DATABASE_NAME,
    DEFAULT_SCHEMA_NAME, DEFAULT_SUPER_USER, DEFAULT_SUPER_USER_FOR_PG,
    DEFAULT_SUPER_USER_FOR_PG_ID, DEFAULT_SUPER_USER_ID, SYSTEM_SCHEMAS,
};
use risingwave_common::secret::LocalSecretManager;
use risingwave_common::{bail, current_cluster_version, ensure};
use risingwave_connector::source::cdc::build_cdc_table_id;
use risingwave_connector::source::{should_copy_to_format_encode_options, UPSTREAM_SOURCE_KEY};
use risingwave_pb::catalog::subscription::PbSubscriptionState;
use risingwave_pb::catalog::table::{OptionalAssociatedSourceId, TableType};
use risingwave_pb::catalog::{
    Comment, Connection, CreateType, Database, Function, Index, PbSource, PbStreamJobStatus,
    Schema, Secret, Sink, Source, StreamJobStatus, Subscription, Table, View,
};
use risingwave_pb::ddl_service::{alter_owner_request, alter_set_schema_request, TableJobType};
use risingwave_pb::meta::subscribe_response::{Info, Operation};
use risingwave_pb::user::grant_privilege::{Action, ActionWithGrantOption, Object};
use risingwave_pb::user::update_user_request::UpdateField;
use risingwave_pb::user::{GrantPrivilege, UserInfo};
use tokio::sync::oneshot::Sender;
use tokio::sync::{Mutex, MutexGuard};
use user::*;

pub use self::utils::{get_refed_secret_ids_from_sink, get_refed_secret_ids_from_source};
use crate::manager::{
    IdCategory, MetaSrvEnv, NotificationVersion, StreamingJob, IGNORED_NOTIFICATION_VERSION,
};
use crate::model::{BTreeMapTransaction, MetadataModel, TableFragments};
use crate::storage::Transaction;
use crate::{MetaError, MetaResult};

pub type DatabaseId = u32;
pub type SchemaId = u32;
pub type TableId = u32;
pub type SourceId = u32;
pub type SinkId = u32;
pub type SubscriptionId = u32;
pub type RelationId = u32;
pub type IndexId = u32;
pub type ViewId = u32;
pub type FunctionId = u32;
pub type SecretId = u32;

pub type UserId = u32;
pub type ConnectionId = u32;

pub enum RelationIdEnum {
    Table(TableId),
    Index(IndexId),
    View(ViewId),
    Sink(SinkId),
    Subscription(SubscriptionId),
    Source(SourceId),
}

/// `commit_meta_with_trx` is similar to `commit_meta`, but it accepts an external trx (transaction)
/// and commits it.
macro_rules! commit_meta_with_trx {
    ($manager:expr, $trx:ident, $($val_txn:expr),*) => {
        {
            use tracing::Instrument;
            use $crate::storage::meta_store::MetaStore;
            use $crate::model::{InMemValTransaction, ValTransaction};
            async {
                // Apply the change in `ValTransaction` to trx
                $(
                    $val_txn.apply_to_txn(&mut $trx).await?;
                )*
                // Commit to meta store
                $manager.env.meta_store().as_kv().txn($trx).await?;
                // Upon successful commit, commit the change to in-mem meta
                $(
                    $val_txn.commit();
                )*
                MetaResult::Ok(())
            }
            .instrument(tracing::info_span!(
                "meta_store_commit",
                manager = std::any::type_name_of_val(&*$manager)
            ))
            .await
        }
    };
}

/// `commit_meta` provides a wrapper for committing metadata changes to both in-memory and
/// meta store.
/// * $`manager`: metadata manager, which should contains an env field to access meta store.
/// * $`val_txn`: transactions to commit.
macro_rules! commit_meta {
    ($manager:expr, $($val_txn:expr),*) => {
        {
            let mut trx = Transaction::default();
            $crate::manager::commit_meta_with_trx!($manager, trx, $($val_txn),*)
        }
    };
}

use risingwave_common::util::column_index_mapping::ColIndexMapping;
use risingwave_common::util::epoch::Epoch;
use risingwave_pb::meta::cancel_creating_jobs_request::CreatingJobInfo;
use risingwave_pb::meta::list_object_dependencies_response::PbObjectDependencies;
use risingwave_pb::meta::relation::RelationInfo;
use risingwave_pb::meta::{Relation, RelationGroup};
pub(crate) use {commit_meta, commit_meta_with_trx};

use self::utils::{
    refcnt_dec_sink_secret_ref, refcnt_dec_source_secret_ref, refcnt_inc_sink_secret_ref,
    refcnt_inc_source_secret_ref,
};
use crate::controller::rename::{
    alter_relation_rename, alter_relation_rename_refs, ReplaceTableExprRewriter,
};
use crate::controller::utils::extract_external_table_name_from_definition;
use crate::manager::catalog::utils::{refcnt_dec_connection, refcnt_inc_connection};
use crate::rpc::ddl_controller::DropMode;
use crate::telemetry::MetaTelemetryJobDesc;

pub type CatalogManagerRef = Arc<CatalogManager>;

/// `CatalogManager` manages database catalog information and user information, including
/// authentication and privileges.
///
/// It only has some basic validation for the user information.
/// Other authorization relate to the current session user should be done in Frontend before passing
/// to Meta.
pub struct CatalogManager {
    env: MetaSrvEnv,
    core: Mutex<CatalogManagerCore>,
}

pub struct CatalogManagerCore {
    pub database: DatabaseManager,
    pub user: UserManager,
}

impl CatalogManagerCore {
    async fn new(env: MetaSrvEnv) -> MetaResult<Self> {
        let database = DatabaseManager::new(env.clone()).await?;
        let user = UserManager::new(env.clone(), &database).await?;
        Ok(Self { database, user })
    }

    pub(crate) fn register_finish_notifier(
        &mut self,
        id: TableId,
        sender: Sender<MetaResult<NotificationVersion>>,
    ) {
        self.database
            .creating_table_finish_notifier
            .entry(id)
            .or_default()
            .push(sender);
    }

    pub(crate) fn streaming_job_is_finished(&mut self, job: &StreamingJob) -> MetaResult<bool> {
        fn gen_err(job: &StreamingJob, name: &String) -> MetaError {
            MetaError::catalog_id_not_found(
                job.job_type_str(),
                format!("{} may have been dropped/cancelled", name),
            )
        }
        let (job_status, name) = match job {
            StreamingJob::MaterializedView(table) | StreamingJob::Table(_, table, _) => (
                self.database
                    .tables
                    .get(&table.id)
                    .map(|table| table.stream_job_status),
                &table.name,
            ),
            StreamingJob::Sink(sink, _) => (
                self.database
                    .sinks
                    .get(&sink.id)
                    .map(|sink| sink.stream_job_status),
                &sink.name,
            ),
            StreamingJob::Index(index, _) => (
                self.database
                    .indexes
                    .get(&index.id)
                    .map(|index| index.stream_job_status),
                &index.name,
            ),
            StreamingJob::Source(source) => {
                return Ok(self.database.sources.contains_key(&source.id));
            }
        };

        job_status
            .map(|status| status == StreamJobStatus::Created as i32)
            .or_else(|| {
                if self
                    .database
                    .in_progress_creating_streaming_job
                    .contains_key(&job.id())
                {
                    Some(false)
                } else {
                    None
                }
            })
            .ok_or_else(|| gen_err(job, name))
    }

    pub(crate) fn notify_finish(&mut self, id: TableId, version: NotificationVersion) {
        for tx in self
            .database
            .creating_table_finish_notifier
            .remove(&id)
            .into_iter()
            .flatten()
        {
            let _ = tx.send(Ok(version));
        }
    }

    pub(crate) fn notify_finish_failed(&mut self, err: &MetaError) {
        for tx in take(&mut self.database.creating_table_finish_notifier)
            .into_values()
            .flatten()
        {
            let _ = tx.send(Err(err.clone()));
        }
        // Clear in progress creation streaming job. Note that background job is not tracked here, so that
        // it won't affect background jobs.
        self.database.in_progress_creating_streaming_job.clear();
    }
}

impl CatalogManager {
    pub async fn new(env: MetaSrvEnv) -> MetaResult<Self> {
        let core = Mutex::new(CatalogManagerCore::new(env.clone()).await?);
        let catalog_manager = Self { env, core };
        catalog_manager.init().await?;
        Ok(catalog_manager)
    }

    async fn init(&self) -> MetaResult<()> {
        self.init_user().await?;
        self.init_database().await?;
        self.source_backward_compat_check().await?;
        self.table_sink_catalog_update().await?;
        self.table_catalog_cdc_table_id_update().await?;
        Ok(())
    }

    pub async fn current_notification_version(&self) -> NotificationVersion {
        self.env.notification_manager().current_version().await
    }

    pub async fn get_catalog_core_guard(&self) -> MutexGuard<'_, CatalogManagerCore> {
        self.core.lock().await
    }

    /// This function is for maintaining backward compatibility with older source formats when `format_encode_options` is
    /// merged into `with_properties`.
    /// Context: <https://github.com/risingwavelabs/risingwave/pull/13762>.
    ///
    /// We identify a 'legacy' source based on two conditions:
    /// 1. The `format_encode_options` in `source_info` is empty.
    /// 2. Keys with certain prefixes belonging to `format_encode_options` exist in `with_properties` instead.
    ///
    /// And if the source is identified as 'legacy', we copy the misplaced keys from `with_properties` to `format_encode_options`.
    async fn source_backward_compat_check(&self) -> MetaResult<()> {
        let core = &mut *self.core.lock().await;
        let mut sources = BTreeMapTransaction::new(&mut core.database.sources);
        let legacy_sources = sources
            .tree_ref()
            .iter()
            .filter(|(_, source)| {
                if let Some(source_info) = &source.info
                    && source_info.format_encode_options.is_empty()
                {
                    true
                } else {
                    false
                }
            })
            .map(|t| t.1.clone())
            .collect_vec();
        for mut source in legacy_sources {
            let connector = source
                .with_properties
                .get(UPSTREAM_SOURCE_KEY)
                .unwrap_or(&String::default())
                .to_owned();
            if let Some(source_info) = source.info.as_mut() {
                source_info
                    .format_encode_options
                    .extend(source.with_properties.iter().filter_map(|(k, v)| {
                        should_copy_to_format_encode_options(k, &connector)
                            .then_some((k.to_owned(), v.to_owned()))
                    }))
            }
            sources.insert(source.id, source);
        }
        commit_meta!(self, sources)?;
        Ok(())
    }

    // Fill in the original_target_columns that wasn't written in the previous version for the table sink.
    async fn table_sink_catalog_update(&self) -> MetaResult<()> {
        let core = &mut *self.core.lock().await;
        let mut sinks = BTreeMapTransaction::new(&mut core.database.sinks);
        let tables = BTreeMapTransaction::new(&mut core.database.tables);
        let legacy_sinks = sinks
            .tree_ref()
            .iter()
            .filter(|(_, sink)| {
                sink.target_table.is_some() && sink.original_target_columns.is_empty()
            })
            .map(|(_, sink)| sink.clone())
            .collect_vec();

        for mut sink in legacy_sinks {
            let target_table = sink.target_table();
            sink.original_target_columns
                .clone_from(&tables.get(&target_table).unwrap().columns);
            tracing::info!(
                "updating sink {} target table columns {:?}",
                sink.id,
                sink.original_target_columns
            );

            sinks.insert(sink.id, sink);
        }
        commit_meta!(self, sinks)?;

        Ok(())
    }

    // Fill in the `cdc_table_id` that wasn't written in the previous version for the table.
    async fn table_catalog_cdc_table_id_update(&self) -> MetaResult<()> {
        let core = &mut *self.core.lock().await;
        let sources = BTreeMapTransaction::new(&mut core.database.sources);
        let mut tables = BTreeMapTransaction::new(&mut core.database.tables);
        let legacy_tables = tables
            .tree_ref()
            .iter()
            .filter(|(_, table)| {
                if let Some(rel_id) = table.dependent_relations.first()
                    && sources.contains_key(rel_id)
                    && table.table_type == TableType::Table as i32
                    && table.cdc_table_id.is_none()
                {
                    true
                } else {
                    false
                }
            })
            .map(|(_, table)| table.clone())
            .collect_vec();
        for mut table in legacy_tables {
            let source_id = table.dependent_relations[0];
            match extract_external_table_name_from_definition(&table.definition) {
                None => {
                    tracing::warn!(
                        table_id = table.id,
                        definition = table.definition,
                        "failed to extract cdc table name from table definition.",
                    )
                }
                Some(external_table_name) => {
                    table.cdc_table_id = Some(build_cdc_table_id(source_id, &external_table_name));
                }
            }
            tables.insert(table.id, table);
        }
        commit_meta!(self, tables)?;
        Ok(())
    }
}

// Database catalog related methods
impl CatalogManager {
    async fn init_database(&self) -> MetaResult<()> {
        let mut database = Database {
            name: DEFAULT_DATABASE_NAME.to_string(),
            owner: DEFAULT_SUPER_USER_ID,
            ..Default::default()
        };
        if self
            .core
            .lock()
            .await
            .database
            .check_database_duplicated(&database.name)
            .is_ok()
        {
            database.id = self
                .env
                .id_gen_manager()
                .as_kv()
                .generate::<{ IdCategory::Database }>()
                .await? as u32;
            self.create_database(&database).await?;
        }
        Ok(())
    }

    pub async fn create_database(&self, database: &Database) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let user_core = &mut core.user;
        database_core.check_database_duplicated(&database.name)?;
        #[cfg(not(test))]
        user_core.ensure_user_id(database.owner)?;

        let mut databases = BTreeMapTransaction::new(&mut database_core.databases);
        let mut schemas = BTreeMapTransaction::new(&mut database_core.schemas);
        databases.insert(database.id, database.clone());
        let mut schemas_added = vec![];
        for schema_name in iter::once(DEFAULT_SCHEMA_NAME).chain(SYSTEM_SCHEMAS) {
            let schema = Schema {
                id: self
                    .env
                    .id_gen_manager()
                    .as_kv()
                    .generate::<{ IdCategory::Schema }>()
                    .await? as u32,
                database_id: database.id,
                name: schema_name.to_string(),
                owner: database.owner,
            };
            schemas.insert(schema.id, schema.clone());
            schemas_added.push(schema);
        }

        commit_meta!(self, databases, schemas)?;

        // database and schemas.
        user_core.increase_ref_count(database.owner, 1 + schemas_added.len());

        let mut version = self
            .notify_frontend(Operation::Add, Info::Database(database.to_owned()))
            .await;
        for schema in schemas_added {
            version = self
                .notify_frontend(Operation::Add, Info::Schema(schema))
                .await;
        }

        Ok(version)
    }

    /// return id of streaming jobs in the database which need to be dropped by stream manager.
    pub async fn drop_database(
        &self,
        database_id: DatabaseId,
    ) -> MetaResult<(
        NotificationVersion,
        Vec<StreamingJobId>,
        Vec<SourceId>,
        Vec<Connection>,
    )> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let user_core = &mut core.user;

        if database_core.has_creation_in_database(database_id) {
            return Err(MetaError::permission_denied(
                "Some relations are creating in the target database, try again later",
            ));
        }

        let mut databases = BTreeMapTransaction::new(&mut database_core.databases);
        let mut schemas = BTreeMapTransaction::new(&mut database_core.schemas);
        let mut sources = BTreeMapTransaction::new(&mut database_core.sources);
        let mut sinks = BTreeMapTransaction::new(&mut database_core.sinks);
        let mut subscriptions = BTreeMapTransaction::new(&mut database_core.subscriptions);
        let mut tables = BTreeMapTransaction::new(&mut database_core.tables);
        let mut indexes = BTreeMapTransaction::new(&mut database_core.indexes);
        let mut views = BTreeMapTransaction::new(&mut database_core.views);
        let mut users = BTreeMapTransaction::new(&mut user_core.user_info);
        let mut functions = BTreeMapTransaction::new(&mut database_core.functions);
        let mut connections = BTreeMapTransaction::new(&mut database_core.connections);
        let mut secrets = BTreeMapTransaction::new(&mut database_core.secrets);

        /// `drop_by_database_id` provides a wrapper for dropping relations by database id, it will
        /// return the relation ids that dropped.
        /// * $`val_txn`: transactions to the relations.
        /// * $`database_id`: database id.
        macro_rules! drop_by_database_id {
            ($val_txn:expr, $database_id:ident) => {{
                let ids_to_drop = $val_txn
                    .tree_ref()
                    .values()
                    .filter(|relation| relation.database_id == $database_id)
                    .map(|relation| relation.id)
                    .collect_vec();
                ids_to_drop
                    .into_iter()
                    .map(|id| $val_txn.remove(id).unwrap())
                    .collect_vec()
            }};
        }

        let database = databases.remove(database_id);
        let connections_dropped;
        if let Some(database) = database {
            let schemas_to_drop = drop_by_database_id!(schemas, database_id);
            let sources_to_drop = drop_by_database_id!(sources, database_id);
            let sinks_to_drop = drop_by_database_id!(sinks, database_id);
            let subscriptions_to_drop = drop_by_database_id!(subscriptions, database_id);
            let tables_to_drop = drop_by_database_id!(tables, database_id);
            let indexes_to_drop = drop_by_database_id!(indexes, database_id);
            let views_to_drop = drop_by_database_id!(views, database_id);
            let functions_to_drop = drop_by_database_id!(functions, database_id);
            let connections_to_drop = drop_by_database_id!(connections, database_id);
            let secrets_to_drop = drop_by_database_id!(secrets, database_id);
            connections_dropped = connections_to_drop.clone();

            let objects = std::iter::once(Object::DatabaseId(database_id))
                .chain(
                    schemas_to_drop
                        .iter()
                        .map(|schema| Object::SchemaId(schema.id)),
                )
                .chain(views_to_drop.iter().map(|view| Object::ViewId(view.id)))
                .chain(tables_to_drop.iter().map(|table| Object::TableId(table.id)))
                .chain(
                    sources_to_drop
                        .iter()
                        .map(|source| Object::SourceId(source.id)),
                )
                .chain(
                    functions_to_drop
                        .iter()
                        .map(|function| Object::FunctionId(function.id)),
                )
                .collect_vec();
            let users_need_update = Self::update_user_privileges(&mut users, &objects);

            commit_meta!(
                self,
                databases,
                schemas,
                sources,
                sinks,
                subscriptions,
                tables,
                indexes,
                views,
                users,
                connections,
                functions
            )?;

            std::iter::once(database.owner)
                .chain(schemas_to_drop.iter().map(|schema| schema.owner))
                .chain(sources_to_drop.iter().map(|source| source.owner))
                .chain(sinks_to_drop.iter().map(|sink| sink.owner))
                .chain(
                    subscriptions_to_drop
                        .iter()
                        .map(|subscription| subscription.owner),
                )
                .chain(
                    tables_to_drop
                        .iter()
                        .filter(|table| valid_table_name(&table.name))
                        .map(|table| table.owner),
                )
                .chain(indexes_to_drop.iter().map(|index| index.owner))
                .chain(views_to_drop.iter().map(|view| view.owner))
                .chain(functions_to_drop.iter().map(|function| function.owner))
                .chain(
                    connections_to_drop
                        .iter()
                        .map(|connection| connection.owner),
                )
                .chain(secrets_to_drop.iter().map(|secret| secret.owner))
                .for_each(|owner_id| user_core.decrease_ref(owner_id));

            // Update relation ref count.
            for table in &tables_to_drop {
                database_core.relation_ref_count.remove(&table.id);
            }
            for source in &sources_to_drop {
                database_core.relation_ref_count.remove(&source.id);
            }
            for view in &views_to_drop {
                database_core.relation_ref_count.remove(&view.id);
            }
            for connection in &connections_to_drop {
                database_core.connection_ref_count.remove(&connection.id);
            }
            for user in users_need_update {
                self.notify_frontend(Operation::Update, Info::User(user))
                    .await;
            }

            // Frontend will drop cache of schema and table in the database.
            let version = self
                .notify_frontend(Operation::Delete, Info::Database(database))
                .await;

            let streaming_job_deleted_ids = tables_to_drop
                .into_iter()
                .filter(|table| valid_table_name(&table.name))
                .map(|table| StreamingJobId::new(table.id))
                .chain(sources_to_drop.iter().filter_map(|source| {
                    source
                        .info
                        .as_ref()
                        .and_then(|info| info.is_shared().then(|| StreamingJobId::new(source.id)))
                }))
                .chain(
                    sinks_to_drop
                        .into_iter()
                        .map(|sink| StreamingJobId::new(sink.id)),
                )
                .collect_vec();
            let source_deleted_ids = sources_to_drop
                .into_iter()
                .map(|source| source.id)
                .collect_vec();

            Ok((
                version,
                streaming_job_deleted_ids,
                source_deleted_ids,
                connections_dropped,
            ))
        } else {
            Err(MetaError::catalog_id_not_found("database", database_id))
        }
    }

    pub async fn create_secret(
        &self,
        secret: Secret,
        secret_plain_payload: Vec<u8>,
    ) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let user_core = &mut core.user;
        database_core.ensure_database_id(secret.database_id)?;
        database_core.ensure_schema_id(secret.schema_id)?;
        #[cfg(not(test))]
        user_core.ensure_user_id(secret.owner)?;
        let key = (
            secret.database_id as DatabaseId,
            secret.schema_id as SchemaId,
            secret.name.clone(),
        );
        database_core.check_secret_name_duplicated(&key)?;

        let secret_id = secret.id;
        let mut secret_entry = BTreeMapTransaction::new(&mut database_core.secrets);

        secret_entry.insert(secret_id, secret.to_owned());
        commit_meta!(self, secret_entry)?;

        user_core.increase_ref(secret.owner);

        // Notify the compute and frontend node plain secret
        let mut secret_plain = secret;
        secret_plain.value.clone_from(&secret_plain_payload);

        LocalSecretManager::global().add_secret(secret_id, secret_plain_payload);
        self.env
            .notification_manager()
            .notify_compute_without_version(Operation::Add, Info::Secret(secret_plain.clone()));

        let version = self
            .notify_frontend(Operation::Add, Info::Secret(secret_plain))
            .await;

        Ok(version)
    }

    pub async fn drop_secret(&self, secret_id: SecretId) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let user_core = &mut core.user;
        let mut secrets = BTreeMapTransaction::new(&mut database_core.secrets);

        match database_core.secret_ref_count.get(&secret_id) {
            Some(ref_count) => {
                let secret_name = secrets
                    .get(&secret_id)
                    .ok_or_else(|| MetaError::catalog_id_not_found("connection", secret_id))?
                    .name
                    .clone();
                Err(MetaError::permission_denied(format!(
                    "Fail to delete secret {} because {} other relation(s) depend on it",
                    secret_name, ref_count
                )))
            }
            None => {
                let secret = secrets
                    .remove(secret_id)
                    .ok_or_else(|| anyhow!("secret not found"))?;

                commit_meta!(self, secrets)?;
                user_core.decrease_ref(secret.owner);

                LocalSecretManager::global().remove_secret(secret.id);
                self.env
                    .notification_manager()
                    .notify_compute_without_version(
                        Operation::Delete,
                        Info::Secret(secret.clone()),
                    );

                let version = self
                    .notify_frontend(Operation::Delete, Info::Secret(secret))
                    .await;
                Ok(version)
            }
        }
    }

    pub async fn create_connection(
        &self,
        connection: Connection,
    ) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let user_core = &mut core.user;
        database_core.ensure_database_id(connection.database_id)?;
        database_core.ensure_schema_id(connection.schema_id)?;
        #[cfg(not(test))]
        user_core.ensure_user_id(connection.owner)?;

        let key = (
            connection.database_id,
            connection.schema_id,
            connection.name.clone(),
        );
        database_core.check_connection_name_duplicated(&key)?;

        let conn_id = connection.id;
        let mut connections = BTreeMapTransaction::new(&mut database_core.connections);
        connections.insert(conn_id, connection.to_owned());
        commit_meta!(self, connections)?;

        user_core.increase_ref(connection.owner);

        let version = self
            .notify_frontend(Operation::Add, Info::Connection(connection))
            .await;
        Ok(version)
    }

    pub async fn drop_connection(
        &self,
        conn_id: ConnectionId,
    ) -> MetaResult<(NotificationVersion, Connection)> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        database_core.ensure_connection_id(conn_id)?;

        let user_core = &mut core.user;
        let mut connections = BTreeMapTransaction::new(&mut database_core.connections);

        match database_core.connection_ref_count.get(&conn_id) {
            Some(ref_count) => {
                let connection_name = connections
                    .get(&conn_id)
                    .ok_or_else(|| MetaError::catalog_id_not_found("connection", conn_id))?
                    .name
                    .clone();
                Err(MetaError::permission_denied(format!(
                    "Fail to delete connection {} because {} other relation(s) depend on it",
                    connection_name, ref_count
                )))
            }
            None => {
                let connection = connections
                    .remove(conn_id)
                    .ok_or_else(|| MetaError::catalog_id_not_found("connection", conn_id))?;

                commit_meta!(self, connections)?;
                user_core.decrease_ref(connection.owner);

                let version = self
                    .notify_frontend(Operation::Delete, Info::Connection(connection.clone()))
                    .await;
                Ok((version, connection))
            }
        }
    }

    pub async fn create_schema(&self, schema: &Schema) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let user_core = &mut core.user;
        database_core.ensure_database_id(schema.database_id)?;
        database_core.check_schema_duplicated(&(schema.database_id, schema.name.clone()))?;
        #[cfg(not(test))]
        user_core.ensure_user_id(schema.owner)?;

        let mut schemas = BTreeMapTransaction::new(&mut database_core.schemas);
        schemas.insert(schema.id, schema.clone());
        commit_meta!(self, schemas)?;

        user_core.increase_ref(schema.owner);

        let version = self
            .notify_frontend(Operation::Add, Info::Schema(schema.to_owned()))
            .await;

        Ok(version)
    }

    pub async fn drop_schema(&self, schema_id: SchemaId) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let user_core = &mut core.user;
        if !database_core.schemas.contains_key(&schema_id) {
            return Err(MetaError::catalog_id_not_found("schema", schema_id));
        }
        if database_core.has_creation_in_schema(schema_id) {
            return Err(MetaError::permission_denied(
                "Some relations are creating in the target schema, try again later",
            ));
        }
        if !database_core.schema_is_empty(schema_id) {
            return Err(MetaError::permission_denied(
                "The schema is not empty, try dropping them first",
            ));
        }
        let mut schemas = BTreeMapTransaction::new(&mut database_core.schemas);
        let mut users = BTreeMapTransaction::new(&mut user_core.user_info);
        let schema = schemas.remove(schema_id).unwrap();

        let users_need_update =
            Self::update_user_privileges(&mut users, &[Object::SchemaId(schema_id)]);

        commit_meta!(self, schemas, users)?;

        user_core.decrease_ref(schema.owner);

        for user in users_need_update {
            self.notify_frontend(Operation::Update, Info::User(user))
                .await;
        }
        let version = self
            .notify_frontend(Operation::Delete, Info::Schema(schema))
            .await;

        Ok(version)
    }

    pub async fn create_view(&self, view: &View) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let user_core = &mut core.user;
        database_core.ensure_database_id(view.database_id)?;
        database_core.ensure_schema_id(view.schema_id)?;
        for dependent_id in &view.dependent_relations {
            // TODO(zehua): refactor when using SourceId.
            database_core.ensure_table_view_or_source_id(dependent_id)?;
        }
        let key = (view.database_id, view.schema_id, view.name.clone());
        database_core.check_relation_name_duplicated(&key)?;
        #[cfg(not(test))]
        user_core.ensure_user_id(view.owner)?;

        let mut views = BTreeMapTransaction::new(&mut database_core.views);
        views.insert(view.id, view.clone());
        commit_meta!(self, views)?;

        user_core.increase_ref(view.owner);

        for &dependent_relation_id in &view.dependent_relations {
            database_core.increase_relation_ref_count(dependent_relation_id);
        }

        let version = self
            .notify_frontend_relation_info(Operation::Add, RelationInfo::View(view.to_owned()))
            .await;

        Ok(version)
    }

    pub async fn create_function(&self, function: &Function) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let user_core = &mut core.user;
        database_core.ensure_database_id(function.database_id)?;
        database_core.ensure_schema_id(function.schema_id)?;
        database_core.check_function_duplicated(&(
            function.database_id,
            function.schema_id,
            function.name.clone(),
            function.arg_types.clone(),
        ))?;

        #[cfg(not(test))]
        user_core.ensure_user_id(function.owner)?;

        tracing::debug!("create function: {:?}", function);
        let mut functions = BTreeMapTransaction::new(&mut database_core.functions);
        functions.insert(function.id, function.clone());
        commit_meta!(self, functions)?;

        user_core.increase_ref(function.owner);

        let version = self
            .notify_frontend(Operation::Add, Info::Function(function.to_owned()))
            .await;

        Ok(version)
    }

    pub async fn drop_function(&self, function_id: FunctionId) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let user_core = &mut core.user;
        let mut functions = BTreeMapTransaction::new(&mut database_core.functions);
        let mut users = BTreeMapTransaction::new(&mut user_core.user_info);

        let function = functions
            .remove(function_id)
            .ok_or_else(|| MetaError::catalog_id_not_found("function", function_id))?;

        let objects = &[Object::FunctionId(function_id)];
        let users_need_update = Self::update_user_privileges(&mut users, objects);

        commit_meta!(self, functions, users)?;

        user_core.decrease_ref(function.owner);

        for user in users_need_update {
            self.notify_frontend(Operation::Update, Info::User(user))
                .await;
        }

        let version = self
            .notify_frontend(Operation::Delete, Info::Function(function))
            .await;

        Ok(version)
    }

    /// Marks current relation as "creating" and add reference count to dependent relations.
    /// And persists internal tables for background DDL progress tracking.
    pub async fn start_create_stream_job_procedure(
        &self,
        stream_job: &StreamingJob,
        internal_tables: Vec<Table>,
    ) -> MetaResult<()> {
        match stream_job {
            StreamingJob::MaterializedView(table) => {
                self.start_create_materialized_view_procedure(table, internal_tables)
                    .await
            }
            StreamingJob::Sink(sink, _) => self.start_create_sink_procedure(sink).await,
            StreamingJob::Index(index, index_table) => {
                self.start_create_index_procedure(index, index_table).await
            }
            StreamingJob::Table(source, table, ..) => {
                if let Some(source) = source {
                    self.start_create_table_procedure_with_source(source, table)
                        .await
                } else {
                    self.start_create_table_procedure(table).await
                }
            }
            StreamingJob::Source(source) => self.start_create_source_procedure(source).await,
        }
    }

    pub async fn mark_creating_tables(&self, creating_tables: &[Table]) {
        let core = &mut self.core.lock().await.database;
        core.mark_creating_tables(creating_tables);
        for table in creating_tables {
            self.notify_hummock_and_compactor_relation_info(
                Operation::Add,
                RelationInfo::Table(table.to_owned()),
            )
            .await;
        }
    }

    pub async fn unmark_creating_tables(&self, creating_table_ids: &[TableId], need_notify: bool) {
        let core = &mut self.core.lock().await.database;
        core.unmark_creating_tables(creating_table_ids);
        if need_notify {
            for table_id in creating_table_ids {
                // TODO: use group notification?
                self.notify_hummock_and_compactor_relation_info(
                    Operation::Delete,
                    RelationInfo::Table(Table {
                        id: *table_id,
                        ..Default::default()
                    }),
                )
                .await;
            }
        }
    }

    async fn notify_hummock_and_compactor_relation_info(
        &self,
        operation: Operation,
        relation_info: RelationInfo,
    ) {
        self.env
            .notification_manager()
            .notify_hummock_relation_info(operation, relation_info.clone())
            .await;

        self.env
            .notification_manager()
            .notify_compactor_relation_info(operation, relation_info)
            .await;
    }

    /// This is used for both `CREATE TABLE`
    pub async fn start_create_table_procedure(&self, table: &Table) -> MetaResult<()> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let user_core = &mut core.user;
        database_core.ensure_database_id(table.database_id)?;
        database_core.ensure_schema_id(table.schema_id)?;
        for dependent_id in &table.dependent_relations {
            database_core.ensure_table_view_or_source_id(dependent_id)?;
        }
        #[cfg(not(test))]
        user_core.ensure_user_id(table.owner)?;
        let key = (table.database_id, table.schema_id, table.name.clone());

        database_core.check_relation_name_duplicated(&key)?;

        if database_core.has_in_progress_creation(&key) {
            bail!("The table is being created");
        } else {
            database_core.mark_creating(&key);
            database_core.mark_creating_streaming_job(table.id, key);
            for &dependent_relation_id in &table.dependent_relations {
                database_core.increase_relation_ref_count(dependent_relation_id);
            }
            user_core.increase_ref(table.owner);
            Ok(())
        }
    }

    /// This is used for `CREATE MATERIALIZED VIEW`.
    pub async fn start_create_materialized_view_procedure(
        &self,
        table: &Table,
        internal_tables: Vec<Table>,
    ) -> MetaResult<()> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let user_core = &mut core.user;
        database_core.ensure_database_id(table.database_id)?;
        database_core.ensure_schema_id(table.schema_id)?;
        for dependent_id in &table.dependent_relations {
            database_core.ensure_table_view_or_source_id(dependent_id)?;
        }
        #[cfg(not(test))]
        user_core.ensure_user_id(table.owner)?;
        let key = (table.database_id, table.schema_id, table.name.clone());

        database_core.check_relation_name_duplicated(&key)?;

        let mut tables = BTreeMapTransaction::new(&mut database_core.tables);
        assert!(
            !tables.contains_key(&table.id),
            "table must not already exist in meta"
        );
        for table in &internal_tables {
            tables.insert(table.id, table.clone());
        }
        tables.insert(table.id, table.clone());
        commit_meta!(self, tables)?;

        for &dependent_relation_id in &table.dependent_relations {
            database_core.increase_relation_ref_count(dependent_relation_id);
        }
        user_core.increase_ref(table.owner);
        let _version = self
            .notify_frontend(
                Operation::Add,
                Info::RelationGroup(RelationGroup {
                    relations: vec![Relation {
                        relation_info: RelationInfo::Table(table.to_owned()).into(),
                    }]
                    .into_iter()
                    .chain(internal_tables.into_iter().map(|internal_table| Relation {
                        relation_info: RelationInfo::Table(internal_table).into(),
                    }))
                    .collect_vec(),
                }),
            )
            .await;
        Ok(())
    }

    fn check_table_creating(tables: &BTreeMap<TableId, Table>, table: &Table) -> MetaResult<()> {
        return if let Some(t) = tables.get(&table.id) {
            assert_eq!(t.get_stream_job_status(), Ok(StreamJobStatus::Creating));
            Ok(())
        } else {
            // If the table does not exist, it should be created in Foreground and cleaned during recovery in some cases.
            assert_eq!(table.create_type(), CreateType::Foreground);
            Err(anyhow!("failed to create table during recovery").into())
        };
    }

    pub async fn assert_tables_deleted(&self, table_ids: Vec<TableId>) {
        let core = self.core.lock().await;
        let tables = &core.database.tables;
        for id in table_ids {
            assert_eq!(tables.get(&id), None,)
        }
    }

    /// We clean the following tables:
    /// 1. Those which belonged to incomplete Foreground jobs.
    /// 2. Those which did not persist their table fragments, we can't recover these.
    /// 3. Those which were only initialized, but not actually running yet.
    /// 4. From 2, since we don't have internal table ids from the fragments,
    ///    we can detect hanging table ids by just finding all internal ids
    ///    with:
    ///    1. `stream_job_status` = CREATING
    ///    2. Not belonging to a background stream job.
    ///
    ///    Clean up these hanging tables by the id.
    pub async fn clean_dirty_tables(&self, fragment_manager: FragmentManagerRef) -> MetaResult<()> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let creating_tables: Vec<Table> = database_core.list_persisted_creating_tables();
        tracing::debug!(
            "creating_tables ids: {:#?}",
            creating_tables.iter().map(|t| t.id).collect_vec()
        );
        let mut reserved_internal_tables = HashSet::new();
        let mut tables_to_clean = vec![];
        let mut internal_tables_to_clean = vec![];
        for table in creating_tables {
            tracing::trace!(
                "checking table {} definition: {}, create_type: {:#?}, table_type: {:#?}",
                table.id,
                table.definition,
                table.get_create_type().unwrap_or(CreateType::Foreground),
                table.get_table_type().unwrap(),
            );
            // 1. Incomplete Foreground jobs
            if table.create_type == CreateType::Foreground as i32
                && table.table_type != TableType::Internal as i32
            // || table.create_type == CreateType::Unspecified as i32
            {
                tracing::debug!("cleaning table_id for foreground: {:#?}", table.id);
                tables_to_clean.push(table);
                continue;
            }
            if table.table_type == TableType::Internal as i32 {
                internal_tables_to_clean.push(table);
                continue;
            }

            // 2. No table fragments
            assert_ne!(table.table_type, TableType::Internal as i32);
            match fragment_manager
                .select_table_fragments_by_table_id(&table.id.into())
                .await
            {
                Err(e) => {
                    if e.is_fragment_not_found() {
                        tracing::debug!("cleaning table_id for no fragments: {:#?}", table.id);
                        tables_to_clean.push(table);
                        continue;
                    } else {
                        return Err(e);
                    }
                }
                Ok(fragment) => {
                    let fragment: TableFragments = fragment;
                    // 3. For those in initial state (i.e. not running / created),
                    // we should purge them.
                    if fragment.is_initial() {
                        tracing::debug!("cleaning table_id with initial state: {:#?}", table.id);
                        tables_to_clean.push(table);
                        continue;
                    } else {
                        assert_eq!(table.create_type, CreateType::Background as i32);
                        // 4. Get all the corresponding internal tables, the rest we can purge.
                        for id in fragment.internal_table_ids() {
                            reserved_internal_tables.insert(id);
                        }
                        continue;
                    }
                }
            }
        }
        for t in internal_tables_to_clean {
            if !reserved_internal_tables.contains(&t.id) {
                tracing::debug!(
                    "cleaning table_id for internal tables not reserved: {:#?}",
                    t.id
                );
                tables_to_clean.push(t);
            }
        }

        let mut tables_to_update = vec![];
        for table in database_core.tables.values() {
            if table.incoming_sinks.is_empty() {
                continue;
            }

            if table
                .incoming_sinks
                .iter()
                .all(|sink_id| database_core.sinks.contains_key(sink_id))
            {
                continue;
            }

            let mut table = table.clone();
            table
                .incoming_sinks
                .retain(|sink_id| database_core.sinks.contains_key(sink_id));
            tables_to_update.push(table);
        }

        let tables = &mut database_core.tables;
        let mut tables = BTreeMapTransaction::new(tables);
        for table in &tables_to_clean {
            let table_id = table.id;
            tracing::debug!("cleaning table_id: {}", table_id);
            let table = tables.remove(table_id);
            assert!(table.is_some(), "table_id {} missing", table_id)
        }

        for table in &tables_to_update {
            let table_id = table.id;
            if tables.contains_key(&table_id) {
                tracing::debug!("updating sink target table_id: {}", table_id);
                tables.insert(table_id, table.clone());
            }
        }

        commit_meta!(self, tables)?;

        if !tables_to_update.is_empty() {
            let _ = self
                .notify_frontend(
                    Operation::Update,
                    Info::RelationGroup(RelationGroup {
                        relations: tables_to_update
                            .into_iter()
                            .map(|table| Relation {
                                relation_info: RelationInfo::Table(table).into(),
                            })
                            .collect(),
                    }),
                )
                .await;
        }

        // Note that `tables_to_clean` doesn't include sink/index/table_with_source creation,
        // because their states are not persisted in the first place, see `start_create_stream_job_procedure`.
        let event_logs = tables_to_clean
            .iter()
            .filter_map(|t| {
                if t.table_type == TableType::Internal as i32 {
                    return None;
                }
                let event = risingwave_pb::meta::event_log::EventDirtyStreamJobClear {
                    id: t.id,
                    name: t.name.to_owned(),
                    definition: t.definition.to_owned(),
                    error: "clear during recovery".to_string(),
                };
                Some(risingwave_pb::meta::event_log::Event::DirtyStreamJobClear(
                    event,
                ))
            })
            .collect_vec();
        if !event_logs.is_empty() {
            self.env.event_log_manager_ref().add_event_logs(event_logs);
        }

        let user_core = &mut core.user;
        for table in &tables_to_clean {
            // If table type is internal, no need to update the ref count OR
            // user ref count.
            if table.table_type != TableType::Internal as i32 {
                // Recovered when init database manager.
                for relation_id in &table.dependent_relations {
                    database_core.decrease_relation_ref_count(*relation_id);
                }
                // Recovered when init user manager.
                tracing::debug!("decrease ref for {}", table.id);
                user_core.decrease_ref(table.owner);
            }
        }
        // Notify frontend of cleaned tables.
        let relations = tables_to_clean
            .into_iter()
            .map(|table| Relation {
                relation_info: RelationInfo::Table(table).into(),
            })
            .collect_vec();
        self.notify_frontend(
            Operation::Delete,
            Info::RelationGroup(RelationGroup { relations }),
        )
        .await;
        Ok(())
    }

    /// `finish_stream_job` finishes a stream job and clean some states.
    pub async fn finish_stream_job(
        &self,
        mut stream_job: StreamingJob,
        internal_tables: Vec<Table>,
    ) -> MetaResult<()> {
        // 1. finish procedure.
        let mut creating_internal_table_ids = internal_tables.iter().map(|t| t.id).collect_vec();

        // Update the corresponding 'created_at' field.
        stream_job.mark_created();

        let (version, table_id) = match stream_job {
            StreamingJob::MaterializedView(table) => {
                creating_internal_table_ids.push(table.id);
                let table_id = table.id;
                let version = self
                    .finish_create_materialized_view_procedure(internal_tables, table)
                    .await?;
                (version, table_id)
            }
            StreamingJob::Sink(sink, target_table) => {
                let sink_id = sink.id;

                let mut version = self
                    .finish_create_sink_procedure(internal_tables, sink)
                    .await?;

                if let Some((table, source)) = target_table {
                    version = self
                        .finish_replace_table_procedure(
                            &source,
                            &table,
                            None,
                            Some(sink_id),
                            None,
                            vec![],
                        )
                        .await?;
                }

                (version, sink_id)
            }
            StreamingJob::Table(source, table, ..) => {
                creating_internal_table_ids.push(table.id);
                let table_id = table.id;
                let version = if let Some(source) = source {
                    self.finish_create_table_procedure_with_source(source, table, internal_tables)
                        .await?
                } else {
                    self.finish_create_table_procedure(internal_tables, table)
                        .await?
                };
                (version, table_id)
            }
            StreamingJob::Index(index, table) => {
                creating_internal_table_ids.push(table.id);
                let table_id = table.id;
                let version = self
                    .finish_create_index_procedure(internal_tables, index, table)
                    .await?;
                (version, table_id)
            }
            StreamingJob::Source(source) => {
                let table_id = source.id;
                let version = self
                    .finish_create_source_procedure(source, internal_tables)
                    .await?;
                (version, table_id)
            }
        };

        // 2. unmark creating tables.
        self.unmark_creating_tables(&creating_internal_table_ids, false)
            .await;

        // 3. notify create streaming job finish
        self.core.lock().await.notify_finish(table_id, version);

        Ok(())
    }

    /// This is used for `CREATE TABLE`.
    pub async fn finish_create_table_procedure(
        &self,
        mut internal_tables: Vec<Table>,
        mut table: Table,
    ) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let mut tables = BTreeMapTransaction::new(&mut database_core.tables);
        let key = (table.database_id, table.schema_id, table.name.clone());
        assert!(
            !tables.contains_key(&table.id)
                && database_core.in_progress_creation_tracker.contains(&key),
            "table must be in creating procedure"
        );
        database_core.in_progress_creation_tracker.remove(&key);
        database_core
            .in_progress_creating_streaming_job
            .remove(&table.id);

        table.stream_job_status = PbStreamJobStatus::Created.into();
        tables.insert(table.id, table.clone());
        for table in &mut internal_tables {
            table.stream_job_status = PbStreamJobStatus::Created.into();
            tables.insert(table.id, table.clone());
        }
        commit_meta!(self, tables)?;

        tracing::debug!(id = ?table.id, "notifying frontend");
        let version = self
            .notify_frontend(
                Operation::Add,
                Info::RelationGroup(RelationGroup {
                    relations: vec![Relation {
                        relation_info: RelationInfo::Table(table.to_owned()).into(),
                    }]
                    .into_iter()
                    .chain(internal_tables.into_iter().map(|internal_table| Relation {
                        relation_info: RelationInfo::Table(internal_table).into(),
                    }))
                    .collect_vec(),
                }),
            )
            .await;

        Ok(version)
    }

    /// This is used for `CREATE MATERIALIZED VIEW`.
    pub async fn finish_create_materialized_view_procedure(
        &self,
        mut internal_tables: Vec<Table>,
        mut table: Table,
    ) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let tables = &mut database_core.tables;
        if cfg!(not(test)) {
            Self::check_table_creating(tables, &table)?;
        }
        let mut tables = BTreeMapTransaction::new(tables);

        table.stream_job_status = PbStreamJobStatus::Created.into();
        tables.insert(table.id, table.clone());
        for table in &mut internal_tables {
            table.stream_job_status = PbStreamJobStatus::Created.into();
            tables.insert(table.id, table.clone());
        }
        commit_meta!(self, tables)?;

        tracing::debug!(id = ?table.id, "notifying frontend");
        let version = self
            .notify_frontend(
                Operation::Update,
                Info::RelationGroup(RelationGroup {
                    relations: vec![Relation {
                        relation_info: RelationInfo::Table(table.to_owned()).into(),
                    }]
                    .into_iter()
                    .chain(internal_tables.into_iter().map(|internal_table| Relation {
                        relation_info: RelationInfo::Table(internal_table).into(),
                    }))
                    .collect_vec(),
                }),
            )
            .await;

        Ok(version)
    }

    /// Used to cleanup `CREATE MATERIALIZED VIEW` state in stream manager.
    /// It is required because failure may not necessarily happen in barrier,
    /// e.g. when cordon nodes.
    /// and we still need some way to cleanup the state.
    ///
    /// Returns false if `table_id` is not found.
    pub async fn cancel_create_materialized_view_procedure(
        &self,
        table_id: TableId,
        internal_table_ids: Vec<TableId>,
    ) -> MetaResult<bool> {
        let core = &mut self.core.lock().await;
        let database_core = &mut core.database;
        let tables = &mut database_core.tables;
        let Some(table) = tables.get(&table_id).cloned() else {
            tracing::warn!(
                "table_id {} missing when attempting to cancel job, could be cleaned on recovery",
                table_id
            );
            return Ok(false);
        };
        let mut internal_tables = vec![];
        for internal_table_id in &internal_table_ids {
            if let Some(table) = tables.get(internal_table_id) {
                internal_tables.push(table.clone());
            }
        }

        // `Unspecified` maps to Created state, due to backwards compatibility.
        // `Created` states should not be cancelled.
        if table
            .get_stream_job_status()
            .unwrap_or(StreamJobStatus::Created)
            != StreamJobStatus::Creating
        {
            return Err(MetaError::invalid_parameter(format!(
                "table is not in creating state id={:#?}",
                table_id
            )));
        }

        tracing::trace!("cleanup tables for {}", table.id);
        let mut table_ids = vec![table.id];
        table_ids.extend(internal_table_ids);

        let tables = &mut database_core.tables;
        let mut tables = BTreeMapTransaction::new(tables);
        for table_id in table_ids {
            let res = tables.remove(table_id);
            assert!(res.is_some(), "table_id {} missing", table_id);
        }
        commit_meta!(self, tables)?;

        {
            let user_core = &mut core.user;
            user_core.decrease_ref(table.owner);
        }

        {
            let database_core = &mut core.database;
            for &dependent_relation_id in &table.dependent_relations {
                database_core.decrease_relation_ref_count(dependent_relation_id);
            }
        }

        for tx in core
            .database
            .creating_table_finish_notifier
            .remove(&table_id)
            .into_iter()
            .flatten()
        {
            let _ = tx.send(Err(MetaError::cancelled(format!(
                "materialized view {table_id} has been cancelled"
            ))));
        }

        // FIXME(kwannoel): Propagate version to fe
        let _version = self
            .notify_frontend(
                Operation::Delete,
                Info::RelationGroup(RelationGroup {
                    relations: vec![Relation {
                        relation_info: RelationInfo::Table(table.to_owned()).into(),
                    }]
                    .into_iter()
                    .chain(internal_tables.into_iter().map(|internal_table| Relation {
                        relation_info: RelationInfo::Table(internal_table).into(),
                    }))
                    .collect_vec(),
                }),
            )
            .await;

        Ok(true)
    }

    /// Used to cleanup `CREATE TABLE` state in stream manager.
    pub async fn cancel_create_table_procedure(&self, table: &Table) {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let user_core = &mut core.user;
        let key = (table.database_id, table.schema_id, table.name.clone());
        assert!(
            !database_core.tables.contains_key(&table.id)
                && database_core.has_in_progress_creation(&key),
            "table must be in creating procedure"
        );
        database_core.unmark_creating(&key);
        database_core.unmark_creating_streaming_job(table.id);
        for &dependent_relation_id in &table.dependent_relations {
            database_core.decrease_relation_ref_count(dependent_relation_id);
        }
        user_core.decrease_ref(table.owner);
    }

    /// return id of streaming jobs in the database which need to be dropped by stream manager.
    pub async fn drop_relation(
        &self,
        relation: RelationIdEnum,
        fragment_manager: FragmentManagerRef,
        drop_mode: DropMode,
    ) -> MetaResult<(NotificationVersion, Vec<StreamingJobId>)> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let user_core = &mut core.user;
        let mut indexes = BTreeMapTransaction::new(&mut database_core.indexes);
        let mut tables = BTreeMapTransaction::new(&mut database_core.tables);
        let mut sources = BTreeMapTransaction::new(&mut database_core.sources);
        let mut sinks = BTreeMapTransaction::new(&mut database_core.sinks);
        let mut subscriptions = BTreeMapTransaction::new(&mut database_core.subscriptions);
        let mut views = BTreeMapTransaction::new(&mut database_core.views);
        let mut users = BTreeMapTransaction::new(&mut user_core.user_info);

        // The deque holds all the relations we need to drop.
        // As we traverse the relation DAG,
        // more relations will be added and popped from this.
        let mut deque = VecDeque::new();

        // Relation dependencies is a DAG rather than a tree, so we need to use `HashSet` instead of
        // `Vec` to record ids.
        //         Sink
        //          |
        //        MView
        //        /   \
        //       View  |
        //        \   /
        //        Table

        // `all_table_ids` are materialized view ids, table ids and index table ids.
        let mut all_table_ids: HashSet<TableId> = HashSet::default();
        let mut all_internal_table_ids: HashSet<TableId> = HashSet::default();
        let mut all_index_ids: HashSet<IndexId> = HashSet::default();
        let mut all_sink_ids: HashSet<SinkId> = HashSet::default();
        let mut all_subscription_ids: HashSet<SubscriptionId> = HashSet::default();
        let mut all_source_ids: HashSet<SourceId> = HashSet::default();
        let mut all_view_ids: HashSet<ViewId> = HashSet::default();
        let mut all_streaming_job_source_ids: HashSet<SourceId> = HashSet::default();

        let relations_depend_on = |relation_id: RelationId| -> Vec<RelationInfo> {
            let tables_depend_on = tables
                .tree_ref()
                .iter()
                .filter_map(|(_, table)| {
                    if table.dependent_relations.contains(&relation_id) {
                        Some(RelationInfo::Table(table.clone()))
                    } else {
                        None
                    }
                })
                .collect_vec();

            let sinks_depend_on = sinks
                .tree_ref()
                .iter()
                .filter_map(|(_, sink)| {
                    if sink.dependent_relations.contains(&relation_id) {
                        Some(RelationInfo::Sink(sink.clone()))
                    } else {
                        None
                    }
                })
                .collect_vec();

            let subscriptions_depend_on = subscriptions
                .tree_ref()
                .iter()
                .filter_map(|(_, subscription)| {
                    if subscription.dependent_table_id == relation_id {
                        Some(RelationInfo::Subscription(subscription.clone()))
                    } else {
                        None
                    }
                })
                .collect_vec();

            let views_depend_on = views
                .tree_ref()
                .iter()
                .filter_map(|(_, view)| {
                    if view.dependent_relations.contains(&relation_id) {
                        Some(RelationInfo::View(view.clone()))
                    } else {
                        None
                    }
                })
                .collect_vec();

            // We don't need to output indexes because they have been handled by tables.
            tables_depend_on
                .into_iter()
                .chain(sinks_depend_on)
                .chain(subscriptions_depend_on)
                .chain(views_depend_on)
                .collect()
        };

        // Initial push into deque.
        match relation {
            RelationIdEnum::Table(table_id) => {
                let table = tables.get(&table_id).cloned();
                if let Some(table) = table {
                    for incoming_sink in &table.incoming_sinks {
                        let sink = sinks.get(incoming_sink).cloned();
                        if let Some(sink) = sink {
                            deque.push_back(RelationInfo::Sink(sink));
                        }
                    }

                    deque.push_back(RelationInfo::Table(table));
                } else {
                    bail!("table doesn't exist");
                }
            }
            RelationIdEnum::Index(index_id) => {
                let index = indexes.get(&index_id).cloned();
                if let Some(index) = index {
                    deque.push_back(RelationInfo::Index(index));
                } else {
                    bail!("index doesn't exist");
                }
            }
            RelationIdEnum::Sink(sink_id) => {
                let sink = sinks.get(&sink_id).cloned();
                if let Some(sink) = sink {
                    deque.push_back(RelationInfo::Sink(sink));
                } else {
                    bail!("sink doesn't exist");
                }
            }
            RelationIdEnum::Subscription(subscription_id) => {
                let subscription = subscriptions.get(&subscription_id).cloned();
                if let Some(subscription) = subscription {
                    deque.push_back(RelationInfo::Subscription(subscription));
                } else {
                    bail!("subscription doesn't exist");
                }
            }
            RelationIdEnum::View(view_id) => {
                let view = views.get(&view_id).cloned();
                if let Some(view) = view {
                    deque.push_back(RelationInfo::View(view));
                } else {
                    bail!("source doesn't exist");
                }
            }
            RelationIdEnum::Source(source_id) => {
                let source = sources.get(&source_id).cloned();
                if let Some(source) = source {
                    deque.push_back(RelationInfo::Source(source));
                } else {
                    bail!("view doesn't exist");
                }
            }
        }

        // Drop cascade loop
        while let Some(relation_info) = deque.pop_front() {
            match relation_info {
                RelationInfo::Table(table) => {
                    let table_id: TableId = table.id;
                    if !all_table_ids.insert(table_id) {
                        continue;
                    }

                    let table_fragments = fragment_manager
                        .select_table_fragments_by_table_id(&table_id.into())
                        .await?;

                    all_internal_table_ids.extend(table_fragments.internal_table_ids());

                    let (index_ids, index_table_ids): (Vec<_>, Vec<_>) = indexes
                        .tree_ref()
                        .iter()
                        .filter(|(_, index)| index.primary_table_id == table_id)
                        .map(|(index_id, index)| (*index_id, index.index_table_id))
                        .unzip();

                    all_index_ids.extend(index_ids.iter().cloned());
                    all_table_ids.extend(index_table_ids.iter().cloned());

                    for index_table_id in &index_table_ids {
                        let internal_table_ids = fragment_manager
                            .select_table_fragments_by_table_id(&(index_table_id.into()))
                            .await
                            .map(|fragments| fragments.internal_table_ids())
                            .unwrap_or_default();

                        // 1 should be used by table scan.
                        if internal_table_ids.len() == 1 {
                            all_internal_table_ids.insert(internal_table_ids[0]);
                        } else {
                            // backwards compatibility with indexes
                            // without backfill state persisted.
                            assert_eq!(internal_table_ids.len(), 0);
                        }
                    }

                    let index_tables = index_table_ids
                        .iter()
                        .map(|index_table_id| tables.get(index_table_id).cloned().unwrap())
                        .collect_vec();

                    for index_table in &index_tables {
                        if let Some(ref_count) =
                            database_core.relation_ref_count.get(&index_table.id)
                        {
                            // Other relations depend on it.
                            match drop_mode {
                                DropMode::Restrict => {
                                    return Err(MetaError::permission_denied(format!(
                                        "Fail to delete index table `{}` because {} other relation(s) depend on it",
                                        index_table.name, ref_count
                                    )));
                                }
                                DropMode::Cascade => {
                                    for relation_info in
                                        relations_depend_on(index_table.id as RelationId)
                                    {
                                        deque.push_back(relation_info);
                                    }
                                }
                            }
                        }
                    }

                    if let Some(ref_count) =
                        database_core.relation_ref_count.get(&table_id).cloned()
                    {
                        if ref_count > index_ids.len() {
                            // Other relations depend on it.
                            match drop_mode {
                                DropMode::Restrict => {
                                    return Err(MetaError::permission_denied(format!(
                                        "Fail to delete table `{}` because {} other relation(s) depend on it",
                                        table.name, ref_count
                                    )));
                                }
                                DropMode::Cascade => {
                                    for relation_info in relations_depend_on(table.id as RelationId)
                                    {
                                        if let RelationInfo::Table(t) = &relation_info {
                                            // Filter table belongs to index_table_ids.
                                            if !index_table_ids.contains(&t.id) {
                                                deque.push_back(relation_info);
                                            }
                                        } else {
                                            deque.push_back(relation_info);
                                        }
                                    }
                                }
                            }
                        }
                    }

                    if let Some(OptionalAssociatedSourceId::AssociatedSourceId(
                        associated_source_id,
                    )) = table.optional_associated_source_id
                    {
                        all_source_ids.insert(associated_source_id);
                    }
                }
                RelationInfo::Index(index) => {
                    if !all_index_ids.insert(index.id) {
                        continue;
                    }
                    all_table_ids.insert(index.index_table_id);

                    let internal_table_ids = fragment_manager
                        .select_table_fragments_by_table_id(&(index.index_table_id.into()))
                        .await
                        .map(|fragments| fragments.internal_table_ids())
                        .unwrap_or_default();

                    // 1 should be used by table scan.
                    if internal_table_ids.len() == 1 {
                        all_internal_table_ids.insert(internal_table_ids[0]);
                    } else {
                        // backwards compatibility with indexes
                        // without backfill state persisted.
                        assert_eq!(internal_table_ids.len(), 0);
                    }

                    if let Some(ref_count) = database_core
                        .relation_ref_count
                        .get(&index.index_table_id)
                        .cloned()
                    {
                        if ref_count > 0 {
                            // Other relations depend on it.
                            match drop_mode {
                                DropMode::Restrict => {
                                    return Err(MetaError::permission_denied(format!(
                                        "Fail to delete index `{}` because {} other relation(s) depend on it",
                                        index.name, ref_count
                                    )));
                                }
                                DropMode::Cascade => {
                                    for relation_info in
                                        relations_depend_on(index.index_table_id as RelationId)
                                    {
                                        deque.push_back(relation_info);
                                    }
                                }
                            }
                        }
                    }
                }
                RelationInfo::Source(source) => {
                    if !all_source_ids.insert(source.id) {
                        continue;
                    }

                    if let Some(info) = source.info
                        && info.is_shared()
                    {
                        all_streaming_job_source_ids.insert(source.id);
                        let source_table_fragments = fragment_manager
                            .select_table_fragments_by_table_id(&source.id.into())
                            .await?;
                        all_internal_table_ids.extend(source_table_fragments.internal_table_ids());
                    }

                    if let Some(ref_count) =
                        database_core.relation_ref_count.get(&source.id).cloned()
                    {
                        if ref_count > 0 {
                            // Other relations depend on it.
                            match drop_mode {
                                DropMode::Restrict => {
                                    return Err(MetaError::permission_denied(format!(
                                        "Fail to delete source `{}` because {} other relation(s) depend on it",
                                        source.name, ref_count
                                    )));
                                }
                                DropMode::Cascade => {
                                    for relation_info in
                                        relations_depend_on(source.id as RelationId)
                                    {
                                        deque.push_back(relation_info);
                                    }
                                }
                            }
                        }
                    }
                }
                RelationInfo::View(view) => {
                    if !all_view_ids.insert(view.id) {
                        continue;
                    }

                    if let Some(ref_count) = database_core.relation_ref_count.get(&view.id).cloned()
                    {
                        if ref_count > 0 {
                            // Other relations depend on it.
                            match drop_mode {
                                DropMode::Restrict => {
                                    return Err(MetaError::permission_denied(format!(
                                        "Fail to delete view `{}` because {} other relation(s) depend on it",
                                        view.name, ref_count
                                    )));
                                }
                                DropMode::Cascade => {
                                    for relation_info in relations_depend_on(view.id as RelationId)
                                    {
                                        deque.push_back(relation_info);
                                    }
                                }
                            }
                        }
                    }
                }
                RelationInfo::Sink(sink) => {
                    if !all_sink_ids.insert(sink.id) {
                        continue;
                    }
                    let table_fragments = fragment_manager
                        .select_table_fragments_by_table_id(&sink.id.into())
                        .await?;

                    all_internal_table_ids.extend(table_fragments.internal_table_ids());

                    if let Some(ref_count) = database_core.relation_ref_count.get(&sink.id).cloned()
                    {
                        if ref_count > 0 {
                            // Other relations depend on it.
                            match drop_mode {
                                DropMode::Restrict => {
                                    return Err(MetaError::permission_denied(format!(
                                        "Fail to delete sink `{}` because {} other relation(s) depend on it",
                                        sink.name, ref_count
                                    )));
                                }
                                DropMode::Cascade => {
                                    for relation_info in relations_depend_on(sink.id as RelationId)
                                    {
                                        deque.push_back(relation_info);
                                    }
                                }
                            }
                        }
                    }
                }
                RelationInfo::Subscription(subscription) => {
                    if !all_subscription_ids.insert(subscription.id) {
                        continue;
                    }

                    if let Some(ref_count) = database_core
                        .relation_ref_count
                        .get(&subscription.id)
                        .cloned()
                    {
                        if ref_count > 0 {
                            // Other relations depend on it.
                            match drop_mode {
                                DropMode::Restrict => {
                                    return Err(MetaError::permission_denied(format!(
                                        "Fail to delete subscription `{}` because {} other relation(s) depend on it",
                                        subscription.name, ref_count
                                    )));
                                }
                                DropMode::Cascade => {
                                    for relation_info in
                                        relations_depend_on(subscription.id as RelationId)
                                    {
                                        deque.push_back(relation_info);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let tables_removed = all_table_ids
            .iter()
            .map(|table_id| tables.remove(*table_id).unwrap())
            .collect_vec();

        let indexes_removed = all_index_ids
            .iter()
            .map(|index_id| indexes.remove(*index_id).unwrap())
            .collect_vec();

        let sources_removed = all_source_ids
            .iter()
            .map(|source_id| sources.remove(*source_id).unwrap())
            .collect_vec();

        let views_removed = all_view_ids
            .iter()
            .map(|view_id| views.remove(*view_id).unwrap())
            .collect_vec();

        let sinks_removed = all_sink_ids
            .iter()
            .map(|sink_id| sinks.remove(*sink_id).unwrap())
            .collect_vec();
        let subscriptions_removed = all_subscription_ids
            .iter()
            .map(|subscription_id| subscriptions.remove(*subscription_id).unwrap())
            .collect_vec();

        if !matches!(relation, RelationIdEnum::Sink(_)) {
            let table_sinks = sinks_removed
                .iter()
                .filter(|sink| {
                    if let Some(target_table) = sink.target_table {
                        // Table sink but associated with the table
                        if matches!(relation, RelationIdEnum::Table(table_id) if table_id == target_table) {
                            false
                        } else {
                            // Table sink
                            true
                        }
                    } else {
                        // Normal sink
                        false
                    }
                })
                .collect_vec();

            // Since dropping the sink into the table requires the frontend to handle some of the logic (regenerating the plan), it’s not compatible with the current cascade dropping.
            if !table_sinks.is_empty() {
                bail!(
                    "Found {} sink(s) into table in dependency, please drop them manually",
                    table_sinks.len()
                );
            }
        }

        let internal_tables = all_internal_table_ids
            .iter()
            .map(|internal_table_id| {
                tables
                    .remove(*internal_table_id)
                    .expect("internal table should exist")
            })
            .collect_vec();

        let users_need_update = {
            // TODO: add sources, sinks and views
            let table_to_drop_ids = all_table_ids
                .iter()
                .chain(&all_internal_table_ids)
                .cloned()
                .collect_vec();

            Self::update_user_privileges(
                &mut users,
                &table_to_drop_ids
                    .into_iter()
                    .map(Object::TableId)
                    .chain(all_source_ids.into_iter().map(Object::SourceId))
                    .chain(all_view_ids.into_iter().map(Object::ViewId))
                    .chain(all_sink_ids.iter().cloned().map(Object::SinkId))
                    .collect_vec(),
            )
        };

        commit_meta!(
            self,
            tables,
            indexes,
            sources,
            views,
            sinks,
            users,
            subscriptions
        )?;

        for index in &indexes_removed {
            user_core.decrease_ref(index.owner);
        }

        // `tables_removed` contains both index, table and mv.
        for table in &tables_removed {
            user_core.decrease_ref(table.owner);
        }

        for source in &sources_removed {
            user_core.decrease_ref(source.owner);
        }

        for view in &views_removed {
            user_core.decrease_ref(view.owner);
        }

        for sink in &sinks_removed {
            user_core.decrease_ref(sink.owner);
        }

        for subscription in &subscriptions_removed {
            user_core.decrease_ref(subscription.owner);
        }

        for user in users_need_update {
            self.notify_frontend(Operation::Update, Info::User(user))
                .await;
        }

        // decrease dependent relations
        for table in &tables_removed {
            for dependent_relation_id in &table.dependent_relations {
                database_core.decrease_relation_ref_count(*dependent_relation_id);
            }
        }

        for source in &sources_removed {
            refcnt_dec_connection(database_core, source.connection_id);
            refcnt_dec_source_secret_ref(database_core, source)?;
        }

        for view in &views_removed {
            for dependent_relation_id in &view.dependent_relations {
                database_core.decrease_relation_ref_count(*dependent_relation_id);
            }
        }

        for sink in &sinks_removed {
            refcnt_dec_connection(database_core, sink.connection_id);
            for dependent_relation_id in &sink.dependent_relations {
                database_core.decrease_relation_ref_count(*dependent_relation_id);
            }
            refcnt_dec_sink_secret_ref(database_core, sink);
        }

        for subscription in &subscriptions_removed {
            database_core.decrease_relation_ref_count(subscription.dependent_table_id);
        }

        let version = self
            .notify_frontend(
                Operation::Delete,
                Info::RelationGroup(RelationGroup {
                    relations: indexes_removed
                        .into_iter()
                        .map(|index| Relation {
                            relation_info: RelationInfo::Index(index).into(),
                        })
                        .chain(internal_tables.into_iter().map(|internal_table| Relation {
                            relation_info: RelationInfo::Table(internal_table).into(),
                        }))
                        .chain(tables_removed.into_iter().map(|table| Relation {
                            relation_info: RelationInfo::Table(table).into(),
                        }))
                        .chain(sources_removed.into_iter().map(|source| Relation {
                            relation_info: RelationInfo::Source(source).into(),
                        }))
                        .chain(views_removed.into_iter().map(|view| Relation {
                            relation_info: RelationInfo::View(view).into(),
                        }))
                        .chain(sinks_removed.into_iter().map(|sink| Relation {
                            relation_info: RelationInfo::Sink(sink).into(),
                        }))
                        .chain(
                            subscriptions_removed
                                .into_iter()
                                .map(|subscription| Relation {
                                    relation_info: RelationInfo::Subscription(subscription).into(),
                                }),
                        )
                        .collect_vec(),
                }),
            )
            .await;

        let catalog_deleted_ids: Vec<StreamingJobId> = all_table_ids
            .into_iter()
            .map(|id| id.into())
            .chain(all_sink_ids.into_iter().map(|id| id.into()))
            .chain(all_streaming_job_source_ids.into_iter().map(|id| id.into()))
            .collect_vec();

        Ok((version, catalog_deleted_ids))
    }

    pub async fn alter_table_name(
        &self,
        table_id: TableId,
        table_name: &str,
    ) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        database_core.ensure_table_id(table_id)?;

        // 1. validate new table name.
        let mut table = database_core.tables.get(&table_id).unwrap().clone();
        let old_name = table.name.clone();
        database_core.check_relation_name_duplicated(&(
            table.database_id,
            table.schema_id,
            table_name.to_string(),
        ))?;

        let source = table.optional_associated_source_id.as_ref().map(
            |OptionalAssociatedSourceId::AssociatedSourceId(id)| {
                let mut source = database_core.sources.get(id).unwrap().clone();
                source.name = table_name.to_string();
                source
            },
        );

        // 2. rename table and its definition.
        table.name = table_name.to_string();
        table.definition = alter_relation_rename(&table.definition, table_name);

        // 3. update all relations that depend on this table, note that indexes are not included.
        self.alter_relation_name_refs_inner(
            database_core,
            table_id,
            &old_name,
            table_name,
            vec![table],
            vec![],
            vec![],
            vec![],
            source,
        )
        .await
    }

    // TODO: refactor dependency cache in catalog manager for better performance.
    #[allow(clippy::too_many_arguments)]
    async fn alter_relation_name_refs_inner(
        &self,
        database_mgr: &mut DatabaseManager,
        relation_id: RelationId,
        from: &str,
        to: &str,
        mut to_update_tables: Vec<Table>,
        mut to_update_views: Vec<View>,
        mut to_update_sinks: Vec<Sink>,
        mut to_update_subscriptions: Vec<Subscription>,
        to_update_source: Option<Source>,
    ) -> MetaResult<NotificationVersion> {
        for table in database_mgr.tables.values() {
            if table.dependent_relations.contains(&relation_id) {
                let mut table = table.clone();
                table.definition = alter_relation_rename_refs(&table.definition, from, to);
                to_update_tables.push(table);
            }
        }

        for view in database_mgr.views.values() {
            if view.dependent_relations.contains(&relation_id) {
                let mut view = view.clone();
                view.sql = alter_relation_rename_refs(&view.sql, from, to);
                to_update_views.push(view);
            }
        }

        for sink in database_mgr.sinks.values() {
            if sink.dependent_relations.contains(&relation_id)
                || sink.target_table == Some(relation_id)
            {
                let mut sink = sink.clone();
                sink.definition = alter_relation_rename_refs(&sink.definition, from, to);
                to_update_sinks.push(sink);
            }
        }

        for subscription in database_mgr.subscriptions.values() {
            if subscription.dependent_table_id == relation_id {
                let mut subscription = subscription.clone();
                subscription.definition =
                    alter_relation_rename_refs(&subscription.definition, from, to);
                to_update_subscriptions.push(subscription);
            }
        }

        // commit meta.
        let mut tables = BTreeMapTransaction::new(&mut database_mgr.tables);
        let mut views = BTreeMapTransaction::new(&mut database_mgr.views);
        let mut sinks = BTreeMapTransaction::new(&mut database_mgr.sinks);
        let mut subscriptions: BTreeMapTransaction<'_, u32, risingwave_pb::catalog::Subscription> =
            BTreeMapTransaction::new(&mut database_mgr.subscriptions);
        let mut sources = BTreeMapTransaction::new(&mut database_mgr.sources);
        to_update_tables.iter().for_each(|table| {
            tables.insert(table.id, table.clone());
        });
        to_update_views.iter().for_each(|view| {
            views.insert(view.id, view.clone());
        });
        to_update_sinks.iter().for_each(|sink| {
            sinks.insert(sink.id, sink.clone());
        });
        to_update_subscriptions.iter().for_each(|subscription| {
            subscriptions.insert(subscription.id, subscription.clone());
        });
        if let Some(source) = &to_update_source {
            sources.insert(source.id, source.clone());
        }
        commit_meta!(self, tables, views, sinks, sources, subscriptions)?;

        // 5. notify frontend.
        assert!(
            !to_update_tables.is_empty()
                || !to_update_views.is_empty()
                || !to_update_sinks.is_empty()
                || !to_update_subscriptions.is_empty()
                || to_update_source.is_some()
        );
        let version = self
            .notify_frontend(
                Operation::Update,
                Info::RelationGroup(RelationGroup {
                    relations: to_update_tables
                        .into_iter()
                        .map(|table| Relation {
                            relation_info: RelationInfo::Table(table).into(),
                        })
                        .chain(to_update_views.into_iter().map(|view| Relation {
                            relation_info: RelationInfo::View(view).into(),
                        }))
                        .chain(to_update_sinks.into_iter().map(|sink| Relation {
                            relation_info: RelationInfo::Sink(sink).into(),
                        }))
                        .chain(
                            to_update_subscriptions
                                .into_iter()
                                .map(|subscription| Relation {
                                    relation_info: RelationInfo::Subscription(subscription).into(),
                                }),
                        )
                        .chain(to_update_source.into_iter().map(|source| Relation {
                            relation_info: RelationInfo::Source(source).into(),
                        }))
                        .collect(),
                }),
            )
            .await;

        Ok(version)
    }

    pub async fn alter_view_name(
        &self,
        view_id: ViewId,
        view_name: &str,
    ) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        database_core.ensure_view_id(view_id)?;

        // 1. validate new view name.
        let mut view = database_core.views.get(&view_id).unwrap().clone();
        let old_name = view.name.clone();
        database_core.check_relation_name_duplicated(&(
            view.database_id,
            view.schema_id,
            view_name.to_string(),
        ))?;

        // 2. rename view, note that there's no need to update its definition since it only stores
        // the query part.
        view.name = view_name.to_string();

        // 3. update all relations that depend on this view.
        self.alter_relation_name_refs_inner(
            database_core,
            view_id,
            &old_name,
            view_name,
            vec![],
            vec![view],
            vec![],
            vec![],
            None,
        )
        .await
    }

    pub async fn alter_sink_name(
        &self,
        sink_id: SinkId,
        sink_name: &str,
    ) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        database_core.ensure_sink_id(sink_id)?;

        // 1. validate new sink name.
        let mut sink = database_core.sinks.get(&sink_id).unwrap().clone();
        database_core.check_relation_name_duplicated(&(
            sink.database_id,
            sink.schema_id,
            sink_name.to_string(),
        ))?;

        // 2. rename sink and its definition.
        sink.name = sink_name.to_string();
        sink.definition = alter_relation_rename(&sink.definition, sink_name);

        // 3. commit meta.
        let mut sinks = BTreeMapTransaction::new(&mut database_core.sinks);
        sinks.insert(sink_id, sink.clone());
        commit_meta!(self, sinks)?;

        let version = self
            .notify_frontend_relation_info(Operation::Update, RelationInfo::Sink(sink))
            .await;

        Ok(version)
    }

    pub async fn alter_subscription_name(
        &self,
        subscription_id: SubscriptionId,
        subscription_name: &str,
    ) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        database_core.ensure_subscription_id(subscription_id)?;

        // 1. validate new subscription name.
        let mut subscription = database_core
            .subscriptions
            .get(&subscription_id)
            .unwrap()
            .clone();
        database_core.check_relation_name_duplicated(&(
            subscription.database_id,
            subscription.schema_id,
            subscription_name.to_string(),
        ))?;

        // 2. rename subscription and its definition.
        subscription.name = subscription_name.to_string();
        subscription.definition =
            alter_relation_rename(&subscription.definition, subscription_name);

        // 3. commit meta.
        let mut subscriptions = BTreeMapTransaction::new(&mut database_core.subscriptions);
        subscriptions.insert(subscription_id, subscription.clone());
        commit_meta!(self, subscriptions)?;

        let version = self
            .notify_frontend_relation_info(
                Operation::Update,
                RelationInfo::Subscription(subscription),
            )
            .await;

        Ok(version)
    }

    pub async fn alter_source_name(
        &self,
        source_id: SourceId,
        source_name: &str,
    ) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        database_core.ensure_source_id(source_id)?;

        // 1. validate new source name.
        let mut source = database_core.sources.get(&source_id).unwrap().clone();
        database_core.check_relation_name_duplicated(&(
            source.database_id,
            source.schema_id,
            source_name.to_string(),
        ))?;

        // 2. rename source and its definition.
        let old_name = source.name.clone();
        source.name = source_name.to_string();
        source.definition = alter_relation_rename(&source.definition, source_name);

        // 3. update, commit and notify all relations that depend on this source.
        self.alter_relation_name_refs_inner(
            database_core,
            source_id,
            &old_name,
            source_name,
            vec![],
            vec![],
            vec![],
            vec![],
            Some(source),
        )
        .await
    }

    pub async fn alter_schema_name(
        &self,
        schema_id: SchemaId,
        schema_name: &str,
    ) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        database_core.ensure_schema_id(schema_id)?;

        // 1. validate new schema name.
        let mut schema = database_core.schemas.get(&schema_id).unwrap().clone();
        database_core.check_schema_duplicated(&(schema.database_id, schema_name.to_string()))?;

        // 2. rename schema.
        schema.name = schema_name.to_string();

        // 3. update, commit and notify.
        let mut schemas = BTreeMapTransaction::new(&mut database_core.schemas);
        schemas.insert(schema_id, schema.clone());
        commit_meta!(self, schemas)?;

        let version = self
            .notify_frontend(Operation::Update, Info::Schema(schema))
            .await;

        Ok(version)
    }

    pub async fn alter_database_name(
        &self,
        database_id: SchemaId,
        database_name: &str,
    ) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        database_core.ensure_database_id(database_id)?;

        // 1. validate new database name.
        let database_name = database_name.to_string();
        let mut database = database_core.databases.get(&database_id).unwrap().clone();
        database_core.check_database_duplicated(&database_name)?;

        // 2. rename database.
        database.name = database_name;

        // 3. update, commit and notify.
        let mut databases = BTreeMapTransaction::new(&mut database_core.databases);
        databases.insert(database_id, database.clone());
        commit_meta!(self, databases)?;

        let version = self
            .notify_frontend(Operation::Update, Info::Database(database))
            .await;

        Ok(version)
    }

    pub async fn alter_source_column(&self, source: Source) -> MetaResult<NotificationVersion> {
        let source_id = source.get_id();
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        database_core.ensure_source_id(source_id)?;

        let original_source = database_core.sources.get(&source_id).unwrap().clone();
        if original_source.get_version() + 1 != source.get_version() {
            bail!("source version is stale");
        }

        let mut sources = BTreeMapTransaction::new(&mut database_core.sources);
        sources.insert(source_id, source.clone());
        commit_meta!(self, sources)?;

        let version = self
            .notify_frontend_relation_info(Operation::Update, RelationInfo::Source(source))
            .await;

        Ok(version)
    }

    pub async fn alter_owner(
        &self,
        fragment_manager: FragmentManagerRef,
        object: alter_owner_request::Object,
        owner_id: UserId,
    ) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let user_core = &mut core.user;

        let relation_info;
        match object {
            alter_owner_request::Object::TableId(table_id) => {
                database_core.ensure_table_id(table_id)?;
                let mut tables = BTreeMapTransaction::new(&mut database_core.tables);
                let mut sources = BTreeMapTransaction::new(&mut database_core.sources);
                let mut indexes = BTreeMapTransaction::new(&mut database_core.indexes);

                let table = tables.tree_ref().get(&table_id).unwrap();
                let old_owner_id = table.owner;
                if old_owner_id == owner_id {
                    return Ok(IGNORED_NOTIFICATION_VERSION);
                }
                // associated source id.
                let to_update_source_id = if let Some(
                    OptionalAssociatedSourceId::AssociatedSourceId(associated_source_id),
                ) = &table.optional_associated_source_id
                {
                    Some(*associated_source_id)
                } else {
                    None
                };

                let mut to_update_table_ids = vec![table_id];
                let mut to_update_internal_table_ids = vec![];

                // indexes and index tables.
                let (to_update_index_ids, index_table_ids): (Vec<_>, Vec<_>) = indexes
                    .tree_ref()
                    .iter()
                    .filter(|(_, index)| index.primary_table_id == table_id)
                    .map(|(index_id, index)| (*index_id, index.index_table_id))
                    .unzip();
                to_update_table_ids.extend(index_table_ids);

                // internal tables.
                for id in &to_update_table_ids {
                    let table_fragment = fragment_manager
                        .select_table_fragments_by_table_id(&(id.into()))
                        .await?;
                    to_update_internal_table_ids.extend(table_fragment.internal_table_ids());
                }

                let mut relations = vec![];
                // update owner.
                for id in &to_update_table_ids {
                    let mut table = tables.get_mut(*id).unwrap();
                    assert_eq!(old_owner_id, table.owner);
                    table.owner = owner_id;
                    relations.push(Relation {
                        relation_info: Some(RelationInfo::Table(table.clone())),
                    });
                }
                for index_id in &to_update_index_ids {
                    let mut index = indexes.get_mut(*index_id).unwrap();
                    assert_eq!(old_owner_id, index.owner);
                    index.owner = owner_id;
                    relations.push(Relation {
                        relation_info: Some(RelationInfo::Index(index.clone())),
                    });
                }
                if let Some(associated_source_id) = &to_update_source_id {
                    let mut source = sources.get_mut(*associated_source_id).unwrap();
                    assert_eq!(old_owner_id, source.owner);
                    source.owner = owner_id;
                    relations.push(Relation {
                        relation_info: Some(RelationInfo::Source(source.clone())),
                    });
                }
                for internal_table_id in to_update_internal_table_ids {
                    let mut table = tables.get_mut(internal_table_id).unwrap();
                    assert_eq!(old_owner_id, table.owner);
                    table.owner = owner_id;
                    relations.push(Relation {
                        relation_info: Some(RelationInfo::Table(table.clone())),
                    });
                }

                commit_meta!(self, tables, indexes, sources)?;
                let count = to_update_table_ids.len()
                    + to_update_index_ids.len()
                    + to_update_source_id.map_or(0, |_| 1);
                user_core.decrease_ref_count(old_owner_id, count);
                user_core.increase_ref_count(owner_id, count);
                relation_info = Info::RelationGroup(RelationGroup { relations });
            }
            alter_owner_request::Object::ViewId(view_id) => {
                database_core.ensure_view_id(view_id)?;
                let mut views = BTreeMapTransaction::new(&mut database_core.views);
                let mut view = views.get_mut(view_id).unwrap();
                let old_owner_id = view.owner;
                if old_owner_id == owner_id {
                    return Ok(IGNORED_NOTIFICATION_VERSION);
                }
                view.owner = owner_id;
                relation_info = Info::RelationGroup(RelationGroup {
                    relations: vec![Relation {
                        relation_info: Some(RelationInfo::View(view.clone())),
                    }],
                });
                commit_meta!(self, views)?;
                user_core.increase_ref(owner_id);
                user_core.decrease_ref(old_owner_id);
            }
            alter_owner_request::Object::SourceId(source_id) => {
                database_core.ensure_source_id(source_id)?;
                let mut sources = BTreeMapTransaction::new(&mut database_core.sources);
                let mut source = sources.get_mut(source_id).unwrap();
                let old_owner_id = source.owner;
                if old_owner_id == owner_id {
                    return Ok(IGNORED_NOTIFICATION_VERSION);
                }
                source.owner = owner_id;
                relation_info = Info::RelationGroup(RelationGroup {
                    relations: vec![Relation {
                        relation_info: Some(RelationInfo::Source(source.clone())),
                    }],
                });
                commit_meta!(self, sources)?;
                user_core.increase_ref(owner_id);
                user_core.decrease_ref(old_owner_id);
            }
            alter_owner_request::Object::SinkId(sink_id) => {
                database_core.ensure_sink_id(sink_id)?;
                let mut sinks = BTreeMapTransaction::new(&mut database_core.sinks);
                let mut tables = BTreeMapTransaction::new(&mut database_core.tables);
                let mut sink = sinks.get_mut(sink_id).unwrap();
                let old_owner_id = sink.owner;
                if old_owner_id == owner_id {
                    return Ok(IGNORED_NOTIFICATION_VERSION);
                }
                sink.owner = owner_id;

                let mut relations = vec![Relation {
                    relation_info: Some(RelationInfo::Sink(sink.clone())),
                }];

                // internal tables
                let internal_table_ids = fragment_manager
                    .select_table_fragments_by_table_id(&(sink_id.into()))
                    .await?
                    .internal_table_ids();
                for id in internal_table_ids {
                    let mut table = tables.get_mut(id).unwrap();
                    assert_eq!(old_owner_id, table.owner);
                    table.owner = owner_id;
                    relations.push(Relation {
                        relation_info: Some(RelationInfo::Table(table.clone())),
                    });
                }

                relation_info = Info::RelationGroup(RelationGroup { relations });
                commit_meta!(self, sinks, tables)?;
                user_core.increase_ref(owner_id);
                user_core.decrease_ref(old_owner_id);
            }
            alter_owner_request::Object::DatabaseId(database_id) => {
                database_core.ensure_database_id(database_id)?;
                let mut databases = BTreeMapTransaction::new(&mut database_core.databases);
                let mut database = databases.get_mut(database_id).unwrap();
                let old_owner_id = database.owner;
                if old_owner_id == owner_id {
                    return Ok(IGNORED_NOTIFICATION_VERSION);
                }
                database.owner = owner_id;
                relation_info = Info::Database(database.clone());
                let mut users = BTreeMapTransaction::new(&mut user_core.user_info);
                let mut user = users.get_mut(owner_id).unwrap();
                let new_grant_privilege = GrantPrivilege {
                    object: Some(Object::DatabaseId(database_id)),
                    action_with_opts: vec![{
                        ActionWithGrantOption {
                            action: Action::Connect as _,
                            with_grant_option: false,
                            granted_by: old_owner_id,
                        }
                    }],
                };
                if let Some(privilege) = user
                    .grant_privileges
                    .iter_mut()
                    .find(|p| p.object == new_grant_privilege.object)
                {
                    Self::merge_privilege(privilege, &new_grant_privilege);
                } else {
                    user.grant_privileges.push(new_grant_privilege.clone());
                }
                let user_info = Info::User(user.clone());
                commit_meta!(self, databases, users)?;
                user_core.increase_ref(owner_id);
                user_core.decrease_ref(old_owner_id);
                self.notify_frontend(Operation::Update, user_info).await;
                let version = self.notify_frontend(Operation::Update, relation_info).await;
                return Ok(version);
            }
            alter_owner_request::Object::SchemaId(schema_id) => {
                database_core.ensure_schema_id(schema_id)?;
                let mut schemas = BTreeMapTransaction::new(&mut database_core.schemas);
                let mut schema = schemas.get_mut(schema_id).unwrap();
                let old_owner_id = schema.owner;
                if old_owner_id == owner_id {
                    return Ok(IGNORED_NOTIFICATION_VERSION);
                }
                schema.owner = owner_id;
                relation_info = Info::Schema(schema.clone());
                commit_meta!(self, schemas)?;
                user_core.increase_ref(owner_id);
                user_core.decrease_ref(old_owner_id);
            }
            alter_owner_request::Object::SubscriptionId(subscription_id) => {
                database_core.ensure_subscription_id(subscription_id)?;
                let mut subscriptions = BTreeMapTransaction::new(&mut database_core.subscriptions);
                let mut subscription = subscriptions.get_mut(subscription_id).unwrap();
                let old_owner_id = subscription.owner;
                if old_owner_id == owner_id {
                    return Ok(IGNORED_NOTIFICATION_VERSION);
                }
                subscription.owner = owner_id;

                let relations = vec![Relation {
                    relation_info: Some(RelationInfo::Subscription(subscription.clone())),
                }];

                relation_info = Info::RelationGroup(RelationGroup { relations });
                commit_meta!(self, subscriptions)?;
                user_core.increase_ref(owner_id);
                user_core.decrease_ref(old_owner_id);
            }
        };

        let version = self.notify_frontend(Operation::Update, relation_info).await;

        Ok(version)
    }

    pub async fn alter_set_schema(
        &self,
        fragment_manager: FragmentManagerRef,
        object: alter_set_schema_request::Object,
        new_schema_id: SchemaId,
    ) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;

        database_core.ensure_schema_id(new_schema_id)?;
        let database_id = database_core
            .schemas
            .get(&new_schema_id)
            .unwrap()
            .get_database_id();

        let mut relation_infos = Vec::new();
        match object {
            alter_set_schema_request::Object::TableId(table_id) => {
                database_core.ensure_table_id(table_id)?;
                let Table {
                    name,
                    optional_associated_source_id,
                    schema_id,
                    ..
                } = database_core.tables.get(&table_id).unwrap();
                if *schema_id == new_schema_id {
                    return Ok(IGNORED_NOTIFICATION_VERSION);
                }

                database_core.check_relation_name_duplicated(&(
                    database_id,
                    new_schema_id,
                    name.to_owned(),
                ))?;

                // associated source id.
                let to_update_source_id = if let Some(
                    OptionalAssociatedSourceId::AssociatedSourceId(associated_source_id),
                ) = optional_associated_source_id
                {
                    Some(*associated_source_id)
                } else {
                    None
                };

                let mut to_update_table_ids = vec![table_id];
                let mut to_update_internal_table_ids = vec![];

                // indexes and index tables.
                let (to_update_index_ids, index_table_ids): (Vec<_>, Vec<_>) = database_core
                    .indexes
                    .iter()
                    .filter(|(_, index)| index.primary_table_id == table_id)
                    .map(|(index_id, index)| (*index_id, index.index_table_id))
                    .unzip();
                to_update_table_ids.extend(index_table_ids);

                // internal tables.
                for table_id in &to_update_table_ids {
                    let table_fragment = fragment_manager
                        .select_table_fragments_by_table_id(&(table_id.into()))
                        .await?;
                    to_update_internal_table_ids.extend(table_fragment.internal_table_ids());
                }

                let mut tables = BTreeMapTransaction::new(&mut database_core.tables);
                for table_id in to_update_table_ids
                    .into_iter()
                    .chain(to_update_internal_table_ids)
                {
                    let mut table = tables.get_mut(table_id).unwrap();
                    table.schema_id = new_schema_id;
                    relation_infos.push(Some(RelationInfo::Table(table.clone())));
                }

                let mut sources = BTreeMapTransaction::new(&mut database_core.sources);
                if let Some(source_id) = to_update_source_id {
                    let mut source = sources.get_mut(source_id).unwrap();
                    source.schema_id = new_schema_id;
                    relation_infos.push(Some(RelationInfo::Source(source.clone())));
                }

                let mut indexes = BTreeMapTransaction::new(&mut database_core.indexes);
                for index_id in to_update_index_ids {
                    let mut index = indexes.get_mut(index_id).unwrap();
                    index.schema_id = new_schema_id;
                    relation_infos.push(Some(RelationInfo::Index(index.clone())));
                }

                commit_meta!(self, tables, sources, indexes)?;
            }
            alter_set_schema_request::Object::ViewId(view_id) => {
                database_core.ensure_view_id(view_id)?;
                let View {
                    name, schema_id, ..
                } = database_core.views.get(&view_id).unwrap();
                if *schema_id == new_schema_id {
                    return Ok(IGNORED_NOTIFICATION_VERSION);
                }

                database_core.check_relation_name_duplicated(&(
                    database_id,
                    new_schema_id,
                    name.to_owned(),
                ))?;
                let mut views = BTreeMapTransaction::new(&mut database_core.views);
                let mut view = views.get_mut(view_id).unwrap();
                view.schema_id = new_schema_id;
                relation_infos.push(Some(RelationInfo::View(view.clone())));
                commit_meta!(self, views)?;
            }
            alter_set_schema_request::Object::SourceId(source_id) => {
                database_core.ensure_source_id(source_id)?;
                let Source {
                    name, schema_id, ..
                } = database_core.sources.get(&source_id).unwrap();
                if *schema_id == new_schema_id {
                    return Ok(IGNORED_NOTIFICATION_VERSION);
                }

                database_core.check_relation_name_duplicated(&(
                    database_id,
                    new_schema_id,
                    name.to_owned(),
                ))?;
                let mut sources = BTreeMapTransaction::new(&mut database_core.sources);
                let mut source = sources.get_mut(source_id).unwrap();
                source.schema_id = new_schema_id;
                relation_infos.push(Some(RelationInfo::Source(source.clone())));
                commit_meta!(self, sources)?;
            }
            alter_set_schema_request::Object::SinkId(sink_id) => {
                database_core.ensure_sink_id(sink_id)?;
                let Sink {
                    name, schema_id, ..
                } = database_core.sinks.get(&sink_id).unwrap();
                if *schema_id == new_schema_id {
                    return Ok(IGNORED_NOTIFICATION_VERSION);
                }

                // internal tables.
                let to_update_internal_table_ids = Vec::from_iter(
                    fragment_manager
                        .select_table_fragments_by_table_id(&(sink_id.into()))
                        .await?
                        .internal_table_ids(),
                );

                database_core.check_relation_name_duplicated(&(
                    database_id,
                    new_schema_id,
                    name.to_owned(),
                ))?;
                let mut sinks = BTreeMapTransaction::new(&mut database_core.sinks);
                let mut sink = sinks.get_mut(sink_id).unwrap();
                sink.schema_id = new_schema_id;
                relation_infos.push(Some(RelationInfo::Sink(sink.clone())));

                let mut tables = BTreeMapTransaction::new(&mut database_core.tables);
                for table_id in to_update_internal_table_ids {
                    let mut table = tables.get_mut(table_id).unwrap();
                    table.schema_id = new_schema_id;
                    relation_infos.push(Some(RelationInfo::Table(table.clone())));
                }

                commit_meta!(self, sinks, tables)?;
            }
            alter_set_schema_request::Object::ConnectionId(connection_id) => {
                database_core.ensure_connection_id(connection_id)?;
                let Connection {
                    name, schema_id, ..
                } = database_core.connections.get(&connection_id).unwrap();
                if *schema_id == new_schema_id {
                    return Ok(IGNORED_NOTIFICATION_VERSION);
                }

                database_core.check_connection_name_duplicated(&(
                    database_id,
                    new_schema_id,
                    name.to_owned(),
                ))?;

                let mut connections = BTreeMapTransaction::new(&mut database_core.connections);
                let mut connection = connections.get_mut(connection_id).unwrap();
                connection.schema_id = new_schema_id;
                let notify_info = Info::Connection(connection.clone());
                commit_meta!(self, connections)?;
                let version = self.notify_frontend(Operation::Update, notify_info).await;
                return Ok(version);
            }
            alter_set_schema_request::Object::FunctionId(function_id) => {
                database_core.ensure_function_id(function_id)?;
                let Function {
                    name,
                    arg_types,
                    schema_id,
                    ..
                } = database_core.functions.get(&function_id).unwrap();
                if *schema_id == new_schema_id {
                    return Ok(IGNORED_NOTIFICATION_VERSION);
                }

                database_core.check_function_duplicated(&(
                    database_id,
                    new_schema_id,
                    name.to_owned(),
                    arg_types.to_owned(),
                ))?;
                let mut functions = BTreeMapTransaction::new(&mut database_core.functions);
                let mut function = functions.get_mut(function_id).unwrap();
                function.schema_id = new_schema_id;
                let notify_info = Info::Function(function.clone());
                commit_meta!(self, functions)?;
                let version = self.notify_frontend(Operation::Update, notify_info).await;
                return Ok(version);
            }
            alter_set_schema_request::Object::SubscriptionId(subscription_id) => {
                database_core.ensure_subscription_id(subscription_id)?;
                let Subscription {
                    name, schema_id, ..
                } = database_core.subscriptions.get(&subscription_id).unwrap();
                if *schema_id == new_schema_id {
                    return Ok(IGNORED_NOTIFICATION_VERSION);
                }

                database_core.check_relation_name_duplicated(&(
                    database_id,
                    new_schema_id,
                    name.to_owned(),
                ))?;
                let mut subscriptions = BTreeMapTransaction::new(&mut database_core.subscriptions);
                let mut subscription = subscriptions.get_mut(subscription_id).unwrap();
                subscription.schema_id = new_schema_id;
                relation_infos.push(Some(RelationInfo::Subscription(subscription.clone())));
                commit_meta!(self, subscriptions)?;
            }
        }

        let version = self
            .notify_frontend(
                Operation::Update,
                Info::RelationGroup(RelationGroup {
                    relations: relation_infos
                        .into_iter()
                        .map(|relation_info| Relation { relation_info })
                        .collect_vec(),
                }),
            )
            .await;
        Ok(version)
    }

    pub async fn alter_index_name(
        &self,
        index_id: IndexId,
        index_name: &str,
    ) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        database_core.ensure_index_id(index_id)?;

        // 1. validate new index name.
        let mut index = database_core.indexes.get(&index_id).unwrap().clone();
        database_core.check_relation_name_duplicated(&(
            index.database_id,
            index.schema_id,
            index_name.to_string(),
        ))?;
        let mut index_table = database_core
            .tables
            .get(&index.index_table_id)
            .unwrap()
            .clone();

        // 2. rename index name.
        index.name = index_name.to_string();
        index_table.name = index_name.to_string();
        index_table.definition = alter_relation_rename(&index_table.definition, index_name);
        let mut indexes = BTreeMapTransaction::new(&mut database_core.indexes);
        let mut tables = BTreeMapTransaction::new(&mut database_core.tables);
        indexes.insert(index_id, index.clone());
        tables.insert(index.index_table_id, index_table.clone());
        commit_meta!(self, indexes, tables)?;

        let version = self
            .notify_frontend(
                Operation::Update,
                Info::RelationGroup(RelationGroup {
                    relations: vec![
                        Relation {
                            relation_info: RelationInfo::Table(index_table).into(),
                        },
                        Relation {
                            relation_info: RelationInfo::Index(index).into(),
                        },
                    ],
                }),
            )
            .await;

        Ok(version)
    }

    pub async fn start_create_source_procedure(&self, source: &Source) -> MetaResult<()> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let user_core = &mut core.user;
        database_core.ensure_database_id(source.database_id)?;
        database_core.ensure_schema_id(source.schema_id)?;
        let key = (source.database_id, source.schema_id, source.name.clone());
        database_core.check_relation_name_duplicated(&key)?;
        #[cfg(not(test))]
        user_core.ensure_user_id(source.owner)?;

        if database_core.has_in_progress_creation(&key) {
            bail!("source is in creating procedure");
        } else {
            database_core.mark_creating(&key);
            user_core.increase_ref(source.owner);
            refcnt_inc_source_secret_ref(database_core, source)?;
            // We have validate the status of connection before starting the procedure.
            refcnt_inc_connection(database_core, source.connection_id)?;
            Ok(())
        }
    }

    pub async fn get_connection_by_id(
        &self,
        connection_id: ConnectionId,
    ) -> MetaResult<Connection> {
        let core = &mut self.core.lock().await;
        let database_core = &core.database;
        database_core
            .get_connection(connection_id)
            .cloned()
            .ok_or_else(|| MetaError::catalog_id_not_found("connection", connection_id))
    }

    pub async fn finish_create_source_procedure(
        &self,
        mut source: Source,
        mut internal_tables: Vec<Table>,
    ) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let mut tables = BTreeMapTransaction::new(&mut database_core.tables);
        let mut sources = BTreeMapTransaction::new(&mut database_core.sources);
        let key = (source.database_id, source.schema_id, source.name.clone());
        assert!(
            !sources.contains_key(&source.id)
                && database_core.in_progress_creation_tracker.contains(&key),
            "source must be in creating procedure"
        );
        database_core.in_progress_creation_tracker.remove(&key);

        source.created_at_epoch = Some(Epoch::now().0);
        sources.insert(source.id, source.clone());
        for table in &mut internal_tables {
            table.stream_job_status = PbStreamJobStatus::Created.into();
            tables.insert(table.id, table.clone());
        }
        commit_meta!(self, sources, tables)?;

        let version = self
            .notify_frontend(
                Operation::Add,
                Info::RelationGroup(RelationGroup {
                    relations: std::iter::once(Relation {
                        relation_info: RelationInfo::Source(source.to_owned()).into(),
                    })
                    .chain(internal_tables.into_iter().map(|internal_table| Relation {
                        relation_info: RelationInfo::Table(internal_table).into(),
                    }))
                    .collect_vec(),
                }),
            )
            .await;

        Ok(version)
    }

    pub async fn cancel_create_source_procedure(&self, source: &Source) -> MetaResult<()> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let user_core = &mut core.user;
        let key = (source.database_id, source.schema_id, source.name.clone());
        assert!(
            !database_core.sources.contains_key(&source.id)
                && database_core.has_in_progress_creation(&key),
            "source must be in creating procedure"
        );

        database_core.unmark_creating(&key);
        user_core.decrease_ref(source.owner);
        refcnt_dec_connection(database_core, source.connection_id);
        refcnt_dec_source_secret_ref(database_core, source)?;
        Ok(())
    }

    pub async fn start_create_table_procedure_with_source(
        &self,
        source: &Source,
        table: &Table,
    ) -> MetaResult<()> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let user_core = &mut core.user;
        database_core.ensure_database_id(source.database_id)?;
        database_core.ensure_schema_id(source.schema_id)?;
        let source_key = (source.database_id, source.schema_id, source.name.clone());
        database_core.check_relation_name_duplicated(&source_key)?;
        #[cfg(not(test))]
        user_core.ensure_user_id(source.owner)?;
        assert_eq!(source.owner, table.owner);

        let mview_key = (table.database_id, table.schema_id, table.name.clone());
        if database_core.has_in_progress_creation(&source_key)
            || database_core.has_in_progress_creation(&mview_key)
        {
            bail!("table or source is in creating procedure");
        } else {
            database_core.mark_creating(&source_key);
            database_core.mark_creating(&mview_key);
            database_core.mark_creating_streaming_job(table.id, mview_key);
            ensure!(table.dependent_relations.is_empty());
            // source and table
            user_core.increase_ref_count(source.owner, 2);

            refcnt_inc_source_secret_ref(database_core, source)?;
            // We have validate the status of connection before starting the procedure.
            refcnt_inc_connection(database_core, source.connection_id)?;
            Ok(())
        }
    }

    pub async fn finish_create_table_procedure_with_source(
        &self,
        source: Source,
        mut mview: Table,
        mut internal_tables: Vec<Table>,
    ) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let mut tables = BTreeMapTransaction::new(&mut database_core.tables);
        let mut sources = BTreeMapTransaction::new(&mut database_core.sources);

        let source_key = (source.database_id, source.schema_id, source.name.clone());
        let mview_key = (mview.database_id, mview.schema_id, mview.name.clone());
        assert!(
            !sources.contains_key(&source.id)
                && !tables.contains_key(&mview.id)
                && database_core
                    .in_progress_creation_tracker
                    .contains(&source_key)
                && database_core
                    .in_progress_creation_tracker
                    .contains(&mview_key),
            "table and source must be in creating procedure"
        );
        database_core
            .in_progress_creation_tracker
            .remove(&source_key);
        database_core
            .in_progress_creation_tracker
            .remove(&mview_key);
        database_core
            .in_progress_creating_streaming_job
            .remove(&mview.id);

        sources.insert(source.id, source.clone());
        mview.stream_job_status = PbStreamJobStatus::Created.into();
        tables.insert(mview.id, mview.clone());
        for table in &mut internal_tables {
            table.stream_job_status = PbStreamJobStatus::Created.into();
            tables.insert(table.id, table.clone());
        }
        commit_meta!(self, sources, tables)?;

        let version = self
            .notify_frontend(
                Operation::Add,
                Info::RelationGroup(RelationGroup {
                    relations: vec![
                        Relation {
                            relation_info: RelationInfo::Table(mview.to_owned()).into(),
                        },
                        Relation {
                            relation_info: RelationInfo::Source(source.to_owned()).into(),
                        },
                    ]
                    .into_iter()
                    .chain(internal_tables.into_iter().map(|internal_table| Relation {
                        relation_info: RelationInfo::Table(internal_table).into(),
                    }))
                    .collect_vec(),
                }),
            )
            .await;

        Ok(version)
    }

    pub async fn cancel_create_table_procedure_with_source(
        &self,
        source: &Source,
        table: &Table,
    ) -> MetaResult<()> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let user_core = &mut core.user;
        let source_key = (source.database_id, source.schema_id, source.name.clone());
        let table_key = (table.database_id, table.schema_id, table.name.clone());
        assert!(
            !database_core.sources.contains_key(&source.id)
                && !database_core.tables.contains_key(&table.id),
            "table and source must be in creating procedure"
        );

        database_core.unmark_creating(&source_key);
        database_core.unmark_creating(&table_key);
        database_core.unmark_creating_streaming_job(table.id);
        user_core.decrease_ref_count(source.owner, 2); // source and table
        refcnt_dec_connection(database_core, source.connection_id);
        refcnt_dec_source_secret_ref(database_core, source)?;
        Ok(())
    }

    pub async fn start_create_index_procedure(
        &self,
        index: &Index,
        index_table: &Table,
    ) -> MetaResult<()> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let user_core = &mut core.user;
        database_core.ensure_database_id(index.database_id)?;
        database_core.ensure_schema_id(index.schema_id)?;
        database_core.ensure_table_id(index.primary_table_id)?;
        let key = (index.database_id, index.schema_id, index.name.clone());
        database_core.check_relation_name_duplicated(&key)?;
        #[cfg(not(test))]
        user_core.ensure_user_id(index.owner)?;
        assert_eq!(index.owner, index_table.owner);

        // `dependent_relations` should contains 1 and only 1 item that is the `primary_table_id`
        assert_eq!(index_table.dependent_relations.len(), 1);
        assert_eq!(index.primary_table_id, index_table.dependent_relations[0]);

        if database_core.has_in_progress_creation(&key) {
            bail!("index already in creating procedure");
        } else {
            database_core.mark_creating(&key);
            database_core.mark_creating_streaming_job(index_table.id, key);
            for &dependent_relation_id in &index_table.dependent_relations {
                database_core.increase_relation_ref_count(dependent_relation_id);
            }
            // index table and index.
            user_core.increase_ref_count(index.owner, 2);
            Ok(())
        }
    }

    pub async fn cancel_create_index_procedure(&self, index: &Index, index_table: &Table) {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let user_core = &mut core.user;
        let key = (index.database_id, index.schema_id, index.name.clone());
        assert!(
            !database_core.indexes.contains_key(&index.id),
            "index must be in creating procedure"
        );

        database_core.unmark_creating(&key);
        database_core.unmark_creating_streaming_job(index_table.id);
        for &dependent_relation_id in &index_table.dependent_relations {
            database_core.decrease_relation_ref_count(dependent_relation_id);
        }
        // index table and index.
        user_core.decrease_ref_count(index.owner, 2);
    }

    pub async fn finish_create_index_procedure(
        &self,
        mut internal_tables: Vec<Table>,
        mut index: Index,
        mut table: Table,
    ) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let key = (table.database_id, table.schema_id, index.name.clone());

        let mut indexes = BTreeMapTransaction::new(&mut database_core.indexes);
        let mut tables = BTreeMapTransaction::new(&mut database_core.tables);
        assert!(
            !indexes.contains_key(&index.id)
                && database_core.in_progress_creation_tracker.contains(&key),
            "index must be in creating procedure"
        );

        database_core.in_progress_creation_tracker.remove(&key);
        database_core
            .in_progress_creating_streaming_job
            .remove(&table.id);

        index.stream_job_status = PbStreamJobStatus::Created.into();
        indexes.insert(index.id, index.clone());

        table.stream_job_status = PbStreamJobStatus::Created.into();
        tables.insert(table.id, table.clone());
        for table in &mut internal_tables {
            table.stream_job_status = PbStreamJobStatus::Created.into();
            tables.insert(table.id, table.clone());
        }
        commit_meta!(self, indexes, tables)?;

        let version = self
            .notify_frontend(
                Operation::Add,
                Info::RelationGroup(RelationGroup {
                    relations: vec![
                        Relation {
                            relation_info: RelationInfo::Table(table.to_owned()).into(),
                        },
                        Relation {
                            relation_info: RelationInfo::Index(index.to_owned()).into(),
                        },
                    ]
                    .into_iter()
                    .chain(internal_tables.into_iter().map(|internal_table| Relation {
                        relation_info: RelationInfo::Table(internal_table).into(),
                    }))
                    .collect_vec(),
                }),
            )
            .await;

        Ok(version)
    }

    pub async fn start_create_sink_procedure(&self, sink: &Sink) -> MetaResult<()> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let user_core = &mut core.user;
        database_core.ensure_database_id(sink.database_id)?;
        database_core.ensure_schema_id(sink.schema_id)?;
        for dependent_id in &sink.dependent_relations {
            database_core.ensure_table_view_or_source_id(dependent_id)?;
        }
        let key = (sink.database_id, sink.schema_id, sink.name.clone());
        database_core.check_relation_name_duplicated(&key)?;
        #[cfg(not(test))]
        user_core.ensure_user_id(sink.owner)?;

        if database_core.has_in_progress_creation(&key) {
            bail!("sink already in creating procedure");
        } else {
            database_core.mark_creating(&key);
            database_core.mark_creating_streaming_job(sink.id, key);
            for &dependent_relation_id in &sink.dependent_relations {
                database_core.increase_relation_ref_count(dependent_relation_id);
            }
            user_core.increase_ref(sink.owner);
            refcnt_inc_sink_secret_ref(database_core, sink);
            // We have validate the status of connection before starting the procedure.
            refcnt_inc_connection(database_core, sink.connection_id)?;
            Ok(())
        }
    }

    pub async fn finish_create_sink_procedure(
        &self,
        mut internal_tables: Vec<Table>,
        mut sink: Sink,
    ) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let key = (sink.database_id, sink.schema_id, sink.name.clone());
        let mut tables = BTreeMapTransaction::new(&mut database_core.tables);
        let mut sinks = BTreeMapTransaction::new(&mut database_core.sinks);
        assert!(
            !sinks.contains_key(&sink.id)
                && database_core.in_progress_creation_tracker.contains(&key),
            "sink must be in creating procedure"
        );

        database_core.in_progress_creation_tracker.remove(&key);
        database_core
            .in_progress_creating_streaming_job
            .remove(&sink.id);

        sink.stream_job_status = PbStreamJobStatus::Created.into();
        sinks.insert(sink.id, sink.clone());
        for table in &mut internal_tables {
            table.stream_job_status = PbStreamJobStatus::Created.into();
            tables.insert(table.id, table.clone());
        }
        commit_meta!(self, sinks, tables)?;

        let version = self
            .notify_frontend(
                Operation::Add,
                Info::RelationGroup(RelationGroup {
                    relations: vec![Relation {
                        relation_info: RelationInfo::Sink(sink.to_owned()).into(),
                    }]
                    .into_iter()
                    .chain(internal_tables.into_iter().map(|internal_table| Relation {
                        relation_info: RelationInfo::Table(internal_table).into(),
                    }))
                    .collect_vec(),
                }),
            )
            .await;

        Ok(version)
    }

    pub async fn cancel_create_sink_procedure(
        &self,
        sink: &Sink,
        target_table: &Option<(Table, Option<PbSource>)>,
    ) {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let user_core = &mut core.user;
        let key = (sink.database_id, sink.schema_id, sink.name.clone());
        assert!(
            !database_core.sinks.contains_key(&sink.id),
            "sink must be in creating procedure"
        );

        database_core.unmark_creating(&key);
        database_core.unmark_creating_streaming_job(sink.id);
        for &dependent_relation_id in &sink.dependent_relations {
            database_core.decrease_relation_ref_count(dependent_relation_id);
        }
        user_core.decrease_ref(sink.owner);
        refcnt_dec_connection(database_core, sink.connection_id);
        refcnt_dec_sink_secret_ref(database_core, sink);

        if let Some((table, source)) = target_table {
            Self::cancel_replace_table_procedure_inner(source, table, core);
        }
    }

    pub async fn start_create_subscription_procedure(
        &self,
        subscription: &Subscription,
    ) -> MetaResult<()> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let user_core = &mut core.user;
        database_core.ensure_database_id(subscription.database_id)?;
        database_core.ensure_schema_id(subscription.schema_id)?;
        database_core
            .ensure_table_view_or_source_id(&TableId::from(subscription.dependent_table_id))?;
        let key = (
            subscription.database_id,
            subscription.schema_id,
            subscription.name.clone(),
        );
        database_core.check_relation_name_duplicated(&key)?;
        #[cfg(not(test))]
        user_core.ensure_user_id(subscription.owner)?;

        if database_core.has_in_progress_creation(&key) {
            bail!("subscription already in creating procedure");
        } else {
            database_core.mark_creating(&key);
            database_core.mark_creating_streaming_job(subscription.id, key);
            database_core.increase_relation_ref_count(subscription.dependent_table_id);
            user_core.increase_ref(subscription.owner);
            let mut subscriptions = BTreeMapTransaction::new(&mut database_core.subscriptions);
            subscriptions.insert(subscription.id, subscription.clone());
            commit_meta!(self, subscriptions)?;
            Ok(())
        }
    }

    pub async fn finish_create_subscription_procedure(
        &self,
        subscription_id: SubscriptionId,
    ) -> MetaResult<()> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let mut subscriptions = BTreeMapTransaction::new(&mut database_core.subscriptions);
        let mut subscription = subscriptions
            .get(&subscription_id)
            .ok_or_else(|| MetaError::catalog_id_not_found("subscription", subscription_id))?
            .clone();
        subscription.created_at_cluster_version = Some(current_cluster_version());
        subscription.created_at_epoch = Some(Epoch::now().0);
        let key = (
            subscription.database_id,
            subscription.schema_id,
            subscription.name.clone(),
        );

        assert!(
            subscription.subscription_state == Into::<i32>::into(PbSubscriptionState::Init)
                && database_core.in_progress_creation_tracker.contains(&key),
            "subscription must be in creating procedure"
        );

        database_core.in_progress_creation_tracker.remove(&key);
        database_core
            .in_progress_creating_streaming_job
            .remove(&subscription.id);

        subscription.subscription_state = PbSubscriptionState::Created.into();
        subscriptions.insert(subscription.id, subscription.clone());
        commit_meta!(self, subscriptions)?;
        Ok(())
    }

    pub async fn notify_create_subscription(
        &self,
        subscription_id: SubscriptionId,
    ) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let subscriptions = BTreeMapTransaction::new(&mut database_core.subscriptions);
        let subscription = subscriptions
            .get(&subscription_id)
            .ok_or_else(|| MetaError::catalog_id_not_found("subscription", subscription_id))?
            .clone();
        assert_eq!(
            subscription.subscription_state,
            Into::<i32>::into(PbSubscriptionState::Created)
        );
        commit_meta!(self, subscriptions)?;

        let version = self
            .notify_frontend(
                Operation::Add,
                Info::RelationGroup(RelationGroup {
                    relations: vec![Relation {
                        relation_info: RelationInfo::Subscription(subscription.to_owned()).into(),
                    }],
                }),
            )
            .await;
        Ok(version)
    }

    pub async fn clean_dirty_subscription(&self) -> MetaResult<()> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let mut subscriptions = BTreeMapTransaction::new(&mut database_core.subscriptions);
        let remove_subscriptions = subscriptions
            .tree_ref()
            .iter()
            .filter(|(_, s)| s.subscription_state == Into::<i32>::into(PbSubscriptionState::Init))
            .map(|(_, s)| s.clone())
            .collect_vec();
        let user_core = &mut core.user;
        for s in &remove_subscriptions {
            subscriptions.remove(s.id);
        }
        commit_meta!(self, subscriptions)?;
        for subscription in remove_subscriptions {
            let key = (
                subscription.database_id,
                subscription.schema_id,
                subscription.name.clone(),
            );
            assert!(
                !database_core.subscriptions.contains_key(&subscription.id),
                "subscription must be in creating procedure"
            );

            database_core.unmark_creating(&key);
            database_core.unmark_creating_streaming_job(subscription.id);
            database_core.decrease_relation_ref_count(subscription.dependent_table_id);
            user_core.decrease_ref(subscription.owner);
        }
        Ok(())
    }

    /// This is used for `ALTER TABLE ADD/DROP COLUMN`.
    pub async fn start_replace_table_procedure(&self, stream_job: &StreamingJob) -> MetaResult<()> {
        let StreamingJob::Table(source, table, job_type) = stream_job else {
            unreachable!("unexpected job: {stream_job:?}")
        };
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        database_core.ensure_database_id(table.database_id)?;
        database_core.ensure_schema_id(table.schema_id)?;

        // general table streaming job should not have dependent relations
        if matches!(job_type, TableJobType::General) {
            assert!(table.dependent_relations.is_empty());
        }

        let key = (table.database_id, table.schema_id, table.name.clone());
        let original_table = database_core
            .get_table(table.id)
            .context("table to alter must exist")?;

        // Check whether the frontend is operating on the latest version of the table.
        if table.get_version()?.version != original_table.get_version()?.version + 1 {
            bail!("table version is stale");
        }

        // TODO: Here we reuse the `creation` tracker for `alter` procedure, as an `alter` must
        // occur after it's created. We may need to add a new tracker for `alter` procedure.
        if database_core.has_in_progress_creation(&key) {
            bail!("table is in altering procedure");
        } else {
            if let Some(source) = source {
                let source_key = (source.database_id, source.schema_id, source.name.clone());
                if database_core.has_in_progress_creation(&source_key) {
                    bail!("source is in altering procedure");
                }
                database_core.mark_creating(&source_key);
            }
            database_core.mark_creating(&key);
            Ok(())
        }
    }

    /// This is used for `ALTER TABLE ADD/DROP COLUMN` and `SINK INTO TABLE`.
    pub async fn finish_replace_table_procedure(
        &self,
        source: &Option<Source>,
        table: &Table,
        table_col_index_mapping: Option<ColIndexMapping>,
        creating_sink_id: Option<SinkId>,
        dropping_sink_id: Option<SinkId>,
        updated_sink_ids: Vec<SinkId>,
    ) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;
        let mut tables = BTreeMapTransaction::new(&mut database_core.tables);
        let mut sources = BTreeMapTransaction::new(&mut database_core.sources);
        let mut indexes = BTreeMapTransaction::new(&mut database_core.indexes);
        let mut sinks = BTreeMapTransaction::new(&mut database_core.sinks);
        let key = (table.database_id, table.schema_id, table.name.clone());

        assert!(
            tables.contains_key(&table.id)
                && database_core.in_progress_creation_tracker.contains(&key),
            "table must exist and be in altering procedure"
        );

        let original_table = tables.get(&table.id).unwrap();
        let mut updated_sinks = vec![];
        for sink_id in updated_sink_ids {
            let mut sink = sinks.get_mut(sink_id).unwrap();
            sink.original_target_columns
                .clone_from(&original_table.columns);
            updated_sinks.push(sink.clone());
        }

        if let Some(source) = source {
            let source_key = (source.database_id, source.schema_id, source.name.clone());
            assert!(
                sources.contains_key(&source.id)
                    && database_core
                        .in_progress_creation_tracker
                        .contains(&source_key),
                "source must exist and be in altering procedure"
            );
            sources.insert(source.id, source.clone());
            database_core
                .in_progress_creation_tracker
                .remove(&source_key);
        }

        let index_ids: Vec<_> = indexes
            .tree_ref()
            .iter()
            .filter(|(_, index)| index.primary_table_id == table.id)
            .map(|(index_id, _index)| *index_id)
            .collect_vec();

        let mut updated_indexes = vec![];

        if let Some(table_col_index_mapping) = table_col_index_mapping {
            let expr_rewriter = ReplaceTableExprRewriter {
                table_col_index_mapping,
            };

            for index_id in &index_ids {
                let mut index = indexes.get_mut(*index_id).unwrap();
                index
                    .index_item
                    .iter_mut()
                    .for_each(|x| expr_rewriter.rewrite_expr(x));
                updated_indexes.push(indexes.get(index_id).cloned().unwrap());
            }
        }

        // TODO: Here we reuse the `creation` tracker for `alter` procedure, as an `alter` must
        database_core.in_progress_creation_tracker.remove(&key);

        let mut table = table.clone();
        table.stream_job_status = PbStreamJobStatus::Created.into();

        if let Some(incoming_sink_id) = creating_sink_id {
            table.incoming_sinks.push(incoming_sink_id);
        }

        if let Some(dropping_sink_id) = dropping_sink_id {
            table
                .incoming_sinks
                .retain(|sink_id| *sink_id != dropping_sink_id);
        }

        tables.insert(table.id, table.clone());

        commit_meta!(self, tables, indexes, sources, sinks)?;

        // Group notification
        let version = self
            .notify_frontend(
                Operation::Update,
                Info::RelationGroup(RelationGroup {
                    relations: vec![Relation {
                        relation_info: RelationInfo::Table(table).into(),
                    }]
                    .into_iter()
                    .chain(source.iter().map(|source| Relation {
                        relation_info: RelationInfo::Source(source.to_owned()).into(),
                    }))
                    .chain(updated_indexes.into_iter().map(|index| Relation {
                        relation_info: RelationInfo::Index(index).into(),
                    }))
                    .chain(updated_sinks.into_iter().map(|sink| Relation {
                        relation_info: RelationInfo::Sink(sink).into(),
                    }))
                    .collect_vec(),
                }),
            )
            .await;

        Ok(version)
    }

    /// This is used for `ALTER TABLE ADD/DROP COLUMN`.
    pub async fn cancel_replace_table_procedure(
        &self,
        stream_job: &StreamingJob,
    ) -> MetaResult<()> {
        let StreamingJob::Table(source, table, ..) = stream_job else {
            unreachable!("unexpected job: {stream_job:?}")
        };
        let core = &mut *self.core.lock().await;

        Self::cancel_replace_table_procedure_inner(source, table, core);
        Ok(())
    }

    fn cancel_replace_table_procedure_inner(
        source: &Option<PbSource>,
        table: &Table,
        core: &mut CatalogManagerCore,
    ) {
        let database_core = &mut core.database;
        let key = (table.database_id, table.schema_id, table.name.clone());

        assert!(
            database_core.tables.contains_key(&table.id)
                && database_core.has_in_progress_creation(&key),
            "table must exist and must be in altering procedure"
        );

        if let Some(source) = source {
            let source_key = (source.database_id, source.schema_id, source.name.clone());
            assert!(
                database_core.sources.contains_key(&source.id)
                    && database_core.has_in_progress_creation(&source_key),
                "source must exist and must be in altering procedure"
            );

            database_core.unmark_creating(&source_key);
        }

        // TODO: Here we reuse the `creation` tracker for `alter` procedure, as an `alter` must
        // occur after it's created. We may need to add a new tracker for `alter` procedure.s
        database_core.unmark_creating(&key);
    }

    pub async fn comment_on(&self, comment: Comment) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let database_core = &mut core.database;

        database_core.ensure_database_id(comment.database_id)?;
        database_core.ensure_schema_id(comment.schema_id)?;
        database_core.ensure_table_id(comment.table_id)?;

        let mut tables = BTreeMapTransaction::new(&mut database_core.tables);

        // unwrap is safe because the table id was ensured before
        let mut table = tables.get_mut(comment.table_id).unwrap();
        if let Some(col_idx) = comment.column_index {
            let column = table
                .columns
                .get_mut(col_idx as usize)
                .ok_or_else(|| MetaError::catalog_id_not_found("column", col_idx))?;
            let column_desc = column.column_desc.as_mut().ok_or_else(|| {
                anyhow!(
                    "column desc at index {} for table id {} not found",
                    col_idx,
                    comment.table_id
                )
            })?;
            column_desc.description = comment.description;
        } else {
            table.description = comment.description;
        }

        let new_table = table.clone();

        commit_meta!(self, tables)?;

        let version = self
            .notify_frontend_relation_info(Operation::Update, RelationInfo::Table(new_table))
            .await;

        Ok(version)
    }

    pub async fn list_connections(&self) -> Vec<Connection> {
        self.core.lock().await.database.list_connections()
    }

    pub async fn list_databases(&self) -> Vec<Database> {
        self.core.lock().await.database.list_databases()
    }

    pub async fn list_schemas(&self) -> Vec<Schema> {
        self.core.lock().await.database.list_schemas()
    }

    pub async fn list_tables(&self) -> Vec<Table> {
        self.core.lock().await.database.list_tables()
    }

    pub async fn list_stream_job_for_telemetry(&self) -> MetaResult<Vec<MetaTelemetryJobDesc>> {
        let tables = self.list_tables().await;
        let mut res = Vec::with_capacity(tables.len());
        let source_read_lock = self.core.lock().await;
        for table_def in tables {
            // filter out internal tables, only allow Table and MaterializedView
            if !(table_def.table_type == TableType::Table as i32
                || table_def.table_type == TableType::MaterializedView as i32)
            {
                continue;
            }
            if let Some(OptionalAssociatedSourceId::AssociatedSourceId(source_id)) =
                table_def.optional_associated_source_id
                && let Some(source) = source_read_lock.database.sources.get(&source_id)
            {
                res.push(MetaTelemetryJobDesc {
                    table_id: table_def.id as i32,
                    connector: source
                        .with_properties
                        .get(UPSTREAM_SOURCE_KEY)
                        .map(|v| v.to_lowercase()),
                    optimization: vec![],
                })
            } else {
                res.push(MetaTelemetryJobDesc {
                    table_id: table_def.id as i32,
                    connector: None,
                    optimization: vec![],
                })
            }
        }

        Ok(res)
    }

    pub async fn list_tables_by_type(&self, table_type: TableType) -> Vec<Table> {
        self.core
            .lock()
            .await
            .database
            .tables
            .values()
            .filter(|table| table.table_type == table_type as i32)
            .cloned()
            .collect_vec()
    }

    /// Lists table catalogs for mviews, without their internal tables.
    pub async fn list_creating_background_mvs(&self) -> Vec<Table> {
        self.core
            .lock()
            .await
            .database
            .list_creating_background_mvs()
    }

    /// Lists table catalogs for all tables with `stream_job_status=CREATING`.
    pub async fn list_persisted_creating_tables(&self) -> Vec<Table> {
        self.core
            .lock()
            .await
            .database
            .list_persisted_creating_tables()
    }

    pub async fn get_all_table_options(&self) -> HashMap<TableId, TableOption> {
        self.core.lock().await.database.get_all_table_options()
    }

    pub async fn list_readonly_table_ids(&self, schema_id: SchemaId) -> Vec<TableId> {
        self.core
            .lock()
            .await
            .database
            .list_readonly_table_ids(schema_id)
    }

    pub async fn list_dml_table_ids(&self, schema_id: SchemaId) -> Vec<TableId> {
        self.core
            .lock()
            .await
            .database
            .list_dml_table_ids(schema_id)
    }

    pub async fn list_view_ids(&self, schema_id: SchemaId) -> Vec<TableId> {
        self.core.lock().await.database.list_view_ids(schema_id)
    }

    pub async fn list_sources(&self) -> Vec<Source> {
        self.core.lock().await.database.list_sources()
    }

    pub async fn list_sinks(&self) -> Vec<Sink> {
        self.core.lock().await.database.list_sinks()
    }

    pub async fn list_subscriptions(&self) -> Vec<Subscription> {
        self.core.lock().await.database.list_subscriptions()
    }

    pub async fn list_views(&self) -> Vec<View> {
        self.core.lock().await.database.list_views()
    }

    pub async fn list_source_ids(&self, schema_id: SchemaId) -> Vec<SourceId> {
        self.core.lock().await.database.list_source_ids(schema_id)
    }

    pub async fn get_table_name_and_type_mapping(&self) -> HashMap<TableId, (String, String)> {
        self.core
            .lock()
            .await
            .database
            .get_table_name_and_type_mapping()
    }

    pub async fn get_table_by_cdc_table_id(&self, cdc_table_id: &String) -> Vec<Table> {
        self.core
            .lock()
            .await
            .database
            .get_table_by_cdc_table_id(cdc_table_id)
    }

    /// `list_stream_job_ids` returns all running and creating stream job ids, this is for recovery
    /// clean up progress.
    pub async fn list_stream_job_ids(&self) -> MetaResult<HashSet<TableId>> {
        let guard = self.core.lock().await;
        let mut all_streaming_jobs: HashSet<TableId> =
            guard.database.list_stream_job_ids().collect();

        all_streaming_jobs.extend(guard.database.all_creating_streaming_jobs());
        Ok(all_streaming_jobs)
    }

    pub async fn find_creating_streaming_job_ids(
        &self,
        infos: Vec<CreatingJobInfo>,
    ) -> Vec<TableId> {
        let guard = self.core.lock().await;
        infos
            .into_iter()
            .flat_map(|info| {
                let relation_key = &(info.database_id, info.schema_id, info.name);
                guard
                    .database
                    .find_creating_streaming_job_id(relation_key)
                    .or_else(|| {
                        guard
                            .database
                            .find_persisted_creating_table_id(relation_key)
                    })
            })
            .collect_vec()
    }

    pub async fn list_object_dependencies(&self) -> Vec<PbObjectDependencies> {
        let core = &self.core.lock().await.database;
        let mut dependencies = vec![];
        for table in core.tables.values() {
            let table_type = table.get_table_type().unwrap();
            let job_status = table.get_stream_job_status().unwrap();
            if table_type != TableType::Internal && job_status != StreamJobStatus::Creating {
                for referenced in &table.dependent_relations {
                    dependencies.push(PbObjectDependencies {
                        object_id: table.id,
                        referenced_object_id: *referenced,
                    });
                }
                for incoming_sinks in &table.incoming_sinks {
                    dependencies.push(PbObjectDependencies {
                        object_id: table.id,
                        referenced_object_id: *incoming_sinks,
                    });
                }
            }
        }
        for sink in core.sinks.values() {
            for referenced in &sink.dependent_relations {
                dependencies.push(PbObjectDependencies {
                    object_id: sink.id,
                    referenced_object_id: *referenced,
                });
            }
        }

        for subscription in core.subscriptions.values() {
            dependencies.push(PbObjectDependencies {
                object_id: subscription.id,
                referenced_object_id: subscription.dependent_table_id,
            });
        }

        dependencies
    }

    async fn notify_frontend(&self, operation: Operation, info: Info) -> NotificationVersion {
        self.env
            .notification_manager()
            .notify_frontend(operation, info)
            .await
    }

    async fn notify_frontend_relation_info(
        &self,
        operation: Operation,
        relation_info: RelationInfo,
    ) -> NotificationVersion {
        self.env
            .notification_manager()
            .notify_frontend_relation_info(operation, relation_info)
            .await
    }

    pub async fn table_is_created(&self, table_id: TableId) -> bool {
        let guard = self.core.lock().await;
        return if let Some(table) = guard.database.tables.get(&table_id) {
            table.get_stream_job_status() != Ok(StreamJobStatus::Creating)
        } else {
            false
        };
    }

    pub async fn get_tables(&self, table_ids: &[TableId]) -> Vec<Table> {
        let mut tables = vec![];
        let guard = self.core.lock().await;
        for table_id in table_ids {
            if let Some(table) = guard.database.in_progress_creating_tables.get(table_id) {
                tables.push(table.clone());
            } else if let Some(table) = guard.database.tables.get(table_id) {
                tables.push(table.clone());
            }
        }
        tables
    }

    pub async fn get_subscription_by_id(
        &self,
        subscription_id: SubscriptionId,
    ) -> MetaResult<Subscription> {
        let guard = self.core.lock().await;
        let subscription = guard
            .database
            .subscriptions
            .get(&subscription_id)
            .ok_or_else(|| MetaError::catalog_id_not_found("subscription", subscription_id))?;
        Ok(subscription.clone())
    }

    pub async fn get_mv_depended_subscriptions(
        &self,
    ) -> MetaResult<HashMap<risingwave_common::catalog::TableId, HashMap<u32, u64>>> {
        let guard = self.core.lock().await;
        let mut map = HashMap::new();
        for subscription in guard.database.subscriptions.values() {
            map.entry(risingwave_common::catalog::TableId::from(
                subscription.dependent_table_id,
            ))
            .or_insert(HashMap::new())
            .insert(subscription.id, subscription.retention_seconds);
        }

        Ok(map)
    }

    pub async fn get_sinks(&self, sink_ids: &[SinkId]) -> Vec<Sink> {
        let mut sinks = vec![];
        let guard = self.core.lock().await;
        for sink_id in sink_ids {
            if let Some(sink) = guard.database.sinks.get(sink_id) {
                sinks.push(sink.clone());
            }
        }
        sinks
    }

    pub async fn get_created_table_ids(&self) -> Vec<u32> {
        let guard = self.core.lock().await;
        guard
            .database
            .tables
            .values()
            .filter(|table| table.get_stream_job_status() != Ok(StreamJobStatus::Creating))
            .map(|table| table.id)
            .collect()
    }

    /// Returns column ids of `table_ids` that is versioned.
    /// Being versioned implies using `ColumnAwareSerde`.
    pub async fn get_versioned_table_schemas(
        &self,
        table_ids: &[TableId],
    ) -> HashMap<TableId, Vec<i32>> {
        let guard = self.core.lock().await;
        table_ids
            .iter()
            .filter_map(|table_id| {
                if let Some(t) = guard.database.tables.get(table_id)
                    && t.version.is_some()
                {
                    let ret = (
                        t.id,
                        t.columns
                            .iter()
                            .map(|c| c.column_desc.as_ref().unwrap().column_id)
                            .collect_vec(),
                    );
                    return Some(ret);
                }
                None
            })
            .collect()
    }

    // TODO: replace *_count with SQL
    #[cfg_attr(coverage, coverage(off))]
    pub async fn source_count(&self) -> usize {
        self.core.lock().await.database.sources.len()
    }

    #[cfg_attr(coverage, coverage(off))]
    pub async fn table_count(&self) -> usize {
        self.core
            .lock()
            .await
            .database
            .tables
            .values()
            .filter(|t| t.table_type == TableType::Table as i32)
            .count()
    }

    #[cfg_attr(coverage, coverage(off))]
    pub async fn materialized_view_count(&self) -> usize {
        self.core
            .lock()
            .await
            .database
            .tables
            .values()
            .filter(|t| t.table_type == TableType::MaterializedView as i32)
            .count()
    }

    #[cfg_attr(coverage, coverage(off))]
    pub async fn index_count(&self) -> usize {
        self.core.lock().await.database.indexes.len()
    }

    #[cfg_attr(coverage, coverage(off))]
    pub async fn sink_count(&self) -> usize {
        self.core.lock().await.database.sinks.len()
    }

    #[cfg_attr(coverage, coverage(off))]
    pub async fn subscription_count(&self) -> usize {
        self.core.lock().await.database.subscriptions.len()
    }

    #[cfg_attr(coverage, coverage(off))]
    pub async fn function_count(&self) -> usize {
        self.core.lock().await.database.functions.len()
    }
}

// User related methods
impl CatalogManager {
    async fn init_user(&self) -> MetaResult<()> {
        let core = &mut self.core.lock().await.user;
        for (user, id) in [
            (DEFAULT_SUPER_USER, DEFAULT_SUPER_USER_ID),
            (DEFAULT_SUPER_USER_FOR_PG, DEFAULT_SUPER_USER_FOR_PG_ID),
        ] {
            if !core.has_user_name(user) {
                let default_user = UserInfo {
                    id,
                    name: user.to_string(),
                    is_super: true,
                    can_create_db: true,
                    can_create_user: true,
                    can_login: true,
                    ..Default::default()
                };

                default_user.insert(self.env.meta_store().as_kv()).await?;
                core.user_info.insert(default_user.id, default_user);
            }
        }

        Ok(())
    }

    pub async fn list_users(&self) -> Vec<UserInfo> {
        self.core.lock().await.user.list_users()
    }

    pub async fn create_user(&self, user: &UserInfo) -> MetaResult<NotificationVersion> {
        let core = &mut self.core.lock().await.user;
        if core.has_user_name(&user.name) {
            return Err(MetaError::permission_denied(format!(
                "User {} already exists",
                user.name
            )));
        }
        let mut users = BTreeMapTransaction::new(&mut core.user_info);
        users.insert(user.id, user.clone());
        commit_meta!(self, users)?;

        let version = self
            .notify_frontend(Operation::Add, Info::User(user.to_owned()))
            .await;
        Ok(version)
    }

    pub async fn update_user(
        &self,
        update_user: &UserInfo,
        update_fields: &[UpdateField],
    ) -> MetaResult<NotificationVersion> {
        let core = &mut self.core.lock().await.user;
        let rename_flag = update_fields
            .iter()
            .any(|&field| field == UpdateField::Rename);
        if rename_flag && core.has_user_name(&update_user.name) {
            return Err(MetaError::permission_denied(format!(
                "User {} already exists",
                update_user.name
            )));
        }

        let mut users = BTreeMapTransaction::new(&mut core.user_info);
        let mut user = users.get_mut(update_user.id).unwrap();

        update_fields.iter().for_each(|&field| match field {
            UpdateField::Unspecified => unreachable!(),
            UpdateField::Super => user.is_super = update_user.is_super,
            UpdateField::Login => user.can_login = update_user.can_login,
            UpdateField::CreateDb => user.can_create_db = update_user.can_create_db,
            UpdateField::CreateUser => user.can_create_user = update_user.can_create_user,
            UpdateField::AuthInfo => user.auth_info.clone_from(&update_user.auth_info),
            UpdateField::Rename => {
                user.name.clone_from(&update_user.name);
            }
        });

        let new_user: UserInfo = user.clone();

        commit_meta!(self, users)?;

        let version = self
            .notify_frontend(Operation::Update, Info::User(new_user))
            .await;
        Ok(version)
    }

    #[cfg(test)]
    pub async fn get_user(&self, id: UserId) -> MetaResult<UserInfo> {
        let core = &self.core.lock().await.user;
        core.user_info
            .get(&id)
            .cloned()
            .ok_or_else(|| MetaError::catalog_id_not_found("user", id))
    }

    pub async fn drop_user(&self, id: UserId) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let user_core = &mut core.user;
        let mut users = BTreeMapTransaction::new(&mut user_core.user_info);
        if !users.contains_key(&id) {
            bail!("User {} not found", id);
        }

        let user = users.remove(id).unwrap();

        if user.name == DEFAULT_SUPER_USER || user.name == DEFAULT_SUPER_USER_FOR_PG {
            return Err(MetaError::permission_denied(format!(
                "Cannot drop default super user {}",
                id
            )));
        }
        if user_core.catalog_create_ref_count.contains_key(&id) {
            return Err(MetaError::permission_denied(format!(
                "User {} cannot be dropped because some objects depend on it",
                user.name
            )));
        }
        if user_core
            .user_grant_relation
            .get(&id)
            .is_some_and(|set| !set.is_empty())
        {
            return Err(MetaError::permission_denied(format!(
                "Cannot drop user {} with privileges granted to others",
                id
            )));
        }

        commit_meta!(self, users)?;

        let version = self
            .notify_frontend(Operation::Delete, Info::User(user))
            .await;
        Ok(version)
    }

    // Defines privilege grant for a user.

    // Merge new granted privilege.
    #[inline(always)]
    fn merge_privilege(origin_privilege: &mut GrantPrivilege, new_privilege: &GrantPrivilege) {
        assert_eq!(origin_privilege.object, new_privilege.object);

        let mut action_map = HashMap::<i32, (bool, u32)>::from_iter(
            origin_privilege
                .action_with_opts
                .iter()
                .map(|ao| (ao.action, (ao.with_grant_option, ao.granted_by))),
        );
        for nao in &new_privilege.action_with_opts {
            if let Some(o) = action_map.get_mut(&nao.action) {
                o.0 |= nao.with_grant_option;
            } else {
                action_map.insert(nao.action, (nao.with_grant_option, nao.granted_by));
            }
        }
        origin_privilege.action_with_opts = action_map
            .into_iter()
            .map(
                |(action, (with_grant_option, granted_by))| ActionWithGrantOption {
                    action,
                    with_grant_option,
                    granted_by,
                },
            )
            .collect();
    }

    // Check whether new_privilege is a subset of origin_privilege, and check grand_option if
    // `need_grand_option` is set.
    #[inline(always)]
    fn check_privilege(
        origin_privilege: &GrantPrivilege,
        new_privilege: &GrantPrivilege,
        need_grand_option: bool,
    ) -> bool {
        assert_eq!(origin_privilege.object, new_privilege.object);

        let action_map = HashMap::<i32, bool>::from_iter(
            origin_privilege
                .action_with_opts
                .iter()
                .map(|ao| (ao.action, ao.with_grant_option)),
        );
        for nao in &new_privilege.action_with_opts {
            if let Some(with_grant_option) = action_map.get(&nao.action) {
                if !with_grant_option && need_grand_option {
                    return false;
                }
            } else {
                return false;
            }
        }
        true
    }

    /// Check whether the user is the owner of the object.
    #[inline(always)]
    fn check_owner(
        database_manager: &DatabaseManager,
        object: &Object,
        user_id: UserId,
    ) -> MetaResult<bool> {
        database_manager
            .get_object_owner(object)
            .map(|owner_id| owner_id == user_id)
    }

    pub async fn grant_privilege(
        &self,
        user_ids: &[UserId],
        new_grant_privileges: &[GrantPrivilege],
        grantor: UserId,
    ) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let user_core = &mut core.user;
        let catalog_core = &core.database;
        let mut users = BTreeMapTransaction::new(&mut user_core.user_info);
        let mut user_updated = Vec::with_capacity(user_ids.len());
        let grantor_info = users
            .get(&grantor)
            .cloned()
            .ok_or_else(|| MetaError::catalog_id_not_found("user", grantor))?;
        for user_id in user_ids {
            let mut user = users
                .get_mut(*user_id)
                .ok_or_else(|| MetaError::catalog_id_not_found("user", user_id))?;

            if user.is_super {
                return Err(MetaError::permission_denied(format!(
                    "Cannot grant privilege to super user {}",
                    user_id
                )));
            }
            if !grantor_info.is_super {
                for new_grant_privilege in new_grant_privileges {
                    if Self::check_owner(
                        catalog_core,
                        new_grant_privilege.object.as_ref().unwrap(),
                        grantor,
                    )? {
                        continue;
                    }
                    if let Some(privilege) = grantor_info
                        .grant_privileges
                        .iter()
                        .find(|p| p.object == new_grant_privilege.object)
                    {
                        if !Self::check_privilege(privilege, new_grant_privilege, true) {
                            return Err(MetaError::permission_denied(format!(
                                "Cannot grant privilege without grant permission for user {}",
                                grantor
                            )));
                        }
                    } else {
                        return Err(MetaError::permission_denied(format!(
                            "Grantor {} does not have one of the privileges",
                            grantor
                        )));
                    }
                }
            }
            new_grant_privileges.iter().for_each(|new_grant_privilege| {
                if let Some(privilege) = user
                    .grant_privileges
                    .iter_mut()
                    .find(|p| p.object == new_grant_privilege.object)
                {
                    Self::merge_privilege(privilege, new_grant_privilege);
                } else {
                    user.grant_privileges.push(new_grant_privilege.clone());
                }
            });
            user_updated.push(user.clone());
        }

        commit_meta!(self, users)?;

        let grant_user = user_core
            .user_grant_relation
            .entry(grantor)
            .or_insert_with(HashSet::new);
        grant_user.extend(user_ids);

        let mut version = 0;
        // FIXME: user might not be updated.
        for user in user_updated {
            version = self
                .notify_frontend(Operation::Update, Info::User(user))
                .await;
        }

        Ok(version)
    }

    // Revoke privilege from object.
    #[inline(always)]
    fn revoke_privilege_inner(
        origin_privilege: &mut GrantPrivilege,
        revoke_grant_privilege: &GrantPrivilege,
        revoke_grant_option: bool,
    ) -> bool {
        assert_eq!(origin_privilege.object, revoke_grant_privilege.object);
        let mut has_change = false;
        if revoke_grant_option {
            // Only revoke with grant option.
            origin_privilege.action_with_opts.iter_mut().for_each(|ao| {
                if revoke_grant_privilege
                    .action_with_opts
                    .iter()
                    .any(|ro| ro.action == ao.action)
                {
                    ao.with_grant_option = false;
                    has_change = true;
                }
            })
        } else {
            let sz = origin_privilege.action_with_opts.len();
            // Revoke all privileges matched with revoke_grant_privilege.
            origin_privilege.action_with_opts.retain(|ao| {
                !revoke_grant_privilege
                    .action_with_opts
                    .iter()
                    .any(|rao| rao.action == ao.action)
            });
            has_change = sz != origin_privilege.action_with_opts.len();
        }
        has_change
    }

    pub async fn revoke_privilege(
        &self,
        user_ids: &[UserId],
        revoke_grant_privileges: &[GrantPrivilege],
        granted_by: UserId,
        revoke_by: UserId,
        revoke_grant_option: bool,
        cascade: bool,
    ) -> MetaResult<NotificationVersion> {
        let core = &mut *self.core.lock().await;
        let user_core = &mut core.user;
        let catalog_core = &core.database;
        let mut users = BTreeMapTransaction::new(&mut user_core.user_info);
        let mut user_updated = HashMap::new();
        let mut users_info: VecDeque<UserInfo> = VecDeque::new();
        let mut visited = HashSet::new();
        // check revoke permission
        let revoke_by = users
            .get(&revoke_by)
            .ok_or_else(|| MetaError::catalog_id_not_found("user", revoke_by))?;
        let same_user = granted_by == revoke_by.id;
        if !revoke_by.is_super {
            for privilege in revoke_grant_privileges {
                if Self::check_owner(
                    catalog_core,
                    privilege.object.as_ref().unwrap(),
                    revoke_by.id,
                )? {
                    continue;
                }
                if let Some(user_privilege) = revoke_by
                    .grant_privileges
                    .iter()
                    .find(|p| p.object == privilege.object)
                {
                    if !Self::check_privilege(user_privilege, privilege, same_user) {
                        return Err(MetaError::permission_denied(format!(
                            "Cannot revoke privilege without permission for user {}",
                            &revoke_by.name
                        )));
                    }
                } else {
                    return Err(MetaError::permission_denied(format!(
                        "User {} does not have one of the privileges",
                        &revoke_by.name
                    )));
                }
            }
        }
        // revoke privileges
        for user_id in user_ids {
            let user = users
                .get(user_id)
                .cloned()
                .ok_or_else(|| MetaError::catalog_id_not_found("user", user_id))?;
            if user.is_super {
                return Err(MetaError::permission_denied(format!(
                    "Cannot revoke privilege from supper user {}",
                    user_id
                )));
            }
            users_info.push_back(user);
        }
        while !users_info.is_empty() {
            let mut cur_user = users_info.pop_front().unwrap();
            let cur_relations = user_core
                .user_grant_relation
                .get(&cur_user.id)
                .cloned()
                .unwrap_or_default();
            let mut recursive_flag = false;
            let mut empty_privilege = false;
            let cur_revoke_grant_option = revoke_grant_option && user_ids.contains(&cur_user.id);
            visited.insert(cur_user.id);
            revoke_grant_privileges
                .iter()
                .for_each(|revoke_grant_privilege| {
                    for privilege in &mut cur_user.grant_privileges {
                        if privilege.object == revoke_grant_privilege.object {
                            recursive_flag |= Self::revoke_privilege_inner(
                                privilege,
                                revoke_grant_privilege,
                                cur_revoke_grant_option,
                            );
                            empty_privilege |= privilege.action_with_opts.is_empty();
                            break;
                        }
                    }
                });
            if recursive_flag {
                // check with cascade/restrict strategy
                if !cascade && !user_ids.contains(&cur_user.id) {
                    return Err(MetaError::permission_denied(format!(
                        "Cannot revoke privilege from user {} for restrict",
                        &cur_user.name
                    )));
                }
                for next_user_id in cur_relations {
                    if users.contains_key(&next_user_id) && !visited.contains(&next_user_id) {
                        users_info.push_back(users.get(&next_user_id).cloned().unwrap());
                    }
                }
                if empty_privilege {
                    cur_user
                        .grant_privileges
                        .retain(|privilege| !privilege.action_with_opts.is_empty());
                }
                if let std::collections::hash_map::Entry::Vacant(e) =
                    user_updated.entry(cur_user.id)
                {
                    users.insert(cur_user.id, cur_user.clone());
                    e.insert(cur_user);
                }
            }
        }

        commit_meta!(self, users)?;

        // Since we might revoke privileges recursively, just simply re-build the grant relation
        // map here.
        user_core.build_grant_relation_map();

        let mut version = 0;
        // FIXME: user might not be updated.
        for (_, user_info) in user_updated {
            version = self
                .notify_frontend(Operation::Update, Info::User(user_info))
                .await;
        }

        Ok(version)
    }

    /// `update_user_privileges` removes the privileges with given object from given users, it will
    /// be called when a database/schema/table/source/sink is dropped.
    #[inline(always)]
    fn update_user_privileges(
        users: &mut BTreeMapTransaction<'_, UserId, UserInfo>,
        objects: &[Object],
    ) -> Vec<UserInfo> {
        let mut users_need_update = vec![];
        let user_keys = users.tree_ref().keys().copied().collect_vec();
        for user_id in user_keys {
            let mut user = users.get_mut(user_id).unwrap();
            let mut new_grant_privileges = user.grant_privileges.clone();
            new_grant_privileges.retain(|p| !objects.contains(p.object.as_ref().unwrap()));
            if new_grant_privileges.len() != user.grant_privileges.len() {
                user.grant_privileges = new_grant_privileges;
                users_need_update.push(user.clone());
            }
        }
        users_need_update
    }

    pub async fn update_source_rate_limit_by_source_id(
        &self,
        source_id: SourceId,
        rate_limit: Option<u32>,
    ) -> MetaResult<()> {
        let source_relation: PbSource;
        {
            let core = &mut *self.core.lock().await;
            let database_core = &mut core.database;
            let mut sources = BTreeMapTransaction::new(&mut database_core.sources);
            let mut source = sources.get_mut(source_id);
            let Some(source_catalog) = source.as_mut() else {
                bail!("source {} not found", source_id)
            };
            source_relation = source_catalog.clone();
            source_catalog.rate_limit = rate_limit;
            commit_meta!(self, sources)?;
        }

        let _version = self
            .notify_frontend(
                Operation::Update,
                Info::RelationGroup(RelationGroup {
                    relations: vec![Relation {
                        relation_info: RelationInfo::Source(source_relation).into(),
                    }],
                }),
            )
            .await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::manager::catalog::extract_external_table_name_from_definition;

    #[test]
    fn test_extract_cdc_table_name() {
        let ddl1 = "CREATE TABLE t1 () FROM pg_source TABLE 'public.t1'";
        let ddl2 = "CREATE TABLE t2 (v1 int) FROM pg_source TABLE 'mydb.t2'";
        assert_eq!(
            extract_external_table_name_from_definition(ddl1),
            Some("public.t1".into())
        );
        assert_eq!(
            extract_external_table_name_from_definition(ddl2),
            Some("mydb.t2".into())
        );
    }
}
