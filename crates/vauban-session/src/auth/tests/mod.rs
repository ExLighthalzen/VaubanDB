use super::*;

const SECRET: &str = "Secret1!";

fn sa() -> SaPasswordAuthenticator {
    SaPasswordAuthenticator(SECRET.into())
}

#[test]
fn sa_with_the_right_password_is_sysadmin() {
    let principal = sa().authenticate("sa", SECRET).unwrap();
    assert_eq!(
        principal,
        Principal {
            login: "sa".into(),
            is_sysadmin: true
        }
    );
}

#[test]
fn login_name_is_case_insensitive() {
    let principal = sa().authenticate("SA", SECRET).unwrap();
    assert_eq!(principal.login, "sa");
    assert!(principal.is_sysadmin);
}

#[test]
fn password_is_case_sensitive_and_refused_with_18456_state_8() {
    let err = sa().authenticate("sa", "secret1!").unwrap_err();
    assert_eq!(err.number, 18456);
    assert_eq!(err.severity, 14);
    assert_eq!(err.state, STATE_WRONG_PASSWORD);
    assert_eq!(err.message, SqlError::login_failed("sa").message);
}

#[test]
fn unknown_login_is_refused_with_18456_state_5() {
    let err = sa().authenticate("bob", SECRET).unwrap_err();
    assert_eq!(err.number, 18456);
    assert_eq!(err.severity, 14);
    assert_eq!(err.state, STATE_UNKNOWN_LOGIN);
    assert_eq!(err.message, SqlError::login_failed("bob").message);
}

#[test]
fn empty_password_is_refused_unless_configured() {
    assert!(sa().authenticate("sa", "").is_err());
    let empty = SaPasswordAuthenticator(String::new());
    assert!(empty.authenticate("sa", "").is_ok());
}

#[test]
fn no_auth_accepts_anything() {
    assert_eq!(
        NoAuth.authenticate("anything", "").unwrap(),
        Principal {
            login: "anything".into(),
            is_sysadmin: true
        }
    );
    assert_eq!(NoAuth.authenticate("bob", "x").unwrap().login, "bob");
}

#[test]
fn debug_of_the_authenticator_hides_the_password() {
    let rendered = format!("{:?}", sa());
    assert!(!rendered.contains(SECRET), "{rendered}");
}
