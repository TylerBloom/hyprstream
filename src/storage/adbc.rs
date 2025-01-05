//! ADBC (Arrow Database Connectivity) storage backend implementation.
//!
//! This module provides a storage backend using ADBC, enabling:
//! - Connection to any ADBC-compliant database
//! - High-performance data transport using Arrow's columnar format
//! - Connection pooling and prepared statements
//! - Support for various database systems (PostgreSQL, MySQL, etc.)
//!
//! The implementation is optimized for efficient data transfer and
//! query execution using Arrow's native formats.

use crate::config::AdbcConfig;
use crate::metrics::MetricRecord;
use crate::storage::StorageBackend;
use adbc_core::{
    driver_manager::{ManagedConnection, ManagedDriver},
    options::{AdbcVersion, OptionDatabase},
    Connection, Database, Driver, Statement,
};
use arrow_array::{Float64Array, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use async_trait::async_trait;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::Status;

/// ADBC-based storage backend for metrics.
///
/// This backend provides:
/// - Integration with ADBC-compliant databases
/// - Connection pooling for optimal performance
/// - Prepared statement management
/// - Efficient data transport using Arrow format
///
/// The implementation supports multiple database systems through
/// ADBC drivers and handles connection management automatically.
pub struct AdbcBackend {
    /// Thread-safe connection to the database
    conn: Arc<Mutex<ManagedConnection>>,
    /// Counter for generating unique statement handles
    statement_counter: AtomicU64,
    /// Cache of prepared statements
    prepared_statements: Arc<Mutex<Vec<(u64, String)>>>,
}

impl AdbcBackend {
    /// Creates a new ADBC backend with the specified configuration.
    ///
    /// This method:
    /// 1. Loads the ADBC driver
    /// 2. Configures the database connection
    /// 3. Sets up the connection pool
    ///
    /// # Arguments
    ///
    /// * `config` - ADBC configuration including driver path and connection settings
    ///
    /// # Returns
    ///
    /// * `Result<Self, Status>` - Configured backend or error
    pub fn new(config: &AdbcConfig) -> Result<Self, Status> {
        let mut driver =
            ManagedDriver::load_dynamic_from_filename(&config.driver_path, None, AdbcVersion::V100)
                .map_err(|e| Status::internal(format!("Failed to load ADBC driver: {}", e)))?;

        let opts = vec![
            (OptionDatabase::Uri, config.url.as_str().into()),
            (OptionDatabase::Username, config.username.as_str().into()),
            (OptionDatabase::Password, config.password.as_str().into()),
            (
                OptionDatabase::Other("database".into()),
                config.database.as_str().into(),
            ),
            (
                OptionDatabase::Other("pool.max_connections".into()),
                config.pool.max_connections.to_string().as_str().into(),
            ),
            (
                OptionDatabase::Other("pool.min_connections".into()),
                config.pool.min_connections.to_string().as_str().into(),
            ),
            (
                OptionDatabase::Other("pool.acquire_timeout".into()),
                config.pool.acquire_timeout_secs.to_string().as_str().into(),
            ),
        ];

        let mut database = driver
            .new_database_with_opts(opts)
            .map_err(|e| Status::internal(format!("Failed to create database: {}", e)))?;

        let connection = database
            .new_connection()
            .map_err(|e| Status::internal(format!("Failed to create connection: {}", e)))?;

        Ok(Self {
            conn: Arc::new(Mutex::new(connection)),
            statement_counter: AtomicU64::new(0),
            prepared_statements: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Gets a connection from the pool.
    ///
    /// This method provides thread-safe access to the database connection.
    async fn get_connection(
        &self,
    ) -> Result<tokio::sync::MutexGuard<'_, ManagedConnection>, Status> {
        Ok(self.conn.lock().await)
    }

    /// Creates the necessary database tables and indexes.
    ///
    /// This method:
    /// 1. Creates the metrics table if it doesn't exist
    /// 2. Sets up appropriate column types for metric data
    /// 3. Creates a primary key for efficient lookups
    async fn create_tables(&self) -> Result<(), Status> {
        let mut conn = self.get_connection().await?;
        let mut stmt = conn
            .new_statement()
            .map_err(|e| Status::internal(format!("Failed to create statement: {}", e)))?;

        stmt.set_sql_query(
            "CREATE TABLE IF NOT EXISTS metrics (
                metric_id TEXT NOT NULL,
                timestamp BIGINT NOT NULL,
                value_running_window_sum DOUBLE PRECISION NOT NULL,
                value_running_window_avg DOUBLE PRECISION NOT NULL,
                value_running_window_count BIGINT NOT NULL,
                PRIMARY KEY (metric_id, timestamp)
            )",
        )
        .map_err(|e| Status::internal(format!("Failed to set query: {}", e)))?;

        stmt.execute_update()
            .map_err(|e| Status::internal(format!("Failed to execute create table: {}", e)))?;

        Ok(())
    }

    /// Converts metrics to an Arrow RecordBatch.
    ///
    /// This method efficiently converts metric records to Arrow's columnar
    /// format for optimal data transport.
    ///
    /// # Arguments
    ///
    /// * `metrics` - Slice of MetricRecord instances to convert
    ///
    /// # Returns
    ///
    /// * `Result<RecordBatch, Status>` - Arrow RecordBatch or error
    fn metrics_to_record_batch(metrics: &[MetricRecord]) -> Result<RecordBatch, Status> {
        let schema = Schema::new(vec![
            Field::new("metric_id", DataType::Utf8, false),
            Field::new("timestamp", DataType::Int64, false),
            Field::new("value_running_window_sum", DataType::Float64, false),
            Field::new("value_running_window_avg", DataType::Float64, false),
            Field::new("value_running_window_count", DataType::Int64, false),
        ]);

        let metric_ids = StringArray::from_iter(metrics.iter().map(|m| Some(m.metric_id.as_str())));
        let timestamps = Int64Array::from_iter(metrics.iter().map(|m| Some(m.timestamp)));
        let sums =
            Float64Array::from_iter(metrics.iter().map(|m| Some(m.value_running_window_sum)));
        let avgs =
            Float64Array::from_iter(metrics.iter().map(|m| Some(m.value_running_window_avg)));
        let counts =
            Int64Array::from_iter(metrics.iter().map(|m| Some(m.value_running_window_count)));

        RecordBatch::try_new(
            Arc::new(schema),
            vec![
                Arc::new(metric_ids),
                Arc::new(timestamps),
                Arc::new(sums),
                Arc::new(avgs),
                Arc::new(counts),
            ],
        )
        .map_err(|e| Status::internal(e.to_string()))
    }
}

#[async_trait]
impl StorageBackend for AdbcBackend {
    /// Initializes the ADBC backend.
    ///
    /// Creates necessary tables and indexes for metric storage.
    async fn init(&self) -> Result<(), Status> {
        self.create_tables().await
    }

    /// Inserts a batch of metrics into storage.
    ///
    /// This method:
    /// 1. Converts metrics to Arrow format
    /// 2. Prepares an insert statement
    /// 3. Binds the data and executes the insert
    async fn insert_metrics(&self, metrics: Vec<MetricRecord>) -> Result<(), Status> {
        let batch = Self::metrics_to_record_batch(&metrics)?;

        let mut conn = self.get_connection().await?;
        let mut stmt = conn
            .new_statement()
            .map_err(|e| Status::internal(format!("Failed to create statement: {}", e)))?;

        stmt.set_sql_query(
            "INSERT INTO metrics (
                metric_id, timestamp, value_running_window_sum,
                value_running_window_avg, value_running_window_count
            ) VALUES (?, ?, ?, ?, ?)",
        )
        .map_err(|e| Status::internal(format!("Failed to set query: {}", e)))?;

        stmt.bind(batch)
            .map_err(|e| Status::internal(format!("Failed to bind values: {}", e)))?;

        stmt.execute_update()
            .map_err(|e| Status::internal(format!("Failed to execute insert: {}", e)))?;

        Ok(())
    }

    /// Queries metrics from a given timestamp.
    ///
    /// This method:
    /// 1. Prepares a parameterized query
    /// 2. Binds the timestamp parameter
    /// 3. Executes the query and processes results
    async fn query_metrics(&self, from_timestamp: i64) -> Result<Vec<MetricRecord>, Status> {
        let mut conn = self.get_connection().await?;
        let mut stmt = conn
            .new_statement()
            .map_err(|e| Status::internal(format!("Failed to create statement: {}", e)))?;

        stmt.set_sql_query(
            "SELECT metric_id, timestamp, value_running_window_sum, value_running_window_avg, value_running_window_count 
             FROM metrics WHERE timestamp >= ?",
        )
        .map_err(|e| Status::internal(format!("Failed to set query: {}", e)))?;

        let param_batch = RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new(
                "timestamp",
                DataType::Int64,
                false,
            )])),
            vec![Arc::new(Int64Array::from_iter_values([from_timestamp]))],
        )
        .map_err(|e| Status::internal(e.to_string()))?;

        stmt.bind(param_batch)
            .map_err(|e| Status::internal(format!("Failed to bind values: {}", e)))?;

        let mut reader = stmt
            .execute()
            .map_err(|e| Status::internal(format!("Failed to execute query: {}", e)))?;

        let mut metrics = Vec::new();
        while let Some(batch_result) = reader.next() {
            let batch = batch_result
                .map_err(|e| Status::internal(format!("Failed to get record batch: {}", e)))?;
            metrics.extend(self.record_batch_to_metrics(&batch)?);
        }

        if metrics.is_empty() {
            return Err(Status::not_found(
                "No metrics found for the given timestamp",
            ));
        }

        Ok(metrics)
    }

    /// Prepares a SQL statement for execution.
    ///
    /// This method:
    /// 1. Generates a unique statement handle
    /// 2. Caches the SQL query
    /// 3. Returns the serialized handle
    async fn prepare_sql(&self, query: &str) -> Result<Vec<u8>, Status> {
        let handle = self.statement_counter.fetch_add(1, Ordering::SeqCst);
        let mut statements = self.prepared_statements.lock().await;
        statements.push((handle, query.to_string()));

        Ok(handle.to_le_bytes().to_vec())
    }

    /// Executes a prepared SQL statement.
    ///
    /// This method:
    /// 1. Deserializes the statement handle
    /// 2. Retrieves the cached SQL query
    /// 3. Executes the query and processes results
    async fn query_sql(&self, statement_handle: &[u8]) -> Result<Vec<MetricRecord>, Status> {
        let handle = u64::from_le_bytes(
            statement_handle
                .try_into()
                .map_err(|_| Status::invalid_argument("Invalid statement handle"))?,
        );

        let statements = self.prepared_statements.lock().await;
        let sql = statements
            .iter()
            .find(|(h, _)| *h == handle)
            .map(|(_, sql)| sql.as_str())
            .ok_or_else(|| Status::invalid_argument("Statement handle not found"))?;

        let mut conn = self.get_connection().await?;
        let mut stmt = conn
            .new_statement()
            .map_err(|e| Status::internal(format!("Failed to create statement: {}", e)))?;

        stmt.set_sql_query(sql)
            .map_err(|e| Status::internal(format!("Failed to set query: {}", e)))?;

        let mut reader = stmt
            .execute()
            .map_err(|e| Status::internal(format!("Failed to execute query: {}", e)))?;

        let mut metrics = Vec::new();
        while let Some(batch_result) = reader.next() {
            let batch = batch_result
                .map_err(|e| Status::internal(format!("Failed to get record batch: {}", e)))?;
            metrics.extend(self.record_batch_to_metrics(&batch)?);
        }

        Ok(metrics)
    }
}
