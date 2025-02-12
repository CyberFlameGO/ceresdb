// Copyright 2022 CeresDB Project Authors. Licensed under Apache-2.0.

//! Schema of table

use std::{
    cmp::{self, Ordering},
    collections::{HashMap, HashSet},
    convert::TryFrom,
    fmt,
    str::FromStr,
    sync::Arc,
};

// Just re-use arrow's types
// TODO(yingwen): No need to support all schema that arrow supports, we can
// use a new type pattern to wrap Schema/SchemaRef and not allow to use
// the data type we not supported
pub use arrow_deps::arrow::datatypes::{
    DataType, Field, Schema as ArrowSchema, SchemaRef as ArrowSchemaRef,
};
use proto::common as common_pb;
use snafu::{ensure, Backtrace, OptionExt, ResultExt, Snafu};

use crate::{
    column_schema::{self, ColumnId, ColumnSchema},
    datum::DatumKind,
    row::{contiguous, RowView},
};

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display(
        "Projection too long, max:{}, given:{}.\nBacktrace:\n{}",
        max,
        given,
        backtrace
    ))]
    ProjectionTooLong {
        max: usize,
        given: usize,
        backtrace: Backtrace,
    },

    #[snafu(display(
        "Invalid projection index, max:{}, given:{}.\nBacktrace:\n{}",
        max,
        given,
        backtrace
    ))]
    InvalidProjectionIndex {
        max: usize,
        given: usize,
        backtrace: Backtrace,
    },

    #[snafu(display("Projection must have timestamp column.\nBacktrace:\n{}", backtrace))]
    ProjectionMissTimestamp { backtrace: Backtrace },

    #[snafu(display(
        "Column name already exists, name:{}.\nBacktrace:\n{}",
        name,
        backtrace
    ))]
    ColumnNameExists { name: String, backtrace: Backtrace },

    #[snafu(display(
        "Column id already exists, name:{}, id:{}.\nBacktrace:\n{}",
        name,
        id,
        backtrace
    ))]
    ColumnIdExists {
        name: String,
        id: ColumnId,
        backtrace: Backtrace,
    },

    #[snafu(display(
        "Unsupported key column type, name:{}, type:{:?}.\nBacktrace:\n{}",
        name,
        kind,
        backtrace
    ))]
    KeyColumnType {
        name: String,
        kind: DatumKind,
        backtrace: Backtrace,
    },

    #[snafu(display(
        "Timestamp key column already exists, timestamp_column:{}, given:{}.\nBacktrace:\n{}",
        timestamp_column,
        given_column,
        backtrace
    ))]
    TimestampKeyExists {
        timestamp_column: String,
        given_column: String,
        backtrace: Backtrace,
    },

    #[snafu(display("Timestamp key not exists.\nBacktrace:\n{}", backtrace))]
    MissingTimestampKey { backtrace: Backtrace },

    #[snafu(display(
        "Key column cannot be nullable, name:{}.\nBacktrace:\n{}",
        name,
        backtrace
    ))]
    NullKeyColumn { name: String, backtrace: Backtrace },

    #[snafu(display(
        "Invalid arrow field, field_name:{}, arrow_schema:{:?}, err:{}",
        field_name,
        arrow_schema,
        source
    ))]
    InvalidArrowField {
        field_name: String,
        arrow_schema: ArrowSchemaRef,
        source: crate::column_schema::Error,
    },

    #[snafu(display(
        "Invalid schema to generate tsid primary key.\nBacktrace:\n{}",
        backtrace
    ))]
    InvalidTsidSchema { backtrace: Backtrace },

    #[snafu(display(
        "Invalid arrow schema key, key:{:?}, raw_value:{}, err:{:?}.\nBacktrace:\n{}",
        key,
        raw_value,
        source,
        backtrace
    ))]
    InvalidArrowSchemaMetaValue {
        key: ArrowSchemaMetaKey,
        raw_value: String,
        source: Box<dyn std::error::Error + Send + Sync>,
        backtrace: Backtrace,
    },

    #[snafu(display(
        "Arrow schema meta key not found, key:{:?}.\nBacktrace:\n{}",
        key,
        backtrace
    ))]
    ArrowSchemaMetaKeyNotFound {
        key: ArrowSchemaMetaKey,
        backtrace: Backtrace,
    },
}

// TODO(boyan)  make these constants configurable
pub const TSID_COLUMN: &str = "tsid";
pub const TIMESTAMP_COLUMN: &str = "timestamp";

pub type Result<T> = std::result::Result<T, Error>;

const DEFAULT_SCHEMA_VERSION: Version = 1;

#[derive(Debug, Snafu)]
pub enum CompatError {
    #[snafu(display("Incompatible column schema for write, err:{}", source))]
    IncompatWriteColumn {
        source: crate::column_schema::CompatError,
    },

    #[snafu(display("Missing column, name:{}", name))]
    MissingWriteColumn { name: String },

    #[snafu(display("Columns to write not found in table, names:{:?}", names))]
    WriteMoreColumn { names: Vec<String> },
}

/// Meta data of the arrow schema
struct ArrowSchemaMeta {
    num_key_columns: usize,
    timestamp_index: usize,
    enable_tsid_primary_key: bool,
    version: u32,
}

#[derive(Copy, Clone, Debug)]
pub enum ArrowSchemaMetaKey {
    NumKeyColumns,
    TimestampIndex,
    EnableTsidPrimaryKey,
    Version,
}

impl ArrowSchemaMetaKey {
    fn as_str(&self) -> &str {
        match self {
            ArrowSchemaMetaKey::NumKeyColumns => "schema:num_key_columns",
            ArrowSchemaMetaKey::TimestampIndex => "schema::timestamp_index",
            ArrowSchemaMetaKey::EnableTsidPrimaryKey => "schema::enable_tsid_primary_key",
            ArrowSchemaMetaKey::Version => "schema::version",
        }
    }
}

