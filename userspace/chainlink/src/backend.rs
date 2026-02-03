//! SQLite backend implementation using sqld's VFS
//!
//! This module provides a `DatabaseBackend` implementation for chainlink
//! that uses sqld's VFS to access SQLite databases in Akuma.

use alloc::string::String;
use alloc::vec::Vec;
use alloc::format;
use core::cell::Cell;

use chainlink::backend::{BackendError, DatabaseBackend, QueryResult, Row, Value};
use sqld::vfs;

/// SQLite backend using sqld's VFS
pub struct SqldBackend {
    db: *mut vfs::sqlite3,
    last_rowid: Cell<i64>,
}

// Safety: SqldBackend is only used in single-threaded userspace
unsafe impl Send for SqldBackend {}
unsafe impl Sync for SqldBackend {}

impl SqldBackend {
    /// Bind parameters into SQL by replacing ?1, ?2, etc. with values
    fn bind_params(sql: &str, params: &[Value]) -> String {
        let mut result = String::from(sql);
        
        // Replace parameters in reverse order to avoid index shifting
        // e.g., replace ?10 before ?1
        for (i, param) in params.iter().enumerate().rev() {
            let placeholder = format!("?{}", i + 1);
            let value_str = match param {
                Value::Null => String::from("NULL"),
                Value::Integer(n) => format!("{}", n),
                Value::Text(s) => {
                    // Escape single quotes by doubling them
                    let escaped = s.replace('\'', "''");
                    format!("'{}'", escaped)
                }
            };
            result = result.replace(&placeholder, &value_str);
        }
        
        result
    }
    
    /// Convert sqld's QueryResult to chainlink's QueryResult
    fn convert_result(sqld_result: vfs::QueryResult, last_rowid: i64) -> QueryResult {
        let rows: Vec<Row> = sqld_result.rows.iter().map(|sqld_row| {
            let values: Vec<Option<String>> = sqld_row.iter().map(|val| {
                if val == "NULL" {
                    None
                } else {
                    Some(val.clone())
                }
            }).collect();
            Row::new(values)
        }).collect();
        
        QueryResult {
            columns: sqld_result.columns,
            rows,
            changes: sqld_result.changes as u64,
            last_insert_rowid: last_rowid,
        }
    }
}

impl DatabaseBackend for SqldBackend {
    fn open(path: &str) -> Result<Self, BackendError> {
        // Note: vfs::init() must be called before this
        let db = vfs::open_db(path)
            .map_err(|e| BackendError::new(e))?;
        
        Ok(SqldBackend {
            db,
            last_rowid: Cell::new(0),
        })
    }
    
    fn execute(&self, sql: &str, params: &[Value]) -> Result<QueryResult, BackendError> {
        // Bind parameters into SQL
        let bound_sql = Self::bind_params(sql, params);
        
        // Execute the SQL
        let sqld_result = vfs::execute_sql(self.db, &bound_sql)
            .map_err(|e| BackendError::new(e))?;
        
        // Update last insert rowid
        let last_rowid = vfs::last_insert_rowid(self.db);
        self.last_rowid.set(last_rowid);
        
        Ok(Self::convert_result(sqld_result, last_rowid))
    }
    
    fn execute_batch(&self, sql: &str) -> Result<(), BackendError> {
        // Split by semicolons and execute each statement
        // This is a simple implementation that doesn't handle semicolons in strings
        for stmt in sql.split(';') {
            let trimmed = stmt.trim();
            if !trimmed.is_empty() {
                vfs::execute_sql(self.db, trimmed)
                    .map_err(|e| BackendError::new(e))?;
            }
        }
        Ok(())
    }
    
    fn last_insert_rowid(&self) -> i64 {
        self.last_rowid.get()
    }
}

impl Drop for SqldBackend {
    fn drop(&mut self) {
        vfs::close_db(self.db);
    }
}
