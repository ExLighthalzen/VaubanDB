//! Unit tests of the crate root: the build target string and the server-name
//! fallback.

use super::*;

#[test]
fn version_target_is_arch_and_os() {
    assert_eq!(
        build_target(),
        format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
    );
}

#[test]
fn server_name_falls_back() {
    // Whatever the environment, the name is never empty.
    assert!(!server_name().is_empty());
}
