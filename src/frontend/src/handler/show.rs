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

use std::sync::Arc;

use itertools::Itertools;
use pgwire::pg_field_descriptor::PgFieldDescriptor;
use pgwire::pg_protocol::truncated_fmt;
use pgwire::pg_response::{PgResponse, StatementType};
use pgwire::pg_server::Session;
use risingwave_common::bail_not_implemented;
use risingwave_common::catalog::{ColumnCatalog, ColumnDesc, DEFAULT_SCHEMA_NAME};
use risingwave_common::session_config::{SearchPath, USER_NAME_WILD_CARD};
use risingwave_common::types::{DataType, Fields, Timestamptz};
use risingwave_common::util::addr::HostAddr;
use risingwave_connector::source::kafka::PRIVATELINK_CONNECTION;
use risingwave_expr::scalar::like::{i_like_default, like_default};
use risingwave_pb::catalog::connection;
use risingwave_sqlparser::ast::{
    display_comma_separated, Ident, ObjectName, ShowCreateType, ShowObject, ShowStatementFilter,
};

use super::{fields_to_descriptors, PgResponseStream, RwPgResponse, RwPgResponseBuilderExt};
use crate::binder::{Binder, Relation};
use crate::catalog::{CatalogError, IndexCatalog};
use crate::error::Result;
use crate::handler::HandlerArgs;
use crate::session::SessionImpl;

pub fn get_columns_from_table(
    session: &SessionImpl,
    table_name: ObjectName,
) -> Result<Vec<ColumnCatalog>> {
    let mut binder = Binder::new_for_system(session);
    let relation = binder.bind_relation_by_name(table_name.clone(), None, None)?;
    let column_catalogs = match relation {
        Relation::Source(s) => s.catalog.columns,
        Relation::BaseTable(t) => t.table_catalog.columns.clone(),
        Relation::SystemTable(t) => t.sys_table_catalog.columns.clone(),
        _ => {
            return Err(CatalogError::NotFound("table or source", table_name.to_string()).into());
        }
    };

    Ok(column_catalogs)
}

pub fn get_columns_from_sink(
    session: &SessionImpl,
    sink_name: ObjectName,
) -> Result<Vec<ColumnCatalog>> {
    let binder = Binder::new_for_system(session);
    let sink = binder.bind_sink_by_name(sink_name.clone())?;
    Ok(sink.sink_catalog.full_columns().to_vec())
}

pub fn get_columns_from_view(
    session: &SessionImpl,
    view_name: ObjectName,
) -> Result<Vec<ColumnCatalog>> {
    let binder = Binder::new_for_system(session);
    let view = binder.bind_view_by_name(view_name.clone())?;

    Ok(view
        .view_catalog
        .columns
        .iter()
        .enumerate()
        .map(|(idx, field)| ColumnCatalog {
            column_desc: ColumnDesc::from_field_with_column_id(field, idx as _),
            is_hidden: false,
        })
        .collect())
}

pub fn get_indexes_from_table(
    session: &SessionImpl,
    table_name: ObjectName,
) -> Result<Vec<Arc<IndexCatalog>>> {
    let mut binder = Binder::new_for_system(session);
    let relation = binder.bind_relation_by_name(table_name.clone(), None, None)?;
    let indexes = match relation {
        Relation::BaseTable(t) => t.table_indexes,
        _ => {
            return Err(CatalogError::NotFound("table or source", table_name.to_string()).into());
        }
    };

    Ok(indexes)
}

fn schema_or_default(schema: &Option<Ident>) -> String {
    schema
        .as_ref()
        .map_or_else(|| DEFAULT_SCHEMA_NAME.to_string(), |s| s.real_value())
}

fn schema_or_search_path(
    session: &Arc<SessionImpl>,
    schema: &Option<Ident>,
    search_path: &SearchPath,
) -> Vec<String> {
    if let Some(s) = schema {
        vec![s.real_value()]
    } else {
        search_path
            .real_path()
            .iter()
            .map(|s| {
                if s.eq(USER_NAME_WILD_CARD) {
                    session.auth_context().user_name.clone()
                } else {
                    s.to_string()
                }
            })
            .collect()
    }
}

#[derive(Fields)]
#[fields(style = "Title Case")]
struct ShowObjectRow {
    name: String,
}

#[derive(Fields)]
#[fields(style = "Title Case")]
pub struct ShowColumnRow {
    pub name: String,
    pub r#type: String,
    pub is_hidden: Option<String>,
    pub description: Option<String>,
}