impl ToString for ArrowSchemaMetaKey {
    fn to_string(&self) -> String {
        self.as_str().to_string()
    }
}

/// Schema version
pub type Version = u32;

/// Mapping column index in table schema to column index in writer schema
#[derive(Default)]
pub struct IndexInWriterSchema(Vec<Option<usize>>);

impl IndexInWriterSchema {
    /// Create a index mapping for same schema with `num_columns` columns.
    pub fn for_same_schema(num_columns: usize) -> Self {
        let indexes = (0..num_columns).into_iter().map(Some).collect();
        Self(indexes)
    }

    /// Returns the column index in writer schema of the column with index
    /// `index_in_table` in the table schema where the writer prepared to
    /// write to.
    ///
    /// If the column is not in writer schema, returns None, which means that
    /// this column should be filled by null.
    ///
    /// Panic if the index_in_table is out of bound
    pub fn column_index_in_writer(&self, index_in_table: usize) -> Option<usize> {
        self.0[index_in_table]
    }
}

// TODO(yingwen): No need to compare all elements in ColumnSchemas, Schema,
// RecordSchema, custom PartialEq for them.

/// Data of column schemas
#[derive(PartialEq)]
pub(crate) struct ColumnSchemas {
    /// Column schemas
    columns: Vec<ColumnSchema>,
    /// Column name to index of that column schema in `columns`, the index is
    /// guaranteed to be valid
    name_to_index: HashMap<String, usize>,
    /// Byte offsets of each column in contiguous row.
    byte_offsets: Vec<usize>,
    /// String buffer offset in contiguous row.
    string_buffer_offset: usize,
}

impl ColumnSchemas {
    fn new(columns: Vec<ColumnSchema>) -> Self {
        let name_to_index = columns
            .iter()
            .enumerate()
            .map(|(idx, c)| (c.name.to_string(), idx))
            .collect();

        let mut current_offset = 0;
        let mut byte_offsets = Vec::with_capacity(columns.len());
        for column_schema in &columns {
            byte_offsets.push(current_offset);
            current_offset += contiguous::byte_size_of_datum(&column_schema.data_type);
        }

        Self {
            columns,
            name_to_index,
            byte_offsets,
            string_buffer_offset: current_offset,
        }
    }
}

impl ColumnSchemas {
    pub fn num_columns(&self) -> usize {
        self.columns().len()
    }

    pub fn columns(&self) -> &[ColumnSchema] {
        &self.columns
    }

    pub fn column(&self, i: usize) -> &ColumnSchema {
        &self.columns[i]
    }

    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.name_to_index.get(name).copied()
    }
}

impl fmt::Debug for ColumnSchemas {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ColumnSchemas")
            // name_to_index is ignored.
            .field("columns", &self.columns)
            .finish()
    }
}

/// Schema of [crate::record_batch::RecordBatch]
///
/// Should be cheap to clone.
///
/// Note: Only `name`, `data_type`, `is_nullable` is valid after converting from
/// arrow's schema, the additional fields like `id`/`is_tag`/`comment` is always
/// unset. Now we only convert arrow's schema into our record before we output
/// the final query result, where the additional fields is never used.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordSchema {
    arrow_schema: ArrowSchemaRef,
    column_schemas: Arc<ColumnSchemas>,
}

impl RecordSchema {
    fn from_column_schemas(column_schemas: ColumnSchemas) -> Self {
        // Convert to arrow fields.
        let fields = column_schemas
            .columns
            .iter()
            .map(|col| col.to_arrow_field())
            .collect();
        // Build arrow schema.
        let arrow_schema = Arc::new(ArrowSchema::new(fields));

        Self {
            arrow_schema,
            column_schemas: Arc::new(column_schemas),
        }
    }

    pub fn num_columns(&self) -> usize {
        self.column_schemas.num_columns()
    }

    pub fn columns(&self) -> &[ColumnSchema] {
        self.column_schemas.columns()
    }

    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.column_schemas.index_of(name)
    }

    pub fn column(&self, i: usize) -> &ColumnSchema {
        self.column_schemas.column(i)
    }

    pub fn to_arrow_schema_ref(&self) -> ArrowSchemaRef {
        self.arrow_schema.clone()
    }
}

impl TryFrom<ArrowSchemaRef> for RecordSchema {
    type Error = Error;

