# sqld - SQLite Daemon

A userspace SQLite server for Akuma that provides a TCP interface for executing SQL queries.

## Building

### Download SQLite 3 Source

Download the SQLite amalgamation source:

```bash
curl -O https://www.sqlite.org/2024/sqlite-amalgamation-3450100.zip
unzip sqlite-amalgamation-3450100.zip
mv sqlite-amalgamation-3450100 sqlite3
```

### Build

```bash
cd userspace
./build.sh
```

## Usage

See [docs/SQLD.md](../../docs/SQLD.md) for full documentation.