impl ShowColumnRow {
    pub fn from_catalog(col: ColumnCatalog) -> Vec<Self> {
        col.column_desc
            .flatten()
            .into_iter()
            .map(|c| {
                let type_name = if let DataType::Struct { .. } = c.data_type {
                    c.type_name.clone()
                } else {
                    c.data_type.to_string()
                };
                ShowColumnRow {
                    name: c.name,
                    r#type: type_name,
                    is_hidden: Some(col.is_hidden.to_string()),
                    description: c.description,
                }
            })
            .collect()
    }
}

#[derive(Fields)]
#[fields(style = "Title Case")]
struct ShowConnectionRow {
    name: String,
    r#type: String,
    properties: String,
}

#[derive(Fields)]
#[fields(style = "Title Case")]
struct ShowFunctionRow {
    name: String,
    arguments: String,
    return_type: String,
    language: String,
    link: Option<String>,
}

#[derive(Fields)]
#[fields(style = "Title Case")]
struct ShowIndexRow {
    name: String,
    on: String,
    key: String,
    include: String,
    distributed_by: String,
}

impl From<Arc<IndexCatalog>> for ShowIndexRow {
    fn from(index: Arc<IndexCatalog>) -> Self {
        let index_display = index.display();
        ShowIndexRow {
            name: index.name.clone(),
            on: index.primary_table.name.clone(),
            key: display_comma_separated(&index_display.index_columns_with_ordering).to_string(),
            include: display_comma_separated(&index_display.include_columns).to_string(),
            distributed_by: display_comma_separated(&index_display.distributed_by_columns)
                .to_string(),
        }
    }
}

#[derive(Fields)]
#[fields(style = "Title Case")]
struct ShowClusterRow {
    id: i32,
    addr: String,
    r#type: String,
    state: String,
    parallelism: i32,
    is_streaming: Option<bool>,
    is_serving: Option<bool>,
    is_unschedulable: Option<bool>,
    started_at: Option<Timestamptz>,
}

#[derive(Fields)]
#[fields(style = "Title Case")]
struct ShowJobRow {
    id: i64,
    statement: String,
    progress: String,
}

#[derive(Fields)]
#[fields(style = "Title Case")]
struct ShowProcessListRow {
    id: String,
    user: String,
    host: String,
    database: String,
    time: Option<String>,
    info: Option<String>,
}

#[derive(Fields)]
#[fields(style = "Title Case")]
struct ShowCreateObjectRow {
    name: String,
    create_sql: String,
}

/// Infer the row description for different show objects.
pub fn infer_show_object(objects: &ShowObject) -> Vec<PgFieldDescriptor> {
    fields_to_descriptors(match objects {
        ShowObject::Columns { .. } => ShowColumnRow::fields(),
        ShowObject::Connection { .. } => ShowConnectionRow::fields(),
        ShowObject::Function { .. } => ShowFunctionRow::fields(),
        ShowObject::Indexes { .. } => ShowIndexRow::fields(),
        ShowObject::Cluster => ShowClusterRow::fields(),
        ShowObject::Jobs => ShowJobRow::fields(),
        ShowObject::ProcessList => ShowProcessListRow::fields(),
        _ => ShowObjectRow::fields(),
    })
}

