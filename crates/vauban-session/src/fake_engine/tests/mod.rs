use super::*;

#[test]
fn delays_out_of_range_or_out_of_shape_are_refused() {
    for delay in [
        "abc",
        "",
        "1",
        "00:00",
        "24:00:00",
        "00:60:00",
        "00:00:60",
        "00:00:00.",
        "00:00:00.1234",
        "00:00:00.abc",
        "-1:00:00",
        "0x:00:00",
        " 00:00:00",
        "00:00:00 ",
    ] {
        assert_eq!(parse_delay(delay), None, "delay {delay:?}");
    }
}

#[test]
fn well_formed_delays_give_their_duration() {
    assert_eq!(parse_delay("00:00:00"), Some(Duration::ZERO));
    assert_eq!(parse_delay("00:00:10"), Some(Duration::from_secs(10)));
    assert_eq!(parse_delay("23:59:59"), Some(Duration::from_secs(86399)));
    assert_eq!(parse_delay("00:00:00.1"), Some(Duration::from_millis(100)));
    assert_eq!(parse_delay("00:00:00.12"), Some(Duration::from_millis(120)));
    assert_eq!(
        parse_delay("00:00:01.023"),
        Some(Duration::from_millis(1023))
    );
}

#[test]
fn only_the_delay_form_of_waitfor_is_served() {
    assert_eq!(waitfor_delay("WAITFOR DELAY '00:00:01'"), Some("00:00:01"));
    assert_eq!(waitfor_delay("waitfor delay 'abc';"), Some("abc"));
    assert_eq!(waitfor_delay("WAITFOR TIME '22:00'"), None);
    assert_eq!(waitfor_delay("WAITFOR DELAY'00:00:01'"), Some("00:00:01"));
    assert_eq!(waitfor_delay("WAITFOR DELAY 00:00:01"), None);
    assert_eq!(waitfor_delay("WAITFORDELAY '00:00:01'"), None);
    assert_eq!(waitfor_delay("WAITFOR DELAYS '00:00:01'"), None);
    assert_eq!(waitfor_delay("SELECT 1"), None);
}

#[test]
fn a_statement_this_file_does_not_know_is_none() {
    // The signal `batch.rs` reads to send the binder's internal error instead: the
    // fake engine answers nothing at all, it does not invent an error 102 any more.
    struct Silent;
    impl ResultSink for Silent {
        fn columns(&mut self, _cols: &[vauban_tds::ColumnMeta]) -> SqlResult<()> {
            unreachable!("nothing is sent for a statement this file does not know")
        }
        fn row(&mut self, _row: &[vauban_types::Value]) -> SqlResult<()> {
            unreachable!("nothing is sent for a statement this file does not know")
        }
        fn done(&mut self, _rowcount: Option<u64>, _more: bool) -> SqlResult<()> {
            unreachable!("nothing is sent for a statement this file does not know")
        }
        fn info(&mut self, _msg: &vauban_errors::InfoMessage) -> SqlResult<()> {
            unreachable!("nothing is sent for a statement this file does not know")
        }
        fn error(&mut self, _err: &SqlError) -> SqlResult<()> {
            unreachable!("nothing is sent for a statement this file does not know")
        }
        fn env_change(&mut self, _change: &vauban_tds::EnvChange) -> SqlResult<()> {
            unreachable!("nothing is sent for a statement this file does not know")
        }
        fn return_value(
            &mut self,
            _name: &str,
            _ty: &vauban_types::TypeInfo,
            _value: &vauban_types::Value,
        ) -> SqlResult<()> {
            unreachable!("nothing is sent for a statement this file does not know")
        }
        fn return_status(&mut self, _status: i32) -> SqlResult<()> {
            unreachable!("nothing is sent for a statement this file does not know")
        }
    }
    let cancel = CancelHandle::new();
    for text in ["SELECT 1", "SELECT @@VERSION", "WAITFOR TIME '22:00'"] {
        assert_eq!(
            answer(&cancel, text, false, &mut Silent).expect("no internal error"),
            None,
            "statement {text:?}"
        );
    }
}

#[test]
fn strip_comments_keeps_words_apart() {
    assert_eq!(strip_comments("a--x\nb"), "a b");
    assert_eq!(strip_comments("a/*x*/b"), "a b");
    assert_eq!(strip_comments("a/* /* x */ */b"), "a b");
    assert_eq!(strip_comments("a -- unterminated"), "a  ");
    assert_eq!(strip_comments("a /* unterminated"), "a ");
}
