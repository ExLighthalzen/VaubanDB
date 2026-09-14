# VaubanDB

**A SQL Server-compatible database server, built for containers.**

VaubanDB speaks the TDS wire protocol and the T-SQL dialect, and exposes the `sys.*` catalog.
Applications and tools written for SQL Server connect to it with the drivers they already
use: `sqlcmd`, `Microsoft.Data.SqlClient`, `mssql-jdbc`, ODBC, `tiberius`. No new driver, no
query rewrite.

It is a new engine written in Rust from the ground up, not a layer over another database.

## Why VaubanDB

- **Runs where you run.** Native builds for `linux/arm64` and `linux/amd64`, plus macOS for
  development. The container image is published for both Linux architectures, so the same
  image runs on Graviton, Ampere, Apple silicon and x86 nodes.
- **Starts in milliseconds.** A single binary of about 10 MB; a fresh server accepts
  its first connection in a few tens of milliseconds and idles in a few megabytes of RAM.
  Spin one up per test, per branch, per pull request.
- **Cloud native by design.** Configured by environment variables or a small TOML file,
  logs as JSON lines, runs as a non-root user, stops cleanly on `SIGTERM`. Prometheus
  metrics, health and readiness endpoints, and a Helm chart are on the roadmap.
- **Open source.** BSD-3-Clause, no dual licensing, no feature gates.

## Status

VaubanDB is under active development and not yet ready for production data.

Today: clients connect over TLS or in clear and log in with SQL authentication; `CREATE`,
`ALTER` and `DROP` for databases, tables, indexes and constraints; `SELECT` over a table with
`WHERE`, the scalar built-in functions, `CAST`/`CONVERT` and the SQL Server type system,
collation and rounding rules; `SET` options; the `sys.*` and `INFORMATION_SCHEMA` views that
tools such as SQL Server Management Studio read at connection. Databases are kept in memory.

In progress: `INSERT`, `UPDATE`, `DELETE`, joins, aggregates, variables, transactions and
locking. Next: durable storage on disk, stored procedures and the rest of T-SQL in the order
the SQL Server versions introduced it, backup and restore, observability endpoints, scheduled
jobs.

## Quick start

```bash
docker run --rm -p 1433:1433 -e VAUBAN_SA_PASSWORD='a-password' ghcr.io/exlighthalzen/vaubandb
sqlcmd -S localhost,1433 -U sa -P 'a-password' -Q "SELECT @@VERSION"
```

SqlClient connection string:
`Server=localhost,1433;User Id=sa;Password=…;Encrypt=Optional;TrustServerCertificate=True`

JDBC URL: `jdbc:sqlserver://localhost:1433;user=sa;password=…;encrypt=false`

The server announces itself as VaubanDB: `SELECT @@VERSION` and `SERVERPROPERTY('Edition')`
say so, and `SERVERPROPERTY('VaubanDB')` returns `1`.

## Build from source

Stable Rust, version pinned in `rust-toolchain.toml`.

```bash
cargo build --release
target/release/vauban serve --in-memory --sa-password 'a-password'
```

## Configuration

| Option | Environment | Default | Meaning |
|---|---|---|---|
| `--bind` | | `0.0.0.0` | Address to listen on |
| `--port` | | `1433` | TCP port |
| `--sa-password` | `VAUBAN_SA_PASSWORD` | none | Password of the `sa` login; required unless `--no-auth` |
| `--no-auth` | | off | Accept any login (development only) |
| `--in-memory` | | off | Keep every database in memory (required in this version) |
| `--data <DIR>` | | none | Data directory; holds the generated TLS certificate |
| `--encrypt` | | `optional` | `off`, `optional` or `required`: encryption announced to clients |
| `--cert`, `--key` | | none | Server certificate and private key (PEM), always together |
| `--log-level` | `RUST_LOG` | `info` | `trace`, `debug`, `info`, `warn`, `error` |
| `--log-format` | | `text` | `text` or `json` (one object per line) |
| `--config` | | `./vauban.toml` | Configuration file |
| | `HOSTNAME` | `vauban` | Name reported as `@@SERVERNAME` |

## Code layout

One Cargo workspace, one crate per layer: `vauban-tds` (protocol), `vauban-parser` (T-SQL),
`vauban-binder`, `vauban-planner`, `vauban-executor`, `vauban-types`, `vauban-sysfn`
(built-in functions), `vauban-catalog` (`sys.*`), `vauban-txn`, `vauban-storage`,
`vauban-session`, `vauban-compat` (procedures and properties the tools expect),
`vauban-errors`, `vauban-cli` (the `vauban` binary).

```bash
cargo test --workspace
```

## License

BSD-3-Clause, see [`LICENSE`](LICENSE).

SQL Server is a trademark of Microsoft Corporation. VaubanDB is an independent project,
neither affiliated with nor endorsed by Microsoft.
