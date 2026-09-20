//! SQL authentication contract: the `Authenticator` trait, the `Principal` it yields and
//! the two implementations (`SaPasswordAuthenticator`, `NoAuth`).

use vauban_errors::{SqlError, SqlResult};

/// Checks a SQL login (user name and password from the LOGIN7, [MS-TDS] 2.2.6.4) and
/// returns the [`Principal`] the session runs as.
///
/// Implementations are shared by every connection through `ServerConfig::authenticator`,
/// hence `Send + Sync`. A refused login is a `SqlError` (18456); the caller turns it into
/// the error token and closes the connection.
///
/// The `state` of a refused login is the **server-side** detail (5: the login does not
/// exist, 8: the password is wrong). The server logs it and sends the client the generic
/// state (see `server.rs`): the detail does not reach the wire.
pub trait Authenticator: Send + Sync {
    /// Authenticates `user` with `password`.
    fn authenticate(&self, user: &str, password: &str) -> SqlResult<Principal>;
}

/// The identity a session runs as, once the login is accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// Login name as accepted (`sa` for the built-in administrator).
    pub login: String,
    /// `true` when the login has the `sysadmin` role. `sa` and the `--no-auth` mode both
    /// yield `true`.
    pub is_sysadmin: bool,
}

/// The built-in administrator login of SQL Server.
const SA: &str = "sa";

/// State of error 18456 logged when the login name is unknown.
const STATE_UNKNOWN_LOGIN: u8 = 5;
/// State of error 18456 logged when the password does not match.
const STATE_WRONG_PASSWORD: u8 = 8;

/// The default authenticator: the single login `sa` with the password
/// given at start-up. The login name is compared without regard to case (SQL Server logins
/// follow the server collation, case-insensitive by default), the password byte for byte.
///
/// `Debug` is implemented by hand so that the password never reaches a log line.
#[derive(Clone)]
pub struct SaPasswordAuthenticator(pub String);

impl std::fmt::Debug for SaPasswordAuthenticator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SaPasswordAuthenticator(<redacted>)")
    }
}

impl Authenticator for SaPasswordAuthenticator {
    fn authenticate(&self, user: &str, password: &str) -> SqlResult<Principal> {
        if !user.eq_ignore_ascii_case(SA) {
            return Err(with_state(
                SqlError::login_failed(user),
                STATE_UNKNOWN_LOGIN,
            ));
        }
        if password != self.0 {
            return Err(with_state(
                SqlError::login_failed(user),
                STATE_WRONG_PASSWORD,
            ));
        }
        Ok(Principal {
            login: SA.to_owned(),
            is_sysadmin: true,
        })
    }
}

/// The `--no-auth` mode: a login is accepted as given, empty password included, with the
/// `sysadmin` role. Meant for development.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoAuth;

impl Authenticator for NoAuth {
    fn authenticate(&self, user: &str, _password: &str) -> SqlResult<Principal> {
        Ok(Principal {
            login: user.to_owned(),
            is_sysadmin: true,
        })
    }
}

/// `err` with its state replaced by the server-side detail.
fn with_state(err: SqlError, state: u8) -> SqlError {
    SqlError { state, ..err }
}

#[cfg(test)]
mod tests;
