//! The built-in functions themselves, one submodule per family, and the single point that
//! registers them all.
//!
//! [`register_builtins`] is called once at start-up, before any query is bound; it is
//! idempotent so that a test or a second server instance in the same process can call it
//! again without hitting the duplicate-name panic of [`crate::register`]. Each submodule
//! exposes one `register_all` and owns its functions; `args` holds what they share, the
//! argument checks [`check_call`] and `invalid_argument_type`.
//!
//! Each family lives in its own submodule. `mod` declarations are sorted by `rustfmt`; the
//! order that matters, the registration order, is the one written in [`register_builtins`].

use std::sync::Once;

mod aggregates;
mod args;
mod datetime_calc;
mod datetime_clock;
mod math;
mod nulls;
mod objects;
mod server_info;
mod strings_codes;
mod strings_core;
mod strings_search;
mod system;

pub use args::check_call;
// The `binder` refuses an unknown `datepart` keyword itself, because SQL Server refuses
// it while compiling the batch and not while producing a row, so the keyword table is
// exported.
pub use datetime_clock::{DatePart, parse_datepart};

/// Registers every built-in function of T-SQL in the global registry.
///
/// Idempotent: the work happens on the first call, later calls do nothing. Registering a
/// name twice panics ([`crate::register`]), which is why this is the only place built-ins
/// are registered.
///
/// It does **not** register the functions of `vauban-compat` (`@@VERSION`,
/// `SERVERPROPERTY`): that crate registers its own, and a second registration of the same
/// name would panic.
pub fn register_builtins() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        nulls::register_all();
        strings_core::register_all();
        strings_search::register_all();
        strings_codes::register_all();
        system::register_all();
        objects::register_all();
        datetime_clock::register_all();
        datetime_calc::register_all();
        math::register_all();
        aggregates::register_all();
        server_info::register_all();
    });
}

#[cfg(test)]
mod tests;
