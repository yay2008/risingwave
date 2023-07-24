// Copyright 2023 RisingWave Labs
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

use futures::Stream;
use mysql_async::prelude::*;
use mysql_async::{ResultSetStream, TextProtocol};
use risingwave_common::array::StreamChunk;
use risingwave_common::buffer::Bitmap;
use risingwave_common::catalog::{Schema, TableId, TableOption};
use risingwave_common::row::Row;
use risingwave_common::util::row_serde::*;
use risingwave_common::util::value_encoding::EitherSerde;
use risingwave_connector::source::cdc::CdcProperties;
use risingwave_storage::row_serde::value_serde::ValueRowSerde;
use risingwave_storage::row_serde::ColumnMapping;
use risingwave_storage::table::TableIter;
use risingwave_storage::StateStore;

use crate::executor::backfill::upstream_table::binlog::UpstreamBinlogOffsetRead;
use crate::executor::backfill::upstream_table::snapshot::{SnapshotReadArgs, UpstreamSnapshotRead};
use crate::executor::backfill::upstream_table::{SchemaTableName, UpstreamDbType};
use crate::executor::StreamExecutorResult;

pub type ExternalStorageTable = ExternalTableInner<EitherSerde>;

#[derive(Clone)]
pub struct ExternalTableInner<SD: ValueRowSerde> {
    /// Id for this table.
    table_id: TableId,

    /// The normalized name of the table, e.g. `dbname.schema_name.table_name`.
    table_name: String,
    schema_name: String,

    table_reader: ExternalTableReaderImpl,

    db_type: UpstreamDbType,

    /// The schema of the output columns, i.e., this table VIEWED BY some executor like
    /// RowSeqScanExecutor.
    /// TODO(siyuan): the schema of the external table defined in the DDL
    schema: Schema,

    /// Used for serializing and deserializing the primary key.
    pk_serializer: OrderedRowSerde,

    output_indices: Vec<usize>,

    /// the key part of output_indices.
    key_output_indices: Option<Vec<usize>>,

    /// the value part of output_indices.
    value_output_indices: Vec<usize>,

    /// used for deserializing key part of output row from pk.
    output_row_in_key_indices: Vec<usize>,

    /// Mapping from column id to column index for deserializing the row.
    mapping: Arc<ColumnMapping>,

    /// Row deserializer to deserialize the whole value in storage to a row.
    row_serde: Arc<SD>,

    /// Indices of primary key.
    /// Note that the index is based on the all columns of the table, instead of the output ones.
    // FIXME: revisit constructions and usages.
    pk_indices: Vec<usize>,

    /// Indices of distribution key for computing vnode.
    /// Note that the index is based on the primary key columns by `pk_indices`.
    dist_key_in_pk_indices: Vec<usize>,

    /// Virtual nodes that the table is partitioned into.
    ///
    /// Only the rows whose vnode of the primary key is in this set will be visible to the
    /// executor. For READ_WRITE instances, the table will also check whether the written rows
    /// confirm to this partition.
    vnodes: Arc<Bitmap>,

    /// Used for catalog table_properties
    table_option: TableOption,

    read_prefix_len_hint: usize,
}

impl<SD: ValueRowSerde> ExternalTableInner<SD> {
    pub fn pk_serializer(&self) -> &OrderedRowSerde {
        &self.pk_serializer
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    pub fn pk_indices(&self) -> &[usize] {
        &self.pk_indices
    }

    pub fn output_indices(&self) -> &[usize] {
        &self.output_indices
    }

    /// Get the indices of the primary key columns in the output columns.
    ///
    /// Returns `None` if any of the primary key columns is not in the output columns.
    pub fn pk_in_output_indices(&self) -> Option<Vec<usize>> {
        self.pk_indices
            .iter()
            .map(|&i| self.output_indices.iter().position(|&j| i == j))
            .collect()
    }

    pub fn table_id(&self) -> TableId {
        self.table_id
    }

    pub fn db_type(&self) -> &UpstreamDbType {
        &self.db_type
    }

    pub fn schema_table_name(&self) -> SchemaTableName {
        SchemaTableName {
            schema_name: self.schema_name.clone(),
            table_name: self.table_name.clone(),
        }
    }

    pub fn table_reader(&self) -> &ExternalTableReaderImpl {
        &self.table_reader
    }
}

impl UpstreamBinlogOffsetRead for ExternalStorageTable {
    fn current_binlog_offset(&self) -> Option<String> {
        // todo(siyuan): issue different sql query to get the binlog offset
        match self {
            &_ => {}
        }

        todo!()
    }
}

// reader for external table used in backfill
pub trait ExternalTableReader {
    type SnapshotStream: Stream<Item = StreamExecutorResult<Option<StreamChunk>>> + Send;

