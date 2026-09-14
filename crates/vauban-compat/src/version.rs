//! Version of the SQL Server that VaubanDB announces, and the constants derived from it.
//!
//! The two source constants live in `session` (`compat` depends on `session`, so they
//! cannot be defined here) and are re-exported under the names of the module README. The
//! derived constants describe the same build: what `SERVERPROPERTY` reports must agree
//! with the LOGINACK token and with `@@VERSION`.

pub use vauban_session::{EDITION, PRODUCT_VERSION, VERSION_BANNER};

/// `SERVERPROPERTY('ProductLevel')`: the level of the base release, before any update.
/// Kept as the numeric surface of [`PRODUCT_VERSION`] (`16.0.4275.2`).
pub(crate) const PRODUCT_LEVEL: &str = "RTM";

/// `SERVERPROPERTY('ProductUpdateLevel')`: the cumulative update of the announced build.
pub(crate) const PRODUCT_UPDATE_LEVEL: &str = "CU26";

/// `SERVERPROPERTY('EngineEdition')`: 3, the value of the Enterprise engine family, which
/// SQL Server reports for its free non-production edition as well.
pub(crate) const ENGINE_EDITION: i32 = 3;

/// The four components of [`PRODUCT_VERSION`], computed at compile time so that a
/// malformed constant fails the build rather than a query.
const VERSION_PARTS: (u16, u16, u16, u16) = parse_version(PRODUCT_VERSION);

/// Splits [`PRODUCT_VERSION`] (`major.minor.build.revision`) into its four numbers.
pub(crate) fn version_parts() -> (u16, u16, u16, u16) {
    VERSION_PARTS
}

/// Parses `major.minor.build.revision` at compile time. The assertions only fire during
/// constant evaluation: a bad constant is a build error, never a runtime panic.
const fn parse_version(s: &str) -> (u16, u16, u16, u16) {
    let bytes = s.as_bytes();
    let mut parts = [0u32; 4];
    let mut part = 0;
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte == b'.' {
            part += 1;
            assert!(part < 4, "PRODUCT_VERSION has more than four parts");
        } else {
            assert!(
                byte.is_ascii_digit(),
                "PRODUCT_VERSION is not digits and dots"
            );
            parts[part] = parts[part] * 10 + (byte - b'0') as u32;
            assert!(parts[part] <= u16::MAX as u32, "a version part exceeds u16");
        }
        i += 1;
    }
    assert!(part == 3, "PRODUCT_VERSION has fewer than four parts");
    (
        parts[0] as u16,
        parts[1] as u16,
        parts[2] as u16,
        parts[3] as u16,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parts_splits_product_version() {
        let (major, minor, build, revision) = version_parts();
        assert_eq!((major, minor), (16, 0));
        assert_eq!(
            format!("{major}.{minor}.{build}.{revision}"),
            PRODUCT_VERSION
        );
    }

    #[test]
    fn parse_version_handles_other_builds() {
        assert_eq!(parse_version("15.0.2000.5"), (15, 0, 2000, 5));
        assert_eq!(parse_version("0.0.0.0"), (0, 0, 0, 0));
    }

    #[test]
    fn banner_agrees_with_the_constants() {
        assert!(VERSION_BANNER.starts_with("VaubanDB"));
        assert!(VERSION_BANNER.contains(PRODUCT_VERSION));
        assert!(VERSION_BANNER.contains(EDITION));
        assert!(!VERSION_BANNER.contains("Microsoft SQL Server"));
    }
}
