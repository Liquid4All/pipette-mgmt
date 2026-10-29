//! Schema-agnostic parquet helpers and [`WriterOpts`], used by
//! [`crate::warehouse`] and [`crate::eval_sample_result`].

use std::path::Path;

use arrow::datatypes::SchemaRef;
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;

/// Per-store parquet write configuration.
#[derive(Debug, Clone, Copy)]
pub struct WriterOpts {
    pub zstd_level: ZstdLevel,
}

impl WriterOpts {
    fn properties(self) -> WriterProperties {
        WriterProperties::builder()
            .set_compression(Compression::ZSTD(self.zstd_level))
            .build()
    }
}

/// zstd's general-purpose level, used for every parquet file this server
/// writes. Not an `expect`: panicking constructs are denied crate-wide and
/// `try_new` is not `const`; the unreachable fallback only changes file size.
pub(crate) fn default_zstd_level() -> ZstdLevel {
    ZstdLevel::try_new(3).unwrap_or_default()
}

impl Default for WriterOpts {
    fn default() -> Self {
        Self {
            zstd_level: default_zstd_level(),
        }
    }
}

pub(crate) fn write_batch_bytes(
    opts: WriterOpts,
    schema: SchemaRef,
    batch: &RecordBatch,
) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut writer = ArrowWriter::try_new(&mut buf, schema, Some(opts.properties()))?;
    writer.write(batch)?;
    writer.close()?;
    Ok(buf)
}

pub(crate) fn write_batches_to_file(
    opts: WriterOpts,
    path: &Path,
    schema: SchemaRef,
    batches: &[RecordBatch],
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(path)?;
    let mut writer = ArrowWriter::try_new(file, schema, Some(opts.properties()))?;
    for batch in batches {
        writer.write(batch)?;
    }
    writer.close()?;
    Ok(())
}

pub(crate) fn read_batches_from_bytes(
    data: &[u8],
) -> anyhow::Result<impl Iterator<Item = anyhow::Result<RecordBatch>>> {
    let reader =
        ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::copy_from_slice(data))?.build()?;
    Ok(reader.map(|r| r.map_err(Into::into)))
}

pub(crate) fn read_batches_from_file(
    path: &Path,
) -> anyhow::Result<impl Iterator<Item = anyhow::Result<RecordBatch>>> {
    let file = std::fs::File::open(path)?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;
    Ok(reader.map(|r| r.map_err(Into::into)))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow::array::{Int32Array, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};
    use parquet::schema::types::ColumnPath;

    use crate::parquet_utils::*;

    #[test]
    fn default_uses_zstd_at_default_level() -> anyhow::Result<()> {
        let props = WriterOpts::default().properties();
        let codec = props.compression(&ColumnPath::from(vec!["any".to_string()]));
        let Compression::ZSTD(level) = codec else {
            anyhow::bail!("expected ZSTD, got {codec:?}");
        };
        assert_eq!(level.compression_level(), 3);
        Ok(())
    }

    fn sample_batch() -> anyhow::Result<(SchemaRef, RecordBatch)> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("name", DataType::Utf8, false),
            Field::new("count", DataType::Int32, false),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(StringArray::from(vec!["a", "b"])),
                Arc::new(Int32Array::from(vec![1, 2])),
            ],
        )?;
        Ok((schema, batch))
    }

    #[test]
    fn bytes_round_trip() -> anyhow::Result<()> {
        let (schema, batch) = sample_batch()?;
        let bytes = write_batch_bytes(WriterOpts::default(), schema, &batch)?;
        let read: Vec<RecordBatch> =
            read_batches_from_bytes(&bytes)?.collect::<anyhow::Result<_>>()?;
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].num_rows(), 2);
        Ok(())
    }

    #[test]
    fn file_round_trip_creates_parent_dirs() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("nested/sub/out.parquet");
        let (schema, batch) = sample_batch()?;
        write_batches_to_file(WriterOpts::default(), &path, schema, &[batch])?;
        let read: Vec<RecordBatch> =
            read_batches_from_file(&path)?.collect::<anyhow::Result<_>>()?;
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].num_rows(), 2);
        Ok(())
    }
}
