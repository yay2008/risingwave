use icelake::io_v2::track_writer::TrackWriter;
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
use risingwave_common::{array::arrow::IcebergArrowConvert, bitmap::Bitmap};

pub mod fs;
pub mod gcs;
pub mod opendal_sink;
pub mod s3;
use std::collections::HashMap;
use std::sync::atomic::AtomicI64;
use std::sync::Arc;

use arrow_schema_iceberg::SchemaRef;
use async_trait::async_trait;
use opendal::{Operator, Writer as OpendalWriter};
use parquet::arrow::AsyncArrowWriter;
use parquet::file::properties::WriterProperties;
use risingwave_common::array::{Op, StreamChunk};
use risingwave_common::catalog::Schema;

use crate::sink::catalog::SinkEncode;
use crate::sink::{Result, SinkError, SinkWriter};

pub struct OpenDalSinkWriter {
    schema: SchemaRef,
    operator: Operator,
    sink_writer: Option<FileWriterEnum>,
    is_append_only: bool,
    write_path: String,
    epoch: Option<u64>,
    executor_id: u64,
    encode_type: SinkEncode,
}

/// The `FileWriterEnum` enum represents different types of file writers used for various sink
/// implementations.
///
/// # Variants
///
/// - `ParquetFileWriter`: Represents a Parquet file writer using the `AsyncArrowWriter<W>`
/// for writing data to a Parquet file. It accepts an implementation of W: `AsyncWrite` + `Unpin` + `Send`
/// as the underlying writer. In this case, the `OpendalWriter` serves as the underlying writer.
///
/// - `FileWriter`: Represents a file writer for sinks other than Parquet. It uses the `OpendalWriter`
/// directly for writing data to the file.
///
/// The choice of writer used during the actual writing process depends on the encode type of the sink.
enum FileWriterEnum {
    ParquetFileWriter(AsyncArrowWriter<TrackWriter>),
}

#[async_trait]
impl SinkWriter for OpenDalSinkWriter {
    async fn write_batch(&mut self, chunk: StreamChunk) -> Result<()> {
        // Note: epoch is used to name the output files.
        // Todo: after enabling sink decouple, use the new naming convention.
        let epoch = self.epoch.ok_or_else(|| {
            SinkError::File("epoch has not been initialize, call `begin_epoch`".to_string())
        })?;
        if self.sink_writer.is_none() {
            self.create_sink_writer(epoch).await?;
        }
        if self.is_append_only {
            self.append_only(chunk).await
        } else {
            // currently file sink only supports append only mode.
            unimplemented!()
        }
    }

    async fn begin_epoch(&mut self, epoch: u64) -> Result<()> {
        self.epoch = Some(epoch);
        Ok(())
    }

    async fn abort(&mut self) -> Result<()> {
        Ok(())
    }

    /// For the file sink, currently, the sink decoupling feature is not enabled.
    /// When a checkpoint arrives, the force commit is performed to write the data to the file.
    /// In the future if flush and checkpoint is decoupled, we should enable sink decouple accordingly.
    async fn barrier(&mut self, is_checkpoint: bool) -> Result<()> {
        if is_checkpoint && let Some(sink_writer) = self.sink_writer.take() {
            match sink_writer {
                FileWriterEnum::ParquetFileWriter(w) => {
                    let _ = w.close().await?;
                }
            };
        }

        Ok(())
    }

    async fn update_vnode_bitmap(&mut self, _vnode_bitmap: Arc<Bitmap>) -> Result<()> {
        Ok(())
    }
}

impl OpenDalSinkWriter {
    pub fn new(
        operator: Operator,
        write_path: &str,
        rw_schema: Schema,
        is_append_only: bool,
        executor_id: u64,
        encode_type: SinkEncode,
    ) -> Result<Self> {
        let arrow_schema = convert_rw_schema_to_arrow_schema(rw_schema)?;
        Ok(Self {
            schema: Arc::new(arrow_schema),
            write_path: write_path.to_string(),
            operator,
            sink_writer: None,
            is_append_only,
            epoch: None,
            executor_id,
            encode_type,
        })
    }

    async fn create_object_writer(&mut self, epoch: u64) -> Result<OpendalWriter> {
        // Todo: specify more file suffixes based on encode_type.
        let suffix = match self.encode_type {
            SinkEncode::Parquet => "parquet",
            _ => unimplemented!(),
        };

        // Note: sink decoupling is not currently supported, which means that output files will not be batched across checkpoints.
        // The current implementation writes files every time a checkpoint arrives, so the naming convention is `epoch + executor_id + .suffix`.
        let object_name = format!(
            "{}/{}_{}.{}",
            self.write_path, epoch, self.executor_id, suffix,
        );
        Ok(self
            .operator
            .writer_with(&object_name)
            .concurrent(8)
            .await?)
    }

    async fn create_sink_writer(&mut self, epoch: u64) -> Result<()> {
        let object_writer = self.create_object_writer(epoch).await?;
        match self.encode_type {
            SinkEncode::Parquet => {
                let props = WriterProperties::builder();
                let written_size = Arc::new(AtomicI64::new(0));
                let track_writer = TrackWriter::new(
                    object_writer.into_futures_async_write(),
                    written_size.clone(),
                );
                self.sink_writer = Some(FileWriterEnum::ParquetFileWriter(
                    AsyncArrowWriter::try_new(
                        track_writer,
                        self.schema.clone(),
                        Some(props.build()),
                    )?,
                ));
            }
            // SinkEncode::Json => {
            //     self.sink_writer = Some(FileWriterEnum::FileWriter(object_writer));
            //     unimplemented!();
            // }
            _ => unimplemented!(),
        }

        Ok(())
    }

    async fn append_only(&mut self, chunk: StreamChunk) -> Result<()> {
        let (mut chunk, ops) = chunk.compact().into_parts();
        let filters =
            chunk.visibility() & ops.iter().map(|op| *op == Op::Insert).collect::<Bitmap>();
        chunk.set_visibility(filters);

        match self
            .sink_writer
            .as_mut()
            .ok_or_else(|| SinkError::File("Sink writer is not created.".to_string()))?
        {
            FileWriterEnum::ParquetFileWriter(w) => {
                let batch =
                    IcebergArrowConvert.to_record_batch(self.schema.clone(), &chunk.compact())?;
                w.write(&batch).await?;
            }
        }

        Ok(())
    }
}

fn convert_rw_schema_to_arrow_schema(
    rw_schema: risingwave_common::catalog::Schema,
) -> anyhow::Result<arrow_schema_iceberg::Schema> {
    let mut schema_fields = HashMap::new();
    rw_schema.fields.iter().for_each(|field| {
        let res = schema_fields.insert(&field.name, &field.data_type);
        // This assert is to make sure there is no duplicate field name in the schema.
        assert!(res.is_none())
    });
    let mut arrow_fields = vec![];
    for rw_field in &rw_schema.fields {
        let arrow_field = IcebergArrowConvert
            .to_arrow_field(&rw_field.name.clone(), &rw_field.data_type.clone())?;

        arrow_fields.push(arrow_field);
    }

    Ok(arrow_schema_iceberg::Schema::new(arrow_fields))
}