    fn try_from(arrow_schema: ArrowSchemaRef) -> Result<Self> {
        let fields = arrow_schema.fields();
        let mut columns = Vec::with_capacity(fields.len());

        for field in fields {
            let column_schema =
                ColumnSchema::try_from(field).with_context(|| InvalidArrowField {
                    arrow_schema: arrow_schema.clone(),
                    field_name: field.name(),
                })?;
            columns.push(column_schema);
        }

        let column_schemas = ColumnSchemas::new(columns);

        Ok(Self::from_column_schemas(column_schemas))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecordSchemaWithKey {
    record_schema: RecordSchema,
    num_key_columns: usize,
}

impl RecordSchemaWithKey {
    pub fn num_columns(&self) -> usize {
        self.record_schema.num_columns()
    }

    pub fn compare_row<LR: RowView, RR: RowView>(&self, lhs: &LR, rhs: &RR) -> Ordering {
        compare_row(self.num_key_columns, lhs, rhs)
    }

    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.record_schema.index_of(name)
    }

    pub fn columns(&self) -> &[ColumnSchema] {
        self.record_schema.columns()
    }

    /// Returns an immutable reference of the key column vector.
    pub fn key_columns(&self) -> &[ColumnSchema] {
        &self.columns()[..self.num_key_columns]
    }

    pub(crate) fn into_record_schema(self) -> RecordSchema {
        self.record_schema
    }

    pub(crate) fn to_arrow_schema_ref(&self) -> ArrowSchemaRef {
        self.record_schema.to_arrow_schema_ref()
    }

    #[inline]
    pub fn num_key_columns(&self) -> usize {
        self.num_key_columns
    }
}

/// Compare the two rows.
///
/// REQUIRES: the two rows must have the same number of key columns as
/// `num_key_columns`.
pub fn compare_row<LR: RowView, RR: RowView>(
    num_key_columns: usize,
    lhs: &LR,
    rhs: &RR,
) -> Ordering {
    for column_idx in 0..num_key_columns {
        // caller should ensure the row view is valid.
        // TODO(xikai): unwrap may not a good way to handle the error.
        let left_datum = lhs.column_by_idx(column_idx);
        let right_datum = rhs.column_by_idx(column_idx);
        // the two datums must be of the same kind type.
        match left_datum.partial_cmp(&right_datum).unwrap() {
            Ordering::Equal => continue,
            v @ Ordering::Less | v @ Ordering::Greater => return v,
        }
    }

    Ordering::Equal
}

// TODO(yingwen): Maybe rename to TableSchema.
/// Schema of a table
///
/// - Should be immutable
/// - Each schema must have a timestamp column
/// - Should be immutable and cheap to clone, though passing by reference is
///   preferred
/// - The prefix of columns makes up the primary key (similar to kudu's schema)
/// - The Schema should built by builder
#[derive(Clone, PartialEq)]
pub struct Schema {
    /// The underlying arrow schema, data type of fields must be supported by
    /// datum
    arrow_schema: ArrowSchemaRef,
    /// The number of primary key columns
    num_key_columns: usize,
    /// Index of timestamp key column
    // TODO(yingwen): Maybe we can remove the restriction that timestamp column must exists in
    //  schema (mainly for projected schema)
    timestamp_index: usize,
    /// Index of tsid key column and None denotes the `enable_tsid_primary_key`
    /// is not set.
    tsid_index: Option<usize>,
    /// Control whether to generate tsid as primary key
    enable_tsid_primary_key: bool,
    /// Column schemas, only holds arc pointer so the Schema can be cloned
    /// without much overhead.
    column_schemas: Arc<ColumnSchemas>,
    /// Version of the schema, schemas with same version should be identical.
    version: Version,
}

impl fmt::Debug for Schema {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Schema")
            // arrow_schema is ignored.
            .field("num_key_columns", &self.num_key_columns)
            .field("timestamp_index", &self.timestamp_index)
            .field("tsid_index", &self.tsid_index)
            .field("enable_tsid_primary_key", &self.enable_tsid_primary_key)
            .field("column_schemas", &self.column_schemas)
            .field("version", &self.version)
            .finish()
    }
}

impl TryFrom<ArrowSchemaRef> for Schema {
    type Error = Error;

    fn try_from(arrow_schema: ArrowSchemaRef) -> Result<Self> {
        Builder::build_from_arrow_schema(arrow_schema)
    }
}

impl TryFrom<RecordSchema> for Schema {
    type Error = Error;

    fn try_from(record_schema: RecordSchema) -> Result<Self> {
        Builder::build_from_arrow_schema(record_schema.to_arrow_schema_ref())
    }
}

impl Schema {
    /// Returns an immutable reference of the vector of [ColumnSchema].
    pub fn columns(&self) -> &[ColumnSchema] {
        self.column_schemas.columns()
    }

    /// Returns an immutable reference of the key column vector.
    pub fn key_columns(&self) -> &[ColumnSchema] {
        &self.columns()[..self.num_key_columns]
    }

    /// Returns an immutable reference of the normal column vector.
    pub fn normal_columns(&self) -> &[ColumnSchema] {
        &self.columns()[self.num_key_columns..]
    }

    /// Returns index of the tsid column.
    pub fn index_of_tsid(&self) -> Option<usize> {
        self.tsid_index
    }

    /// Returns tsid column index and immutable reference of tsid column
    pub fn tsid_column(&self) -> Option<&ColumnSchema> {
        if let Some(idx) = self.index_of_tsid() {
            Some(&self.column_schemas.columns[idx])
        } else {
            None
        }
    }

    /// Returns total number of columns
    pub fn num_columns(&self) -> usize {
        self.column_schemas.num_columns()
    }

    /// Returns an immutable reference of a specific [ColumnSchema] selected by
    /// name.
    pub fn column_with_name(&self, name: &str) -> Option<&ColumnSchema> {
        let index = self.column_schemas.name_to_index.get(name)?;
        Some(&self.column_schemas.columns[*index])
    }

    /// Returns an immutable reference of a specific [ColumnSchema] selected
    /// using an offset within the internal vector.
    ///
    /// Panic if i is out of bound
    pub fn column(&self, i: usize) -> &ColumnSchema {
        self.column_schemas.column(i)
    }

    /// Return the ref to [arrow_deps::arrow::datatypes::SchemaRef]
    pub fn as_arrow_schema_ref(&self) -> &ArrowSchemaRef {
        &self.arrow_schema
    }

    /// Return the cloned [arrow_deps::arrow::datatypes::SchemaRef]
    pub fn to_arrow_schema_ref(&self) -> ArrowSchemaRef {
        self.arrow_schema.clone()
    }

    /// Into [arrow_deps::arrow::datatypes::SchemaRef]
    pub fn into_arrow_schema_ref(self) -> ArrowSchemaRef {
        self.arrow_schema
    }

