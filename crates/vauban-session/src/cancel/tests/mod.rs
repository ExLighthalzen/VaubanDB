use super::*;

#[test]
fn clones_share_the_flag() {
    let handle = CancelHandle::new();
    let clone = handle.clone();
    assert!(!handle.is_cancelled());
    clone.cancel();
    assert!(handle.is_cancelled());
    assert!(clone.is_cancelled());
}

#[test]
fn check_fails_once_cancelled_and_passes_again_after_reset() {
    let handle = CancelHandle::new();
    assert!(handle.check().is_ok());
    handle.cancel();
    let err = handle
        .check()
        .expect_err("a cancelled handle fails its check");
    assert_eq!(err.number, 50000);
    assert!(err.message.contains(CANCELLED), "{}", err.message);
    handle.reset();
    assert!(!handle.is_cancelled());
    assert!(handle.check().is_ok());
}

#[test]
fn reset_reaches_every_clone() {
    let handle = CancelHandle::new();
    let clone = handle.clone();
    clone.cancel();
    handle.reset();
    assert!(!clone.is_cancelled());
}

#[test]
fn token_shares_the_flag() {
    let handle = CancelHandle::new();
    let token = handle.token();
    assert!(!token.is_cancelled());
    handle.cancel();
    assert!(token.is_cancelled());
    token.clone().cancel();
    assert!(handle.is_cancelled());
}
