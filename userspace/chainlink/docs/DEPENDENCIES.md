# Chainlink Dependencies

This document describes the dependencies used by the chainlink userspace application and the modifications required to make them work in Akuma's `no_std` environment.

## Dependency Graph

```
chainlink-cli (this crate)
├── libakuma (syscall interface)
├── sqld (SQLite VFS library)
│   └── libakuma
└── chainlink (issue tracker library)
    ├── chrono (no_std, alloc)
    ├── serde (no_std, alloc)
    ├── serde_json (no_std, alloc)
    └── anyhow (no_std)
```

## sqld Modifications

The `sqld` crate was refactored to expose its VFS module as a library that other crates can depend on.

### Changes to sqld/Cargo.toml

Added library configuration:

```toml
[lib]
name = "sqld"
path = "src/lib.rs"
```

### New file: sqld/src/lib.rs

```rust
#![no_std]
extern crate alloc;
pub mod vfs;
```

### Changes to sqld/src/vfs.rs

Added `sqlite3_last_insert_rowid` FFI binding:

```rust
extern "C" {
    pub fn sqlite3_last_insert_rowid(db: *mut sqlite3) -> i64;
}

pub fn last_insert_rowid(db: *mut sqlite3) -> i64 {
    unsafe { sqlite3_last_insert_rowid(db) }
}
```

### Changes to sqld/src/main.rs and sqld/src/server.rs

Updated to use the library's vfs module:

```rust
// Before
mod vfs;

// After
use sqld::vfs;
```

## Chainlink Library Modifications

The chainlink library at `/Users/netoneko/github.com/netoneko/chainlink/chainlink` required several modifications to work in `no_std` mode.

### 1. Made clap Optional

The `clap` crate requires `std` and was pulling in dependencies that don't compile in `no_std`. Made it optional behind a `cli` feature:

**chainlink/Cargo.toml:**

```toml
[features]
default = ["std", "rusqlite-backend", "cli"]
std = ["chrono/std", "chrono/now", "serde/std", "serde_json/std", "anyhow/std"]
rusqlite-backend = ["rusqlite"]
cli = ["clap"]  # NEW

[dependencies]
clap = { version = "4", features = ["derive"], optional = true }  # Made optional
```

The binary requires the `cli` feature:

```toml
[[bin]]
name = "chainlink"
path = "src/main.rs"
required-features = ["std", "rusqlite-backend", "cli"]
```

### 2. Timestamp Handling for no_std

The `chrono` crate's `Utc::now()` function requires the `now` feature, which in turn requires `std` for accessing the system clock. In `no_std` mode, we need an alternative.

**chainlink/src/db.rs** - Added helper functions:

```rust
// Timestamp helper for no_std environments
#[cfg(feature = "std")]
fn current_timestamp() -> DateTime<Utc> {
    Utc::now()
}

#[cfg(not(feature = "std"))]
fn current_timestamp() -> DateTime<Utc> {
    // Use a static counter to provide ordering in no_std mode
    use core::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let secs = COUNTER.fetch_add(1, Ordering::SeqCst) as i64;
    // Create a DateTime from Unix timestamp (starting from 2024-01-01)
    DateTime::from_timestamp(1704067200 + secs, 0).unwrap_or_else(|| {
        DateTime::from_timestamp(1704067200, 0).unwrap()
    })
}

#[cfg(feature = "std")]
fn default_timestamp() -> DateTime<Utc> {
    Utc::now()
}

#[cfg(not(feature = "std"))]
fn default_timestamp() -> DateTime<Utc> {
    DateTime::from_timestamp(1704067200, 0).unwrap()
}
```

Then replaced all `Utc::now()` calls:
- `Utc::now()` → `current_timestamp()`
- `unwrap_or_else(Utc::now)` → `unwrap_or_else(default_timestamp)`

### 3. ToString Trait Import

**chainlink/src/utils.rs:**

```rust
#[cfg(not(feature = "std"))]
use alloc::string::{String, ToString};  // Added ToString
```

## SqldBackend Implementation

The `SqldBackend` struct implements chainlink's `DatabaseBackend` trait. This is the bridge between the chainlink library and sqld's VFS.

### Parameter Binding

The chainlink library uses parameterized SQL queries:

```rust
backend.execute("SELECT * FROM issues WHERE id = ?1", &[Value::Integer(42)])
```

However, sqld's `execute_sql` only accepts raw SQL strings. The backend manually substitutes parameters:

```rust
fn bind_params(sql: &str, params: &[Value]) -> String {
    let mut result = String::from(sql);
    
    // Replace in reverse order to avoid index shifting
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
```

### NULL Value Handling

sqld returns `"NULL"` as a literal string for NULL values. The backend converts these:

```rust
let values: Vec<Option<String>> = sqld_row.iter().map(|val| {
    if val == "NULL" {
        None
    } else {
        Some(val.clone())
    }
}).collect();
```

## Future Improvements

### Real-time Clock Support

Currently, timestamps use a monotonic counter. If Akuma exposes the PL031 RTC via a syscall, the chainlink library could be modified to:

1. Accept a timestamp provider function/trait
2. Use libakuma's RTC syscall in no_std mode

### Upstream Contributions

The chainlink library modifications could be contributed upstream:

1. Make `clap` optional (already done locally)
2. Add a `timestamp-provider` feature for custom timestamp sources
3. Document no_std usage in the library README

## Version Information

- chainlink library: 0.1.0 (local)
- sqld: 0.1.0
- libakuma: 0.1.0
- chrono: 0.4.x (no_std, alloc features)
- serde: 1.x (no_std, alloc, derive features)
- serde_json: 1.x (no_std, alloc features)