    /// Find the index of the column with the given name.
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.column_schemas.index_of(name)
    }

    /// Returns the number of columns in primary key
    #[inline]
    pub fn num_key_columns(&self) -> usize {
        self.num_key_columns
    }

    /// Get the name of the timestamp column
    #[inline]
    pub fn timestamp_name(&self) -> &str {
        &self.column(self.timestamp_index()).name
    }

    /// Get the index of the timestamp column
    #[inline]
    pub fn timestamp_index(&self) -> usize {
        self.timestamp_index
    }

    /// Get the version of this schema
    #[inline]
    pub fn version(&self) -> Version {
        self.version
    }

    /// Compare the two rows.
    ///
    /// REQUIRES: the two rows must have the key columns defined by the schema.
    pub fn compare_row<R: RowView>(&self, lhs: &R, rhs: &R) -> Ordering {
        compare_row(self.num_key_columns, lhs, rhs)
    }

    /// Returns `Ok` if rows with `writer_schema` can write to table with the
    /// same schema as `self`.
    pub fn compatible_for_write(
        &self,
        writer_schema: &Schema,
        index_in_writer: &mut IndexInWriterSchema,
    ) -> std::result::Result<(), CompatError> {
        index_in_writer.0.reserve(self.num_columns());

        let mut num_col_in_writer = 0;
        for column in self.columns() {
            // Find column in schema of writer.
            match writer_schema.index_of(&column.name) {
                Some(writer_index) => {
                    let writer_column = writer_schema.column(writer_index);

                    // Column is found in writer
                    num_col_in_writer += 1;

                    // Column with same name, but not compatible
                    column
                        .compatible_for_write(writer_column)
                        .context(IncompatWriteColumn)?;

                    // Column is compatible, push index mapping
                    index_in_writer.0.push(Some(writer_index));
                }
                None => {
                    // Column is not found in writer, then the column should be nullable.
                    ensure!(
                        column.is_nullable,
                        MissingWriteColumn { name: &column.name }
                    );

                    // Column is nullable, push index mapping
                    index_in_writer.0.push(None);
                }
            }
        }
        // All columns of this schema have been checked

        // If the writer have columns not in this schema, then we consider it
        // incompatible
        ensure!(
            num_col_in_writer == writer_schema.num_columns(),
            WriteMoreColumn {
                names: writer_schema
                    .columns()
                    .iter()
                    .filter_map(|c| if self.column_with_name(&c.name).is_none() {
                        Some(c.name.clone())
                    } else {
                        None
                    })
                    .collect::<Vec<_>>(),
            }
        );

        Ok(())
    }

    pub fn to_record_schema(&self) -> RecordSchema {
        RecordSchema {
            arrow_schema: self.arrow_schema.clone(),
            column_schemas: self.column_schemas.clone(),
        }
    }

    pub fn to_record_schema_with_key(&self) -> RecordSchemaWithKey {
        RecordSchemaWithKey {
            record_schema: self.to_record_schema(),
            num_key_columns: self.num_key_columns,
        }
    }

    /// Panic if projection is invalid.
    pub(crate) fn project_record_schema_with_key(
        &self,
        projection: &[usize],
    ) -> RecordSchemaWithKey {
        let mut columns = Vec::with_capacity(self.num_key_columns);
        // Keep all key columns in order.
        for key_column in self.key_columns() {
            columns.push(key_column.clone());
        }

        // Collect normal columns needed by the projection.
        for p in projection {
            if *p >= self.num_key_columns {
                // A normal column
                let normal_column = &self.columns()[*p];
                columns.push(normal_column.clone());
            }
        }

        let record_schema = RecordSchema::from_column_schemas(ColumnSchemas::new(columns));

        RecordSchemaWithKey {
            record_schema,
            num_key_columns: self.num_key_columns,
        }
    }

    /// Panic if projection is invalid.
    pub(crate) fn project_record_schema(&self, projection: &[usize]) -> RecordSchema {
        let mut columns = Vec::with_capacity(projection.len());

        // Collect all columns needed by the projection.
        for p in projection {
            let column_schema = &self.columns()[*p];
            // Insert the index in projected schema of the column
            columns.push(column_schema.clone());
        }

        RecordSchema::from_column_schemas(ColumnSchemas::new(columns))
    }

    /// Returns byte offsets in contiguous row.
    #[inline]
    pub fn byte_offsets(&self) -> &[usize] {
        &self.column_schemas.byte_offsets
    }

    /// Returns byte offset in contiguous row of given column.
    ///
    /// Panic if out of bound.
    #[inline]
    pub fn byte_offset(&self, index: usize) -> usize {
        self.column_schemas.byte_offsets[index]
    }

    /// Returns string buffer offset in contiguous row.
    #[inline]
    pub fn string_buffer_offset(&self) -> usize {
        self.column_schemas.string_buffer_offset
    }
}

impl TryFrom<common_pb::TableSchema> for Schema {
    type Error = Error;

    fn try_from(schema: common_pb::TableSchema) -> Result<Self> {
        let mut builder = Builder::with_capacity(schema.columns.len())
            .version(schema.version)
            .enable_tsid_primary_key(schema.enable_tsid_primary_key);

        for (i, column_schema_pb) in schema.columns.into_iter().enumerate() {
            let column = ColumnSchema::from(column_schema_pb);

            if i < schema.num_key_columns as usize {
                builder = builder.add_key_column(column)?;
            } else {
                builder = builder.add_normal_column(column)?;
            }
        }

        builder.build()
    }
}

impl From<Schema> for common_pb::TableSchema {
    fn from(schema: Schema) -> Self {
        let mut table_schema = common_pb::TableSchema::new();

        for column in schema.columns() {
            // Convert schema of each column
            let column_schema = column.to_pb();
            table_schema.columns.push(column_schema);
        }

        table_schema.num_key_columns = schema.num_key_columns as u32;
        table_schema.timestamp_index = schema.timestamp_index as u32;
        table_schema.enable_tsid_primary_key = schema.enable_tsid_primary_key;
        table_schema.version = schema.version;

        table_schema
    }
}