    fn get_normalized_table_name(table_name: &SchemaTableName) -> String;

    // todo: Use GAT to return a future
    fn current_binlog_offset(&self) -> Option<String>;

    fn snapshot_read(&self, table_name: &SchemaTableName) -> Self::SnapshotStream;
}

pub enum ExternalTableReaderImpl {
    MYSQL(MySqlExternalTableReader),
    POSTGERS(PostgresExternalTableReader),
}

// todo(siyuan): embeded db client in the reader
pub struct MySqlExternalTableReader {
    pool: mysql_async::Pool,
    client: None,
}

impl MySqlExternalTableReader {
    pub async fn new(cdc_props: CdcProperties) -> Self {
        // todo: create a mysql client for upstream db

        let database_url = "mysql://root:123456@localhost:3306/mydb";
        let pool = mysql_async::Pool::new(database_url);

        Self { pool, client: None }
    }
}

impl ExternalTableReader for MySqlExternalTableReader {
    type SnapshotStream = impl Stream<Item = StreamExecutorResult<Option<StreamChunk>>> + Send;

    fn get_normalized_table_name(table_name: &SchemaTableName) -> String {
        format!("`{}`", table_name.table_name)
    }

    fn current_binlog_offset(&self) -> Option<String> {
        todo!()
    }

    fn snapshot_read(&self, table_name: &SchemaTableName) -> Self::SnapshotStream {
        async move {
            let sql = format!(
                "SELECT * FROM {}",
                Self::get_normalized_table_name(table_name)
            );

            // let mut conn = self.pool.get_conn().await?;
            //
            // let mut result_set = conn.query_iter(sql).await?;
            todo!("mysql snapshot read")
        }
    }
}

pub struct PostgresExternalTableReader {
    client: None,
}

impl ExternalTableReader for PostgresExternalTableReader {
    type SnapshotStream = impl Stream<Item = StreamExecutorResult<Option<StreamChunk>>> + Send;

    fn get_normalized_table_name(table_name: &SchemaTableName) -> String {
        format!("{}.{}", table_name.schema_name, table_name.table_name)
    }

    fn current_binlog_offset(&self) -> Option<String> {
        todo!()
    }

    fn snapshot_read(&self, table_name: &SchemaTableName) -> Self::SnapshotStream {
        todo!()
    }
}

impl ExternalTableReader for ExternalTableReaderImpl {
    type SnapshotStream = impl Stream<Item = StreamExecutorResult<Option<StreamChunk>>> + Send;

    fn get_normalized_table_name(table_name: &SchemaTableName) -> String {
        todo!()
    }

    fn current_binlog_offset(&self) -> Option<String> {
        todo!()
    }

    fn snapshot_read(&self, table_name: &SchemaTableName) -> Self::SnapshotStream {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use futures::pin_mut;
    use mysql_async::prelude::*;
    use mysql_async::Row;
    use tokio_stream::StreamExt;

    // unit test for external table reader
    #[tokio::test]
    async fn test_mysql_table_reader() {
        let pool = mysql_async::Pool::new("mysql://root:123456@localhost:8306/mydb");

        let mut conn = pool.get_conn().await.unwrap();
        let mut result_set = conn.query_iter("SELECT * FROM `t1`").await?;
        let s = result_set.stream::<Row>().await?.unwrap();
        pin_mut!(s);
        while let Some(row) = s.next().await {
            let row = row?;
            println!("got {:?}", row);
        }
    }
}