pub async fn handle_show_object(
    handler_args: HandlerArgs,
    command: ShowObject,
    filter: Option<ShowStatementFilter>,
) -> Result<RwPgResponse> {
    let session = handler_args.session;

    if let Some(ShowStatementFilter::Where(..)) = filter {
        bail_not_implemented!("WHERE clause in SHOW statement");
    }

    let catalog_reader = session.env().catalog_reader();

    let names = match command {
        // If not include schema name, use default schema name
        ShowObject::Table { schema } => {
            let search_path = session.config().search_path();
            let mut table_names_in_schema = vec![];
            for schema in schema_or_search_path(&session, &schema, &search_path) {
                // If the schema is not found, skip it
                if let Ok(schema_catalog) = catalog_reader
                    .read_guard()
                    .get_schema_by_name(session.database(), schema.as_ref())
                {
                    table_names_in_schema
                        .extend(schema_catalog.iter_table().map(|t| t.name.clone()));
                }
            }

            table_names_in_schema
        }
        ShowObject::InternalTable { schema } => catalog_reader
            .read_guard()
            .get_schema_by_name(session.database(), &schema_or_default(&schema))?
            .iter_internal_table()
            .map(|t| t.name.clone())
            .collect(),
        ShowObject::Database => catalog_reader.read_guard().get_all_database_names(),
        ShowObject::Schema => catalog_reader
            .read_guard()
            .get_all_schema_names(session.database())?,
        ShowObject::View { schema } => catalog_reader
            .read_guard()
            .get_schema_by_name(session.database(), &schema_or_default(&schema))?
            .iter_view()
            .map(|t| t.name.clone())
            .collect(),
        ShowObject::MaterializedView { schema } => catalog_reader
            .read_guard()
            .get_schema_by_name(session.database(), &schema_or_default(&schema))?
            .iter_created_mvs()
            .map(|t| t.name.clone())
            .collect(),
        ShowObject::Source { schema } => catalog_reader
            .read_guard()
            .get_schema_by_name(session.database(), &schema_or_default(&schema))?
            .iter_source()
            .map(|t| t.name.clone())
            .chain(session.temporary_source_manager().keys())
            .collect(),
        ShowObject::Sink { schema } => catalog_reader
            .read_guard()
            .get_schema_by_name(session.database(), &schema_or_default(&schema))?
            .iter_sink()
            .map(|t| t.name.clone())
            .collect(),
        ShowObject::Subscription { schema } => catalog_reader
            .read_guard()
            .get_schema_by_name(session.database(), &schema_or_default(&schema))?
            .iter_subscription()
            .map(|t| t.name.clone())
            .collect(),
        ShowObject::Secret { schema } => catalog_reader
            .read_guard()
            .get_schema_by_name(session.database(), &schema_or_default(&schema))?
            .iter_secret()
            .map(|t| t.name.clone())
            .collect(),
        ShowObject::Columns { table } => {
            let Ok(columns) = get_columns_from_table(&session, table.clone())
                .or(get_columns_from_sink(&session, table.clone()))
                .or(get_columns_from_view(&session, table.clone()))
            else {
                return Err(CatalogError::NotFound(
                    "table, source, sink or view",
                    table.to_string(),
                )
                .into());
            };

            return Ok(PgResponse::builder(StatementType::SHOW_COMMAND)
                .rows(columns.into_iter().flat_map(ShowColumnRow::from_catalog))
                .into());
        }
        ShowObject::Indexes { table } => {
            let indexes = get_indexes_from_table(&session, table)?;

            return Ok(PgResponse::builder(StatementType::SHOW_COMMAND)
                .rows(indexes.into_iter().map(ShowIndexRow::from))
                .into());
        }
        ShowObject::Connection { schema } => {
            let reader = catalog_reader.read_guard();
            let schema =
                reader.get_schema_by_name(session.database(), &schema_or_default(&schema))?;
            let rows = schema
                .iter_connections()
                .map(|c| {
                    let name = c.name.clone();
                    let r#type = match &c.info {
                        connection::Info::PrivateLinkService(_) => {
                            PRIVATELINK_CONNECTION.to_string()
                        },
                    };
                    let source_names = schema
                        .get_source_ids_by_connection(c.id)
                        .unwrap_or(Vec::new())
                        .into_iter()
                        .filter_map(|sid| schema.get_source_by_id(&sid).map(|catalog| catalog.name.as_str()))
                        .collect_vec();
                    let sink_names = schema
                        .get_sink_ids_by_connection(c.id)
                        .unwrap_or(Vec::new())
                        .into_iter()
                        .filter_map(|sid| schema.get_sink_by_id(&sid).map(|catalog| catalog.name.as_str()))
                        .collect_vec();
                    let properties = match &c.info {
                        connection::Info::PrivateLinkService(i) => {
                            format!(
                                "provider: {}\nservice_name: {}\nendpoint_id: {}\navailability_zones: {}\nsources: {}\nsinks: {}",
                                i.get_provider().unwrap().as_str_name(),
                                i.service_name,
                                i.endpoint_id,
                                serde_json::to_string(&i.dns_entries.keys().collect_vec()).unwrap(),
                                serde_json::to_string(&source_names).unwrap(),
                                serde_json::to_string(&sink_names).unwrap(),
                            )
                        }
                    };
                    ShowConnectionRow {
                        name,
                        r#type,
                        properties,
                    }
                });
            return Ok(PgResponse::builder(StatementType::SHOW_COMMAND)
                .rows(rows)
                .into());
        }
        ShowObject::Function { schema } => {
            let reader = catalog_reader.read_guard();
            let rows = reader
                .get_schema_by_name(session.database(), &schema_or_default(&schema))?
                .iter_function()
                .map(|t| ShowFunctionRow {
                    name: t.name.clone(),
                    arguments: t.arg_types.iter().map(|t| t.to_string()).join(", "),
                    return_type: t.return_type.to_string(),
                    language: t.language.clone(),
                    link: t.link.clone(),
                });
            return Ok(PgResponse::builder(StatementType::SHOW_COMMAND)
                .rows(rows)
                .into());
        }
        ShowObject::Cluster => {
            let workers = session.env().meta_client().list_all_nodes().await?;
            let rows = workers.into_iter().sorted_by_key(|w| w.id).map(|worker| {
                let addr: HostAddr = worker.host.as_ref().unwrap().into();
                let property = worker.property.as_ref();
                ShowClusterRow {
                    id: worker.id as _,
                    addr: addr.to_string(),
                    r#type: worker.get_type().unwrap().as_str_name().into(),
                    state: worker.get_state().unwrap().as_str_name().to_string(),
                    parallelism: worker.get_parallelism() as _,
                    is_streaming: property.map(|p| p.is_streaming),
                    is_serving: property.map(|p| p.is_serving),
                    is_unschedulable: property.map(|p| p.is_unschedulable),
                    started_at: worker
                        .started_at
                        .map(|ts| Timestamptz::from_secs(ts as i64).unwrap()),
                }
            });
            return Ok(PgResponse::builder(StatementType::SHOW_COMMAND)
                .rows(rows)
                .into());
        }
        ShowObject::Jobs => {
            let resp = session.env().meta_client().get_ddl_progress().await?;
            let rows = resp.into_iter().map(|job| ShowJobRow {
                id: job.id as i64,
                statement: job.statement,
                progress: job.progress,
            });
            return Ok(PgResponse::builder(StatementType::SHOW_COMMAND)
                .rows(rows)
                .into());
        }
        ShowObject::ProcessList => {
            let sessions_map = session.env().sessions_map().read();
            let rows = sessions_map.values().map(|s| {
                ShowProcessListRow {
                    // Since process id and the secret id in the session id are the same in RisingWave, just display the process id.
                    id: format!("{}", s.id().0),
                    user: s.user_name().to_owned(),
                    host: format!("{}", s.peer_addr()),
                    database: s.database().to_owned(),
                    time: s
                        .elapse_since_running_sql()
                        .map(|mills| format!("{}ms", mills)),
                    info: s
                        .running_sql()
                        .map(|sql| format!("{}", truncated_fmt::TruncatedFmt(&sql, 1024))),
                }
            });

            return Ok(PgResponse::builder(StatementType::SHOW_COMMAND)
                .rows(rows)
                .into());
        }
        ShowObject::Cursor => {
            let (rows, pg_descs) = session.get_cursor_manager().get_all_query_cursors().await;
            return Ok(PgResponse::builder(StatementType::SHOW_COMMAND)
                .row_cnt_opt(Some(rows.len() as i32))
                .values(PgResponseStream::from(rows), pg_descs)
                .into());
        }
        ShowObject::SubscriptionCursor => {
            let (rows, pg_descs) = session
                .get_cursor_manager()
                .get_all_subscription_cursors()
                .await;
            return Ok(PgResponse::builder(StatementType::SHOW_COMMAND)
                .row_cnt_opt(Some(rows.len() as i32))
                .values(PgResponseStream::from(rows), pg_descs)
                .into());
        }
    };

    let rows = names
        .into_iter()
        .filter(|arg| match &filter {
            Some(ShowStatementFilter::Like(pattern)) => like_default(arg, pattern),
            Some(ShowStatementFilter::ILike(pattern)) => i_like_default(arg, pattern),
            Some(ShowStatementFilter::Where(..)) => unreachable!(),
            None => true,
        })
        .map(|name| ShowObjectRow { name });

    Ok(PgResponse::builder(StatementType::SHOW_COMMAND)
        .rows(rows)
        .into())
}

