# vauban-cli

The `vauban` binary: reads its configuration, assembles the engine, starts the TDS
server, logs, and stops cleanly on SIGINT/SIGTERM. The process announces itself as
VaubanDB, a SQL Server-compatible alternative, not as Microsoft SQL Server.

Before it listens, `serve` fills the process-wide function registry of `sysfn`
(`sysfn::register_builtins()`, then `compat::register_functions()`), so that the first
client to connect already sees every function. The binary is the only place that can do it:
`compat` depends on `session`, never the other way round, so `session` cannot register its
own functions. Both calls are idempotent.

```
vauban serve [OPTIONS]    # start the server
vauban version            # print the version and the build target
```

## Options of `serve`

| Option | Default | Meaning |
|---|---|---|
| `--bind <ADDR>` | `0.0.0.0` | Address to listen on |
| `--port <N>` | `1433` | TCP port |
| `--data <DIR>` | unset | Instance directory: `vauban.ctl`, `data`, `wal`. Also holds the generated TLS certificate (`tls/`). Mutually exclusive with `--in-memory` |
| `--in-memory` | off | Keep the databases in memory. Mutually exclusive with `--data`. Exactly one of the two is required |
| `--sa-password <PASSWORD>` | none | Password of the `sa` login. Prefer `VAUBAN_SA_PASSWORD` |
| `--no-auth` | off | Accept any login and password (development only) |
| `--encrypt <off\|optional\|required>` | `optional` | Encryption policy announced at PRELOGIN (see [TLS](#tls)) |
| `--cert <PEM>` / `--key <PEM>` | none | Server certificate and private key, always together (see [TLS](#tls)) |
| `--log-level <trace\|debug\|info\|warn\|error>` | `info` | Minimum level written to the log |
| `--log-format <text\|json>` | `text` | One human-readable line, or one JSON object per line |
| `--config <FILE>` | `./vauban.toml` if it exists | Configuration file |

Environment:

- `VAUBAN_SA_PASSWORD`: the `sa` password. An empty value counts as unset.
- `VAUBAN_PROGRAM_NAME`: LOGINACK `ProgName`. Empty counts as unset. At most 255 UTF-16
  code units: LOGINACK carries the name as a B_VARCHAR, whose length byte counts units, so
  one character outside the basic multilingual plane weighs two. A longer name stops the
  startup with exit code 2 and a message naming both lengths, rather than cutting the first
  login without an error. `VAUBAN_VERSION_BANNER` and `VAUBAN_EDITION` are not checked: they
  reach the client as `nvarchar` values, not as a B_VARCHAR.
- `VAUBAN_VERSION_BANNER`: text of `SELECT @@VERSION`. Empty counts as unset.
- `VAUBAN_EDITION`: `SERVERPROPERTY('Edition')`. Empty counts as unset. When this is set
  and `VAUBAN_VERSION_BANNER` is not, the last line of the default banner is rewritten so
  the two stay in agreement.
- `RUST_LOG`: a `tracing_subscriber::EnvFilter` directive; when set it wins over `--log-level`.
- `HOSTNAME`: the name announced to clients (`@@SERVERNAME`); `vauban` when unset.

The default product name, banner and edition are VaubanDB: an alternative, not a
Microsoft product. Setting any of the three identity variables logs a single warning
(`product identity overridden by the operator`). The numeric surface (`ProductVersion`,
`EngineEdition` 3) and `SERVERPROPERTY('VaubanDB') = 1` are not configurable.

Without `--no-auth`, a `sa` password is required. `--no-auth` with a password logs a warning
and ignores the password. A password given on the command line logs a warning: it is visible
in the process list.

## TLS

`--encrypt` takes one of three policies:

| Policy | Announced at PRELOGIN | Effect |
|---|---|---|
| `optional` (default) | `ENCRYPT_OFF` | The login is encrypted when the client asks for it; the rest is clear text unless the client requires encryption |
| `required` | `ENCRYPT_REQ` | Everything is encrypted; a client that cannot encrypt is refused |
| `off` | `ENCRYPT_NOT_SUP` | No TLS at all, for development only |

With `optional` or `required` the server needs a certificate. Three sources, in order:

1. **`--cert` and `--key`**: a PEM certificate (the chain, leaf first) and its PEM private
   key (PKCS#8, PKCS#1 RSA or SEC1 EC). A missing file or a file without a PEM block is a
   configuration error naming the path.
2. **`--data <DIR>`** without `--cert`: the server reads `<DIR>/tls/server.crt` and
   `<DIR>/tls/server.key`. On the first start they do not exist: a self-signed certificate
   is generated and written there (the key with mode `0600` on Unix), and reused on every
   later start. One file without the other is refused, never overwritten.
3. **Neither**: an ephemeral self-signed certificate lives in memory and changes at every
   start; a warning says so.

The generated certificate has subject `CN=<server name>`, SAN `DNS:<server name>`,
`DNS:localhost`, `IP:127.0.0.1`, an ECDSA P-256 key and a ten-year validity. The server
name is the one announced to clients (`HOSTNAME`). The log prints the SHA256 fingerprint of
the certificate in use at `info`; the private key never appears in the log.

Only **TLS 1.2** is offered: the TDS 7.4 handshake travels inside PRELOGIN packets, which
does not carry TLS 1.3. TLS 1.3 comes with TDS 8.0 (strict mode) in a later version.

### Client side

A self-signed certificate is not trusted by default: tell the client to trust the server
certificate, or install the certificate on the client.

| Client | Setting |
|---|---|
| `sqlcmd` | `-C` (`sqlcmd -S localhost,1433 -U sa -P ... -C`) |
| SqlClient (.NET) | `TrustServerCertificate=true` in the connection string |
| JDBC | `trustServerCertificate=true` in the URL or the properties |
| ODBC 17/18 | `TrustServerCertificate=yes` in the connection string |
| `tiberius` | `config.trust_cert()` |

With `--encrypt off`, clients must not require encryption (`Encrypt=false`, `-N o` for
`sqlcmd`); a client that insists is refused.

### Replacing the certificate

- With `--cert`/`--key`: point them at the new files and restart.
- With `--data`: stop the server, replace `<DIR>/tls/server.crt` and `<DIR>/tls/server.key`
  (or delete both to get a new self-signed one), restart. The new fingerprint is in the log.

## Precedence

Command line > environment variable > configuration file > default.

The command line only overrides what it explicitly sets: an option left out on the command
line lets the file speak. A flag (`--in-memory`, `--no-auth`) can only turn a setting on; the
file can turn it on too (`in_memory = true`).

## `vauban.toml`

Keys are the option names with underscores instead of dashes. Every key is optional; an
unknown key is an error.

```toml
bind = "127.0.0.1"
port = 1433
data = "/var/lib/vauban"
in_memory = true
sa_password = "Str0ngPassw0rd!"
no_auth = false
encrypt = "off"          # "off" | "optional" | "required"
cert = "/etc/vauban/server.pem"
key = "/etc/vauban/server.key"
log_level = "info"       # "trace" | "debug" | "info" | "warn" | "error"
log_format = "text"      # "text" | "json"
# Optional product identity. Empty is unset. Do not copy a third-party product name.
# program_name = "CustomDB"
# version_banner = "CustomBanner"
# edition = "Custom Edition"
```

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Clean shutdown after SIGINT or SIGTERM |
| `1` | The server could not start (port in use, …) or failed |
| `2` | Configuration error; the message starts with `error:` on stderr |
| `130` | A second signal arrived during the shutdown |

## Shutdown

On SIGINT or SIGTERM the server stops accepting, cancels the open connections, waits at most
five seconds for them, logs `shutdown complete` and exits with `0`. A second signal exits
immediately with `130`.

## Logs

Logs go to stdout. The `sa` password never appears in them, at any level: the effective
configuration is logged at `debug` with the password redacted, and the startup line only says
whether authentication is `sa` or `no-auth`.

## Ports of the tests

Every server started by an integration test listens on a **fixed** port: the base
`VAUBAN_TEST_PORT` (`1433` when unset or unreadable) plus an offset that belongs to one
test. Asking the system for an ephemeral port, closing the listener and handing the number to
the server would leave a window in which another test takes it; a fixed offset per test has
no such window and still allows `cargo test` to run the tests in parallel threads.

The offsets are disjoint across the three test files; `0..=9` is not used by this crate:

| File | Offsets | Ports with the default base |
|---|---|---|
| `tests/tls.rs` | `10..=19` | `1443`–`1452` |
| `tests/cli.rs` | `20..=24` | `1453`–`1457` |
| `tests/registry.rs` | `30..=39` | `1463`–`1472` |

A test that starts two servers one after the other reuses its own offset; the first server is
stopped and waited for before the second starts.

Set `VAUBAN_TEST_PORT` to move the whole range when a SQL Server already listens on
`1433`, or when two checkouts are tested at the same time:

```bash
VAUBAN_TEST_PORT=14330 cargo test -p vauban-cli
```
