//! DuckDB storage backend implementation.
//!
//! This module provides a high-performance storage backend using DuckDB,
//! an embedded analytical database. The implementation supports:
//! - In-memory and persistent storage options
//! - Efficient batch operations
//! - SQL query capabilities
//! - Time-based filtering
//!
//! # Configuration
//!
//! The DuckDB backend can be configured using the following options:
//!
//! ```toml
//! [engine]
//! engine = "duckdb"
//! connection = ":memory:"  # Use ":memory:" for in-memory or file path
//! options = {
//!     threads = "4",      # Optional: Number of threads (default: 4)
//!     read_only = "false" # Optional: Read-only mode (default: false)
//! }
//! ```
//!
//! Or via command line:
//!
//! ```bash
//! hyprstream \
//!   --engine duckdb \
//!   --engine-connection ":memory:" \
//!   --engine-options threads=4 \
//!   --engine-options read_only=false
//! ```
//!
//! DuckDB is particularly well-suited for analytics workloads and
//! provides excellent performance for both caching and primary storage.

use std::collections::HashMap;
use std::sync::Arc;
use duckdb::{Connection, Config};
use tokio::sync::Mutex;
use tonic::Status;
use crate::metrics::MetricRecord;
use crate::config::Credentials;
use crate::storage::StorageBackend;
use crate::storage::cache::{CacheManager, CacheEviction};
use async_trait::async_trait;

/// DuckDB-based storage backend for metrics.
#[derive(Clone)]
pub struct DuckDbBackend {
    conn: Arc<Mutex<Connection>>,
    connection_string: String,
    options: HashMap<String, String>,
    cache_manager: CacheManager,
}

#[async_trait]
impl CacheEviction for DuckDbBackend {
    async fn execute_eviction(&self, query: &str) -> Result<(), Status> {
        let conn = self.conn.clone();
        let query = query.to_string();
        tokio::spawn(async move {
            let conn_guard = conn.lock().await;
            if let Err(e) = conn_guard.execute_batch(&query) {
                eprintln!("Background eviction error: {}", e);
            }
        });
        Ok(())
    }
}

#[async_trait]
impl StorageBackend for DuckDbBackend {
    async fn init(&self) -> Result<(), Status> {
        self.create_tables().await
    }

    async fn insert_metrics(&self, metrics: Vec<MetricRecord>) -> Result<(), Status> {
        // Check if eviction is needed
        if let Some(cutoff) = self.cache_manager.should_evict().await? {
            let query = self.cache_manager.eviction_query(cutoff);
            self.execute_eviction(&query).await?;
        }

        let mut query = String::from("INSERT INTO metrics (timestamp, metric_id, value_running_window_sum, value_running_window_avg, value_running_window_count) VALUES ");
        let mut first = true;

        for metric in metrics {
            if !first {
                query.push_str(", ");
            }
            first = false;

            query.push_str(&format!(
                "({}, '{}', {}, {}, {})",
                metric.timestamp,
                metric.metric_id,
                metric.value_running_window_sum,
                metric.value_running_window_avg,
                metric.value_running_window_count
            ));
        }

        self.execute(&query).await
    }

    async fn query_metrics(&self, from_timestamp: i64) -> Result<Vec<MetricRecord>, Status> {
        // Check if eviction is needed
        if let Some(cutoff) = self.cache_manager.should_evict().await? {
            let query = self.cache_manager.eviction_query(cutoff);
            self.execute_eviction(&query).await?;
        }

        let query = format!(
            "SELECT timestamp, metric_id, value_running_window_sum, value_running_window_avg, value_running_window_count \
             FROM metrics WHERE timestamp >= {}",
            from_timestamp
        );

        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(&query)
            .map_err(|e| Status::internal(e.to_string()))?;

        let mut rows = stmt.query([])
            .map_err(|e| Status::internal(e.to_string()))?;

        let mut metrics = Vec::new();
        while let Some(row) = rows.next().map_err(|e| Status::internal(e.to_string()))? {
            let metric = MetricRecord {
                timestamp: row.get(0).map_err(|e| Status::internal(e.to_string()))?,
                metric_id: row.get(1).map_err(|e| Status::internal(e.to_string()))?,
                value_running_window_sum: row.get(2).map_err(|e| Status::internal(e.to_string()))?,
                value_running_window_avg: row.get(3).map_err(|e| Status::internal(e.to_string()))?,
                value_running_window_count: row.get(4).map_err(|e| Status::internal(e.to_string()))?,
            };
            metrics.push(metric);
        }

        Ok(metrics)
    }

    async fn prepare_sql(&self, query: &str) -> Result<Vec<u8>, Status> {
        Ok(query.as_bytes().to_vec())
    }

    async fn query_sql(&self, statement_handle: &[u8]) -> Result<Vec<MetricRecord>, Status> {
        let sql = std::str::from_utf8(statement_handle)
            .map_err(|e| Status::internal(e.to_string()))?;
        self.query_metrics(sql.parse().unwrap_or(0)).await
    }

    fn new_with_options(
        connection_string: &str,
        options: &HashMap<String, String>,
        credentials: Option<&Credentials>,
    ) -> Result<Self, Status> {
        let mut all_options = options.clone();
        if let Some(creds) = credentials {
            all_options.insert("username".to_string(), creds.username.clone());
            all_options.insert("password".to_string(), creds.password.clone());
        }

        let ttl = all_options.get("ttl")
            .and_then(|s| s.parse().ok())
            .map(|ttl| if ttl == 0 { None } else { Some(ttl) })
            .unwrap_or(None);

        Self::new(connection_string.to_string(), all_options, ttl)
    }
}

impl DuckDbBackend {
    /// Creates a new DuckDB backend instance.
    pub fn new(connection_string: String, options: HashMap<String, String>, ttl: Option<u64>) -> Result<Self, Status> {
        let config = Config::default();
        let conn = Connection::open_with_flags(&connection_string, config)
            .map_err(|e| Status::internal(e.to_string()))?;

        let backend = Self {
            conn: Arc::new(Mutex::new(conn)),
            connection_string,
            options,
            cache_manager: CacheManager::new(ttl),
        };

        // Initialize tables
        let backend_clone = backend.clone();
        tokio::spawn(async move {
            if let Err(e) = backend_clone.create_tables().await {
                eprintln!("Failed to create tables: {}", e);
            }
        });

        Ok(backend)
    }

    /// Creates a new DuckDB backend with an in-memory database.
    pub fn new_in_memory() -> Result<Self, Status> {
        Self::new(":memory:".to_string(), HashMap::new(), Some(0))
    }

    /// Creates the necessary tables in the database.
    async fn create_tables(&self) -> Result<(), Status> {
        let create_table = r#"
            CREATE TABLE IF NOT EXISTS metrics (
                timestamp BIGINT NOT NULL,
                metric_id VARCHAR NOT NULL,
                value_running_window_sum DOUBLE NOT NULL,
                value_running_window_avg DOUBLE NOT NULL,
                value_running_window_count BIGINT NOT NULL
            )
        "#;

        self.execute(create_table).await?;

        // Create a more optimized index for TTL-based eviction
        let create_index = r#"
            CREATE INDEX IF NOT EXISTS metrics_timestamp_idx ON metrics(timestamp) WITH (prefetch_blocks = 8)
        "#;

        self.execute(create_index).await
    }

    /// Executes a SQL query.
    async fn execute(&self, query: &str) -> Result<(), Status> {
        let conn = self.conn.lock().await;
        conn.execute_batch(query)
            .map_err(|e| Status::internal(e.to_string()))
    }
}
