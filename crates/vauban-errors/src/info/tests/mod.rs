use super::*;

#[test]
fn info_message_is_send_sync_static() {
    fn assert<T: Send + Sync + 'static>() {}
    assert::<InfoMessage>();
}

#[test]
fn info_message_supports_equality_and_clone() {
    let msg = InfoMessage {
        number: 0,
        severity: 0,
        state: 1,
        message: "hello".to_string(),
        line: 1,
        procedure: None,
    };
    assert_eq!(msg.clone(), msg);
}

#[test]
fn info_null_eliminated_is_8153() {
    let msg = InfoMessage::null_eliminated();
    assert_eq!(msg.number, 8153);
    assert_eq!(msg.severity, 10);
    assert_eq!(msg.state, 1);
    assert_eq!(msg.line, 0);
    assert_eq!(
        msg.message,
        "Warning: an aggregate or SET operation ignored a NULL value."
    );
}

#[test]
fn info_null_eliminated_stays_informational() {
    assert!(InfoMessage::null_eliminated().severity <= 10);
}