/// Schema builder
#[must_use]
pub struct Builder {
    columns: Vec<ColumnSchema>,
    /// The number of primary key columns
    num_key_columns: usize,
    /// Timestamp column index
    timestamp_index: Option<usize>,
    column_names: HashSet<String>,
    column_ids: HashSet<ColumnId>,
    /// Version of the schema
    version: Version,
    /// Auto increment the column id if the id of the input ColumnSchema is
    /// [crate::column_schema::COLUMN_ID_UNINIT].
    auto_increment_column_id: bool,
    max_column_id: ColumnId,
    enable_tsid_primary_key: bool,
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

impl Builder {
    /// Create a new builder
    pub fn new() -> Self {
        Self::with_capacity(0)
    }

    /// Create a builder with given capacity
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            columns: Vec::with_capacity(capacity),
            num_key_columns: 0,
            timestamp_index: None,
            column_names: HashSet::with_capacity(capacity),
            column_ids: HashSet::with_capacity(capacity),
            version: DEFAULT_SCHEMA_VERSION,
            auto_increment_column_id: false,
            max_column_id: column_schema::COLUMN_ID_UNINIT,
            enable_tsid_primary_key: false,
        }
    }

    /// Add a key column
    pub fn add_key_column(mut self, mut column: ColumnSchema) -> Result<Self> {
        self.may_alloc_column_id(&mut column);
        self.validate_column(&column, true)?;

        ensure!(!column.is_nullable, NullKeyColumn { name: column.name });

        // FIXME(xikai): it seems not reasonable to decide the timestamp column in this
        // way.
        let is_timestamp = DatumKind::Timestamp == column.data_type;
        if is_timestamp {
            ensure!(
                self.timestamp_index.is_none(),
                TimestampKeyExists {
                    timestamp_column: &self.columns[self.timestamp_index.unwrap()].name,
                    given_column: column.name,
                }
            );
            self.timestamp_index = Some(self.num_key_columns);
        }

        self.insert_new_key_column(column);

        Ok(self)
    }

    /// Add a normal (non key) column
    pub fn add_normal_column(mut self, mut column: ColumnSchema) -> Result<Self> {
        self.may_alloc_column_id(&mut column);
        self.validate_column(&column, false)?;

        self.insert_new_normal_column(column);

        Ok(self)
    }

    /// Set version of the schema
    pub fn version(mut self, version: Version) -> Self {
        self.version = version;
        self
    }

    /// When auto increment is true, assign the column schema an auto
    /// incremented id if its id is [crate::column_schema::COLUMN_ID_UNINIT].
    ///
    /// Default is false
    pub fn auto_increment_column_id(mut self, auto_increment: bool) -> Self {
        self.auto_increment_column_id = auto_increment;
        self
    }

    /// Enable tsid as primary key.
    pub fn enable_tsid_primary_key(mut self, enable_tsid_primary_key: bool) -> Self {
        self.enable_tsid_primary_key = enable_tsid_primary_key;
        self
    }

    fn may_alloc_column_id(&mut self, column: &mut ColumnSchema) {
        // Assign this column an id
        if self.auto_increment_column_id && column.id == column_schema::COLUMN_ID_UNINIT {
            column.id = self.max_column_id + 1;
        }

        self.max_column_id = cmp::max(self.max_column_id, column.id);
    }

    // TODO(yingwen): Do we need to support null data type?
    fn validate_column(&self, column: &ColumnSchema, is_key: bool) -> Result<()> {
        ensure!(
            !self.column_names.contains(&column.name),
            ColumnNameExists { name: &column.name }
        );

        // Check datum kind if this is a key column
        if is_key {
            ensure!(
                column.data_type.is_key_kind(),
                KeyColumnType {
                    name: &column.name,
                    kind: column.data_type,
                }
            );
        }

        ensure!(
            !self.column_ids.contains(&column.id),
            ColumnIdExists {
                name: &column.name,
                id: column.id,
            }
        );

        Ok(())
    }

    fn insert_new_key_column(&mut self, column: ColumnSchema) {
        self.column_names.insert(column.name.clone());
        self.column_ids.insert(column.id);

        self.columns.insert(self.num_key_columns, column);
        self.num_key_columns += 1;
    }

    fn insert_new_normal_column(&mut self, column: ColumnSchema) {
        self.column_names.insert(column.name.clone());
        self.column_ids.insert(column.id);

        self.columns.push(column);
    }

    fn build_from_arrow_schema(arrow_schema: ArrowSchemaRef) -> Result<Schema> {
        let fields = arrow_schema.fields();
        let mut columns = Vec::with_capacity(fields.len());

        for field in fields {
            let column_schema =
                ColumnSchema::try_from(field).with_context(|| InvalidArrowField {
                    arrow_schema: arrow_schema.clone(),
                    field_name: field.name(),
                })?;
            columns.push(column_schema);
        }

        // FIXME(xikai): Now we have to tolerate the decoding failure because of the bug
        // of  datafusion (fixed by: https://github.com/apache/arrow-datafusion/commit/1448d9752ab3a38f02732274f91136a6a6ad3db4).
        //  (The bug may cause the meta data of the schema meta lost duration plan
        // execution.)
        let ArrowSchemaMeta {
            num_key_columns,
            timestamp_index,
            enable_tsid_primary_key,
            version,
        } = Self::parse_arrow_schema_meta_or_default(arrow_schema.metadata())?;
        let tsid_index = Self::find_tsid_index(enable_tsid_primary_key, &columns)?;

        let column_schemas = Arc::new(ColumnSchemas::new(columns));

        Ok(Schema {
            arrow_schema,
            num_key_columns,
            timestamp_index,
            tsid_index,
            enable_tsid_primary_key,
            column_schemas,
            version,
        })
    }

    fn parse_arrow_schema_meta_value<T>(
        meta: &HashMap<String, String>,
        key: ArrowSchemaMetaKey,
    ) -> Result<T>
    where
        T: FromStr,
        T::Err: std::error::Error + Send + Sync + 'static,
    {
        let raw_value = meta
            .get(key.as_str())
            .context(ArrowSchemaMetaKeyNotFound { key })?;
        T::from_str(raw_value.as_str())
            .map_err(|e| Box::new(e) as _)
            .context(InvalidArrowSchemaMetaValue { key, raw_value })
    }

    /// Parse the necessary meta information from the arrow schema's meta data.
    fn parse_arrow_schema_meta_or_default(
        meta: &HashMap<String, String>,
    ) -> Result<ArrowSchemaMeta> {
        match Self::parse_arrow_schema_meta(meta) {
            Ok(v) => Ok(v),
            Err(Error::ArrowSchemaMetaKeyNotFound { .. }) => Ok(ArrowSchemaMeta {
                num_key_columns: 0,
                timestamp_index: 0,
                enable_tsid_primary_key: false,
                version: 0,
            }),
            Err(e) => Err(e),
        }
    }

    /// Parse the necessary meta information from the arrow schema's meta data.
    fn parse_arrow_schema_meta(meta: &HashMap<String, String>) -> Result<ArrowSchemaMeta> {
        Ok(ArrowSchemaMeta {
            num_key_columns: Self::parse_arrow_schema_meta_value(
                meta,
                ArrowSchemaMetaKey::NumKeyColumns,
            )?,
            timestamp_index: Self::parse_arrow_schema_meta_value(
                meta,
                ArrowSchemaMetaKey::TimestampIndex,
            )?,
            enable_tsid_primary_key: Self::parse_arrow_schema_meta_value(
                meta,
                ArrowSchemaMetaKey::EnableTsidPrimaryKey,
            )?,
            version: Self::parse_arrow_schema_meta_value(meta, ArrowSchemaMetaKey::Version)?,
        })
    }

    /// Build arrow schema meta data.
    ///
    /// Requires: the timestamp index is not None.
    fn build_arrow_schema_meta(&self) -> HashMap<String, String> {
        let mut meta = HashMap::with_capacity(4);
        meta.insert(
            ArrowSchemaMetaKey::NumKeyColumns.to_string(),
            self.num_key_columns.to_string(),
        );
        meta.insert(
            ArrowSchemaMetaKey::TimestampIndex.to_string(),
            self.timestamp_index.unwrap().to_string(),
        );
        meta.insert(
            ArrowSchemaMetaKey::Version.to_string(),
            self.version.to_string(),
        );
        meta.insert(
            ArrowSchemaMetaKey::EnableTsidPrimaryKey.to_string(),
            self.enable_tsid_primary_key.to_string(),
        );

        meta
    }

    fn find_tsid_index(
        enable_tsid_primary_key: bool,
        columns: &[ColumnSchema],
    ) -> Result<Option<usize>> {
        if !enable_tsid_primary_key {
            return Ok(None);
        }

        let idx = columns
            .iter()
            .enumerate()
            .find_map(|(idx, col_schema)| {
                if col_schema.name == TSID_COLUMN {
                    Some(idx)
                } else {
                    None
                }
            })
            .context(InvalidTsidSchema)?;

        Ok(Some(idx))
    }

    /// Build the schema
    pub fn build(self) -> Result<Schema> {
        let timestamp_index = self.timestamp_index.context(MissingTimestampKey)?;
        // Timestamp key column is exists, so key columns should not be zero
        assert!(self.num_key_columns > 0);
        if self.enable_tsid_primary_key {
            ensure!(self.num_key_columns == 2, InvalidTsidSchema);
        }

        let tsid_index = Self::find_tsid_index(self.enable_tsid_primary_key, &self.columns)?;

        let fields = self.columns.iter().map(|c| c.to_arrow_field()).collect();
        let meta = self.build_arrow_schema_meta();

        Ok(Schema {
            arrow_schema: Arc::new(ArrowSchema::new_with_metadata(fields, meta)),
            num_key_columns: self.num_key_columns,
            timestamp_index,
            tsid_index,
            enable_tsid_primary_key: self.enable_tsid_primary_key,
            column_schemas: Arc::new(ColumnSchemas::new(self.columns)),
            version: self.version,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bytes::Bytes,
        datum::Datum,
        row::{Row, RowWithMeta},
        time::Timestamp,
    };

    #[test]
    fn test_schema() {
        let schema = Builder::new()
            .auto_increment_column_id(true)
            .add_key_column(
                column_schema::Builder::new("key1".to_string(), DatumKind::Varbinary)
                    .build()
                    .expect("should succeed build column schema"),
            )
            .unwrap()
            .add_key_column(
                column_schema::Builder::new("timestamp".to_string(), DatumKind::Timestamp)
                    .build()
                    .expect("should succeed build column schema"),
            )
            .unwrap()
            .add_normal_column(
                column_schema::Builder::new("field1".to_string(), DatumKind::Double)
                    .build()
                    .expect("should succeed build column schema"),
            )
            .unwrap()
            .add_normal_column(
                column_schema::Builder::new("field2".to_string(), DatumKind::Double)
                    .build()
                    .expect("should succeed build column schema"),
            )
            .unwrap()
            .build()
            .unwrap();

        // Length related test
        assert_eq!(4, schema.columns().len());
        assert_eq!(4, schema.num_columns());
        assert_eq!(2, schema.num_key_columns());
        assert_eq!(1, schema.timestamp_index());

        // Test key columns
        assert_eq!(2, schema.key_columns().len());
        assert_eq!("key1", &schema.key_columns()[0].name);
        assert_eq!("timestamp", &schema.key_columns()[1].name);

        // Test normal columns
        assert_eq!(2, schema.normal_columns().len());
        assert_eq!("field1", &schema.normal_columns()[0].name);
        assert_eq!("field2", &schema.normal_columns()[1].name);

        // Test column_with_name()
        let field1 = schema.column_with_name("field1").unwrap();
        assert_eq!(3, field1.id);
        assert_eq!("field1", field1.name);
        assert!(schema.column_with_name("not exists").is_none());

        // Test column()
        assert_eq!(field1, schema.column(2));

        // Test arrow schema
        let arrow_schema = schema.as_arrow_schema_ref();
        let key1 = arrow_schema.field(0);
        assert_eq!("key1", key1.name());
        let field2 = arrow_schema.field(3);
        assert_eq!("field2", field2.name());

        // Test index_of()
        assert_eq!(1, schema.index_of("timestamp").unwrap());
        assert!(schema.index_of("not exists").is_none());

        // Test pb convert
        let schema_pb = common_pb::TableSchema::from(schema.clone());
        let schema_from_pb = Schema::try_from(schema_pb).unwrap();
        assert_eq!(schema, schema_from_pb);
    }

    #[test]
    fn test_build_unordered() {
        let schema = Builder::new()
            .auto_increment_column_id(true)
            .add_normal_column(
                column_schema::Builder::new("field1".to_string(), DatumKind::Double)
                    .build()
                    .expect("should succeed build column schema"),
            )
            .unwrap()
            .add_key_column(
                column_schema::Builder::new("key1".to_string(), DatumKind::Timestamp)
                    .build()
                    .expect("should succeed build column schema"),
            )
            .unwrap()
            .add_key_column(
                column_schema::Builder::new("key2".to_string(), DatumKind::Varbinary)
                    .build()
                    .expect("should succeed build column schema"),
            )
            .unwrap()
            .add_normal_column(
                column_schema::Builder::new("field2".to_string(), DatumKind::Double)
                    .build()
                    .expect("should succeed build column schema"),
            )
            .unwrap()
            .build()
            .unwrap();

        let columns = schema.columns();
        assert_eq!(2, columns[0].id);
        assert_eq!("key1", columns[0].name);
        assert_eq!(3, columns[1].id);
        assert_eq!("key2", columns[1].name);
        assert_eq!(1, columns[2].id);
        assert_eq!("field1", columns[2].name);
        assert_eq!(4, columns[3].id);
        assert_eq!("field2", columns[3].name);
    }

    #[test]
    fn test_name_exists() {
        let builder = Builder::new()
            .auto_increment_column_id(true)
            .add_normal_column(
                column_schema::Builder::new("field1".to_string(), DatumKind::Double)
                    .build()
                    .expect("should succeed build column schema"),
            )
            .unwrap();
        assert!(builder
            .add_normal_column(
                column_schema::Builder::new("field1".to_string(), DatumKind::Double)
                    .build()
                    .expect("should succeed build column schema"),
            )
            .is_err());
    }

    #[test]
    fn test_id_exists() {
        let builder = Builder::new()
            .add_normal_column(
                column_schema::Builder::new("field1".to_string(), DatumKind::Double)
                    .id(1)
                    .build()
                    .expect("should succeed build column schema"),
            )
            .unwrap();
        assert!(builder
            .add_normal_column(
                column_schema::Builder::new("field2".to_string(), DatumKind::Double)
                    .id(1)
                    .build()
                    .expect("should succeed build column schema")
            )
            .is_err());
    }

    #[test]
    fn test_key_column_type() {
        assert!(Builder::new()
            .add_key_column(
                column_schema::Builder::new("key".to_string(), DatumKind::Double)
                    .id(1)
                    .build()
                    .expect("should succeed build column schema")
            )
            .is_err());
    }

    #[test]
    fn test_timestamp_key_exists() {
        let builder = Builder::new()
            .auto_increment_column_id(true)
            .add_key_column(
                column_schema::Builder::new("key1".to_string(), DatumKind::Timestamp)
                    .build()
                    .expect("should succeed build column schema"),
            )
            .unwrap();
        assert!(builder
            .add_key_column(
                column_schema::Builder::new("key2".to_string(), DatumKind::Timestamp)
                    .build()
                    .expect("should succeed build column schema")
            )
            .is_err());
    }

    #[test]
    fn test_mulitple_timestamp() {
        Builder::new()
            .auto_increment_column_id(true)
            .add_key_column(
                column_schema::Builder::new("key1".to_string(), DatumKind::Timestamp)
                    .build()
                    .expect("should succeed build column schema"),
            )
            .unwrap()
            .add_normal_column(
                column_schema::Builder::new("field1".to_string(), DatumKind::Timestamp)
                    .build()
                    .expect("should succeed build column schema"),
            )
            .unwrap()
            .build()
            .unwrap();
    }

    #[test]
    fn test_missing_timestamp_key() {
        let builder = Builder::new()
            .auto_increment_column_id(true)
            .add_key_column(
                column_schema::Builder::new("key1".to_string(), DatumKind::Varbinary)
                    .build()
                    .expect("should succeed build column schema"),
            )
            .unwrap()
            .add_normal_column(
                column_schema::Builder::new("field1".to_string(), DatumKind::Double)
                    .build()
                    .expect("should succeed build column schema"),
            )
            .unwrap();
        assert!(builder.build().is_err());
    }

    #[test]
    fn test_null_key() {
        assert!(Builder::new()
            .add_key_column(
                column_schema::Builder::new("key1".to_string(), DatumKind::Varbinary)
                    .id(1)
                    .is_nullable(true)
                    .build()
                    .expect("should succeed build column schema")
            )
            .is_err());
    }

    #[test]
    fn test_max_column_id() {
        let builder = Builder::new()
            .add_key_column(
                column_schema::Builder::new("key1".to_string(), DatumKind::Varbinary)
                    .id(2)
                    .build()
                    .expect("should succeed build column schema"),
            )
            .unwrap()
            .add_normal_column(
                column_schema::Builder::new("field1".to_string(), DatumKind::Timestamp)
                    .id(5)
                    .build()
                    .expect("should succeed build column schema"),
            )
            .unwrap();

        let schema = builder
            .auto_increment_column_id(true)
            .add_key_column(
                column_schema::Builder::new("key2".to_string(), DatumKind::Timestamp)
                    .build()
                    .expect("should succeed build column schema"),
            )
            .unwrap()
            .add_normal_column(
                column_schema::Builder::new("field2".to_string(), DatumKind::Timestamp)
                    .build()
                    .expect("should succeed build column schema"),
            )
            .unwrap()
            .build()
            .unwrap();

        let columns = schema.columns();
        // Check key1
        assert_eq!("key1", &columns[0].name);
        assert_eq!(2, columns[0].id);
        // Check key2
        assert_eq!("key2", &columns[1].name);
        assert_eq!(6, columns[1].id);
        // Check field1
        assert_eq!("field1", &columns[2].name);
        assert_eq!(5, columns[2].id);
        // Check field2
        assert_eq!("field2", &columns[3].name);
        assert_eq!(7, columns[3].id);
    }

    fn assert_row_compare(ordering: Ordering, schema: &Schema, row1: &Row, row2: &Row) {
        let schema_with_key = schema.to_record_schema_with_key();
        let lhs = RowWithMeta {
            row: row1,
            schema: &schema_with_key,
        };
        let rhs = RowWithMeta {
            row: row2,
            schema: &schema_with_key,
        };
        assert_eq!(ordering, schema.compare_row(&lhs, &rhs));
    }

    #[test]
    fn test_compare_row() {
        let schema = Builder::new()
            .auto_increment_column_id(true)
            .add_key_column(
                column_schema::Builder::new("key1".to_string(), DatumKind::Varbinary)
                    .build()
                    .expect("should succeed build column schema"),
            )
            .unwrap()
            .add_key_column(
                column_schema::Builder::new("key2".to_string(), DatumKind::Timestamp)
                    .build()
                    .expect("should succeed build column schema"),
            )
            .unwrap()
            .add_normal_column(
                column_schema::Builder::new("field1".to_string(), DatumKind::Double)
                    .build()
                    .expect("should succeed build column schema"),
            )
            .unwrap()
            .build()
            .unwrap();

        // Test equal
        {
            let row1 = Row::from_datums(vec![
                Datum::Varbinary(Bytes::from_static(b"key1")),
                Datum::Timestamp(Timestamp::new(1005)),
                Datum::Double(12.5),
            ]);
            let row2 = Row::from_datums(vec![
                Datum::Varbinary(Bytes::from_static(b"key1")),
                Datum::Timestamp(Timestamp::new(1005)),
                Datum::Double(15.5),
            ]);

            assert_row_compare(Ordering::Equal, &schema, &row1, &row2);
        }

        // Test first key column less
        {
            let row1 = Row::from_datums(vec![
                Datum::Varbinary(Bytes::from_static(b"key2")),
                Datum::Timestamp(Timestamp::new(1005)),
                Datum::Double(17.5),
            ]);
            let row2 = Row::from_datums(vec![
                Datum::Varbinary(Bytes::from_static(b"key5")),
                Datum::Timestamp(Timestamp::new(1005)),
                Datum::Double(17.5),
            ]);

            assert_row_compare(Ordering::Less, &schema, &row1, &row2);
        }

        // Test second key column less
        {
            let row1 = Row::from_datums(vec![
                Datum::Varbinary(Bytes::from_static(b"key2")),
                Datum::Timestamp(Timestamp::new(1002)),
                Datum::Double(17.5),
            ]);
            let row2 = Row::from_datums(vec![
                Datum::Varbinary(Bytes::from_static(b"key2")),
                Datum::Timestamp(Timestamp::new(1005)),
                Datum::Double(17.5),
            ]);

            assert_row_compare(Ordering::Less, &schema, &row1, &row2);
        }

        // Test first key column greater
        {
            let row1 = Row::from_datums(vec![
                Datum::Varbinary(Bytes::from_static(b"key7")),
                Datum::Timestamp(Timestamp::new(1005)),
                Datum::Double(17.5),
            ]);
            let row2 = Row::from_datums(vec![
                Datum::Varbinary(Bytes::from_static(b"key5")),
                Datum::Timestamp(Timestamp::new(1005)),
                Datum::Double(17.5),
            ]);

            assert_row_compare(Ordering::Greater, &schema, &row1, &row2);
        }

        // Test second key column greater
        {
            let row1 = Row::from_datums(vec![
                Datum::Varbinary(Bytes::from_static(b"key2")),
                Datum::Timestamp(Timestamp::new(1007)),
                Datum::Double(17.5),
            ]);
            let row2 = Row::from_datums(vec![
                Datum::Varbinary(Bytes::from_static(b"key2")),
                Datum::Timestamp(Timestamp::new(1005)),
                Datum::Double(17.5),
            ]);

            assert_row_compare(Ordering::Greater, &schema, &row1, &row2);
        }
    }

    #[test]
    fn test_build_from_arrow_schema() {
        let schema = Builder::new()
            .auto_increment_column_id(true)
            .enable_tsid_primary_key(true)
            .add_key_column(
                column_schema::Builder::new(TSID_COLUMN.to_string(), DatumKind::UInt64)
                    .build()
                    .expect("should succeed build column schema"),
            )
            .unwrap()
            .add_key_column(
                column_schema::Builder::new("timestamp".to_string(), DatumKind::Timestamp)
                    .build()
                    .expect("should succeed build column schema"),
            )
            .unwrap()
            .add_normal_column(
                column_schema::Builder::new("value".to_string(), DatumKind::Double)
                    .build()
                    .expect("should succeed build column schema"),
            )
            .unwrap()
            .build()
            .expect("should succeed to build schema");

        let arrow_schema = schema.clone().into_arrow_schema_ref();
        let new_schema = Builder::build_from_arrow_schema(arrow_schema)
            .expect("should succeed to build new schema");

        assert_eq!(schema, new_schema);
    }
}