pub fn infer_show_create_object() -> Vec<PgFieldDescriptor> {
    fields_to_descriptors(ShowCreateObjectRow::fields())
}

pub fn handle_show_create_object(
    handle_args: HandlerArgs,
    show_create_type: ShowCreateType,
    name: ObjectName,
) -> Result<RwPgResponse> {
    let session = handle_args.session;
    let catalog_reader = session.env().catalog_reader().read_guard();
    let (schema_name, object_name) =
        Binder::resolve_schema_qualified_name(session.database(), name.clone())?;
    let schema_name = schema_name.unwrap_or(DEFAULT_SCHEMA_NAME.to_string());
    let schema = catalog_reader.get_schema_by_name(session.database(), &schema_name)?;
    let sql = match show_create_type {
        ShowCreateType::MaterializedView => {
            let mv = schema
                .get_created_table_by_name(&object_name)
                .filter(|t| t.is_mview())
                .ok_or_else(|| CatalogError::NotFound("materialized view", name.to_string()))?;
            mv.create_sql()
        }
        ShowCreateType::View => {
            let view = schema
                .get_view_by_name(&object_name)
                .ok_or_else(|| CatalogError::NotFound("view", name.to_string()))?;
            view.create_sql()
        }
        ShowCreateType::Table => {
            let table = schema
                .get_created_table_by_name(&object_name)
                .filter(|t| t.is_table())
                .ok_or_else(|| CatalogError::NotFound("table", name.to_string()))?;
            table.create_sql()
        }
        ShowCreateType::Sink => {
            let sink = schema
                .get_sink_by_name(&object_name)
                .ok_or_else(|| CatalogError::NotFound("sink", name.to_string()))?;
            sink.create_sql()
        }
        ShowCreateType::Source => {
            let source = schema
                .get_source_by_name(&object_name)
                .filter(|s| s.associated_table_id.is_none())
                .ok_or_else(|| CatalogError::NotFound("source", name.to_string()))?;
            source.create_sql()
        }
        ShowCreateType::Index => {
            let index = schema
                .get_created_table_by_name(&object_name)
                .filter(|t| t.is_index())
                .ok_or_else(|| CatalogError::NotFound("index", name.to_string()))?;
            index.create_sql()
        }
        ShowCreateType::Function => {
            bail_not_implemented!("show create on: {}", show_create_type);
        }
        ShowCreateType::Subscription => {
            let subscription = schema
                .get_subscription_by_name(&object_name)
                .ok_or_else(|| CatalogError::NotFound("subscription", name.to_string()))?;
            subscription.create_sql()
        }
    };
    let name = format!("{}.{}", schema_name, object_name);

    Ok(PgResponse::builder(StatementType::SHOW_COMMAND)
        .rows([ShowCreateObjectRow {
            name,
            create_sql: sql,
        }])
        .into())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::ops::Index;

    use futures_async_stream::for_await;

    use crate::test_utils::{create_proto_file, LocalFrontend, PROTO_FILE_DATA};

    #[tokio::test]
    async fn test_show_source() {
        let frontend = LocalFrontend::new(Default::default()).await;

        let sql = r#"CREATE SOURCE t1 (column1 varchar)
        WITH (connector = 'kafka', kafka.topic = 'abc', kafka.brokers = 'localhost:1001')
        FORMAT PLAIN ENCODE JSON"#;
        frontend.run_sql(sql).await.unwrap();

        let mut rows = frontend.query_formatted_result("SHOW SOURCES").await;
        rows.sort();
        assert_eq!(rows, vec!["Row([Some(b\"t1\")])".to_string(),]);
    }

    #[tokio::test]
    async fn test_show_column() {
        let proto_file = create_proto_file(PROTO_FILE_DATA);
        let sql = format!(
            r#"CREATE SOURCE t
    WITH (connector = 'kafka', kafka.topic = 'abc', kafka.brokers = 'localhost:1001')
    FORMAT PLAIN ENCODE PROTOBUF (message = '.test.TestRecord', schema.location = 'file://{}')"#,
            proto_file.path().to_str().unwrap()
        );
        let frontend = LocalFrontend::new(Default::default()).await;
        frontend.run_sql(sql).await.unwrap();

        let sql = "show columns from t";
        let mut pg_response = frontend.run_sql(sql).await.unwrap();

        let mut columns = HashMap::new();
        #[for_await]
        for row_set in pg_response.values_stream() {
            let row_set = row_set.unwrap();
            for row in row_set {
                columns.insert(
                    std::str::from_utf8(row.index(0).as_ref().unwrap())
                        .unwrap()
                        .to_string(),
                    std::str::from_utf8(row.index(1).as_ref().unwrap())
                        .unwrap()
                        .to_string(),
                );
            }
        }

        let expected_columns: HashMap<String, String> = maplit::hashmap! {
            "id".into() => "integer".into(),
            "country.zipcode".into() => "character varying".into(),
            "zipcode".into() => "bigint".into(),
            "country.city.address".into() => "character varying".into(),
            "country.address".into() => "character varying".into(),
            "country.city".into() => "test.City".into(),
            "country.city.zipcode".into() => "character varying".into(),
            "rate".into() => "real".into(),
            "country".into() => "test.Country".into(),
            "_rw_kafka_timestamp".into() => "timestamp with time zone".into(),
            "_row_id".into() => "serial".into(),
        };

        assert_eq!(columns, expected_columns);
    }
}
