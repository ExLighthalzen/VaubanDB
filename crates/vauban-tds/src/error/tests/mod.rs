use super::*;

/// How many variants [`TdsError`] declares, recounted on the list above.
const VARIANTS: usize = 13;

/// How many of them [`TdsError::is_encoding_failure`] puts on the codec side,
/// recounted on its first arm.
const ENCODING_VARIANTS: usize = 4;

/// Rank of the variant of `err` in the declaration order of [`TdsError`].
///
/// Written out rather than derived, so that a variant added to the enum stops the build
/// here and has to be given a sample below.
fn rank(err: &TdsError) -> usize {
    match err {
        TdsError::Io(_) => 0,
        TdsError::Tls(_) => 1,
        TdsError::Malformed(_) => 2,
        TdsError::MessageTooLarge(_) => 3,
        TdsError::UnexpectedPacketType(_) => 4,
        TdsError::Unsupported(_) => 5,
        TdsError::NotImplemented(_) => 6,
        TdsError::EncryptionRefused => 7,
        TdsError::ConnectionClosed => 8,
        TdsError::ValueTypeMismatch { .. } => 9,
        TdsError::NullInNotNullable => 10,
        TdsError::RowWithoutMetadata => 11,
        TdsError::ColumnCountMismatch { .. } => 12,
    }
}

/// One value per variant, each with the side the classification owes it.
fn samples() -> Vec<(TdsError, bool)> {
    vec![
        (TdsError::Io(std::io::Error::other("socket")), false),
        (
            TdsError::Tls(rustls::Error::General("handshake".into())),
            false,
        ),
        (TdsError::Malformed("PRELOGIN option past the end"), false),
        (TdsError::MessageTooLarge(1 << 20), false),
        (TdsError::UnexpectedPacketType(0x01), false),
        (TdsError::Unsupported("MARS"), false),
        (TdsError::NotImplemented("BULK_LOAD"), false),
        (TdsError::EncryptionRefused, false),
        (TdsError::ConnectionClosed, false),
        (TdsError::ValueTypeMismatch { expected: "int" }, true),
        (TdsError::NullInNotNullable, true),
        (TdsError::RowWithoutMetadata, true),
        (
            TdsError::ColumnCountMismatch {
                expected: 2,
                got: 1,
            },
            true,
        ),
    ]
}

/// The samples cover the enum once each, and each takes the side it is given: the two
/// counts below hold the classification still.
#[test]
fn each_variant_has_one_sample_and_its_side() {
    let mut sides: [Option<bool>; VARIANTS] = [None; VARIANTS];
    for (err, encoding) in samples() {
        assert_eq!(err.is_encoding_failure(), encoding, "{err:?}");
        let slot = &mut sides[rank(&err)];
        assert!(slot.is_none(), "two samples for the variant of {err:?}");
        *slot = Some(encoding);
    }
    assert_eq!(sides.iter().flatten().count(), VARIANTS, "{sides:?}");
    assert_eq!(
        sides.iter().flatten().filter(|side| **side).count(),
        ENCODING_VARIANTS,
        "{sides:?}"
    );
}
