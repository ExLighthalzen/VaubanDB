//! Unit tests of `Config`: CLI/env/file precedence and defaults, identity overrides
//! (`program_name`, `version_banner`, `edition`) and their length rules, TOML parsing
//! and diagnostics, and the merge helper [`Config::assemble`] the other two test
//! modules of this crate also call.

use super::*;

fn args(list: &[&str]) -> ServeArgs {
    ServeArgs::try_parse_from(std::iter::once("serve").chain(list.iter().copied()))
        .expect("valid arguments")
}

fn file(text: &str) -> Option<ConfigLayer> {
    Some(ConfigLayer::from_toml(text).expect("valid TOML"))
}

#[test]
fn port_command_line_beats_file() {
    let cfg = Config::assemble(&args(&["--port", "1600"]), None, file("port = 1500"));
    assert_eq!(cfg.port, 1600);
}

#[test]
fn port_file_beats_default() {
    let cfg = Config::assemble(&args(&[]), None, file("port = 1500"));
    assert_eq!(cfg.port, 1500);
}

#[test]
fn port_default_is_1433() {
    let cfg = Config::assemble(&args(&[]), None, None);
    assert_eq!(cfg.port, 1433);
}

#[test]
fn password_from_environment() {
    let cfg = Config::assemble(&args(&[]), Some("x".into()), None);
    assert_eq!(cfg.sa_password.as_deref(), Some("x"));
    assert_eq!(cfg.sa_password_source, Some(PasswordSource::Environment));
}

#[test]
fn password_command_line_beats_environment() {
    let cfg = Config::assemble(&args(&["--sa-password", "cli"]), Some("x".into()), None);
    assert_eq!(cfg.sa_password.as_deref(), Some("cli"));
    assert_eq!(cfg.sa_password_source, Some(PasswordSource::CommandLine));
}

#[test]
fn password_environment_beats_file() {
    let cfg = Config::assemble(&args(&[]), Some("x".into()), file("sa_password = \"f\""));
    assert_eq!(cfg.sa_password.as_deref(), Some("x"));
}

#[test]
fn password_from_file() {
    let cfg = Config::assemble(&args(&[]), None, file("sa_password = \"f\""));
    assert_eq!(cfg.sa_password.as_deref(), Some("f"));
    assert_eq!(cfg.sa_password_source, Some(PasswordSource::File));
}

#[test]
fn flags_from_file_are_honoured_when_absent_on_command_line() {
    let cfg = Config::assemble(&args(&[]), None, file("in_memory = true\nno_auth = true"));
    assert!(cfg.in_memory);
    assert!(cfg.no_auth);
}

#[test]
fn defaults() {
    let cfg = Config::assemble(&args(&[]), None, None);
    assert_eq!(cfg.bind, IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    assert_eq!(cfg.port, 1433);
    assert_eq!(cfg.data, None);
    assert!(!cfg.in_memory);
    assert!(!cfg.no_auth);
    assert_eq!(cfg.encrypt, EncryptPolicy::Optional);
    assert_eq!(cfg.cert, None);
    assert_eq!(cfg.key, None);
    assert_eq!(cfg.log_level, LogLevel::Info);
    assert_eq!(cfg.log_format, LogFormat::Text);
}

#[test]
fn every_key_of_the_file_is_read() {
    let text = r#"
        bind = "127.0.0.1"
        port = 1500
        data = "/var/lib/vauban"
        in_memory = true
        sa_password = "f"
        no_auth = false
        encrypt = "required"
        cert = "server.pem"
        key = "server.key"
        log_level = "debug"
        log_format = "json"
    "#;
    let cfg = Config::assemble(&args(&[]), None, file(text));
    assert_eq!(cfg.bind, "127.0.0.1".parse::<IpAddr>().unwrap());
    assert_eq!(cfg.port, 1500);
    assert_eq!(cfg.data.as_deref(), Some(Path::new("/var/lib/vauban")));
    assert!(cfg.in_memory);
    assert_eq!(cfg.sa_password.as_deref(), Some("f"));
    assert!(!cfg.no_auth);
    assert_eq!(cfg.encrypt, EncryptPolicy::Required);
    assert_eq!(cfg.cert.as_deref(), Some(Path::new("server.pem")));
    assert_eq!(cfg.key.as_deref(), Some(Path::new("server.key")));
    assert_eq!(cfg.log_level, LogLevel::Debug);
    assert_eq!(cfg.log_format, LogFormat::Json);
}

#[test]
fn unknown_key_in_file_is_refused() {
    let Err(err) = ConfigLayer::from_toml("prot = 1500") else {
        panic!("unknown key accepted");
    };
    assert!(matches!(err, ConfigError::File(_)), "{err:?}");
    assert!(err.to_string().contains("prot"), "{err}");
}

#[test]
fn encrypt_maps_to_policy() {
    assert_eq!(EncryptPolicy::from(Encrypt::Off), EncryptPolicy::Off);
    assert_eq!(
        EncryptPolicy::from(Encrypt::Optional),
        EncryptPolicy::Optional
    );
    assert_eq!(
        EncryptPolicy::from(Encrypt::Required),
        EncryptPolicy::Required
    );
}

#[test]
fn debug_redacts_the_password() {
    let cfg = Config::assemble(&args(&["--sa-password", "Secret1!"]), None, None);
    let debug = format!("{cfg:?}");
    assert!(debug.contains("<redacted>"), "{debug}");
    assert!(!debug.contains("Secret1!"), "{debug}");
}

#[test]
fn debug_without_password() {
    let cfg = Config::assemble(&args(&["--no-auth"]), None, None);
    assert!(format!("{cfg:?}").contains("sa_password: None"));
}

#[test]
fn validate_requires_a_password_or_no_auth() {
    let cfg = Config::assemble(&args(&["--in-memory"]), None, None);
    assert_eq!(cfg.validate(), Err(ConfigError::MissingPassword));
    let cfg = Config::assemble(&args(&["--in-memory", "--no-auth"]), None, None);
    assert_eq!(cfg.validate(), Ok(()));
    let cfg = Config::assemble(&args(&["--in-memory"]), Some("x".into()), None);
    assert_eq!(cfg.validate(), Ok(()));
}

#[test]
fn validate_requires_cert_and_key_together() {
    let cfg = Config::assemble(
        &args(&["--in-memory", "--no-auth", "--cert", "c"]),
        None,
        None,
    );
    assert_eq!(cfg.validate(), Err(ConfigError::CertWithoutKey));
    let cfg = Config::assemble(
        &args(&["--in-memory", "--no-auth", "--key", "k"]),
        None,
        None,
    );
    assert_eq!(cfg.validate(), Err(ConfigError::CertWithoutKey));
    let cfg = Config::assemble(
        &args(&["--in-memory", "--no-auth", "--cert", "c", "--key", "k"]),
        None,
        None,
    );
    assert_eq!(cfg.validate(), Ok(()));
}

#[test]
fn validate_requires_exactly_one_of_in_memory_and_data() {
    let cfg = Config::assemble(&args(&["--no-auth"]), None, None);
    assert_eq!(cfg.validate(), Err(ConfigError::StorageUnset));
    let cfg = Config::assemble(
        &args(&["--no-auth", "--in-memory", "--data", "d"]),
        None,
        None,
    );
    assert_eq!(cfg.validate(), Err(ConfigError::StorageBoth));
    let cfg = Config::assemble(&args(&["--no-auth", "--in-memory"]), None, None);
    assert_eq!(cfg.validate(), Ok(()));
    let cfg = Config::assemble(&args(&["--no-auth", "--data", "d"]), None, None);
    assert_eq!(cfg.validate(), Ok(()));
}

#[test]
fn warnings_on_command_line_password_and_ignored_password() {
    let cfg = Config::assemble(&args(&["--sa-password", "Secret1!"]), None, None);
    let warnings = cfg.warnings();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("visible in the process list"));
    assert!(!warnings[0].contains("Secret1!"));

    let cfg = Config::assemble(&args(&["--no-auth"]), Some("x".into()), None);
    assert_eq!(cfg.warnings(), vec!["--no-auth ignores the sa password"]);

    let cfg = Config::assemble(&args(&["--no-auth"]), None, None);
    assert!(cfg.warnings().is_empty());
}

#[test]
fn auth_mode_never_names_the_password() {
    let cfg = Config::assemble(&args(&["--sa-password", "Secret1!"]), None, None);
    assert_eq!(cfg.auth_mode(), "sa");
    let cfg = Config::assemble(&args(&["--no-auth"]), None, None);
    assert_eq!(cfg.auth_mode(), "no-auth");
}

#[test]
fn missing_file_is_an_error() {
    let Err(err) = ConfigLayer::from_file(Path::new("/nonexistent/vauban.toml")) else {
        panic!("missing file accepted");
    };
    assert!(matches!(err, ConfigError::File(_)), "{err:?}");
    assert!(
        err.to_string().contains("/nonexistent/vauban.toml"),
        "{err}"
    );
}

#[test]
fn identity_environment_beats_file() {
    let env = ConfigLayer {
        program_name: Some("FromEnv".into()),
        ..ConfigLayer::default()
    };
    let cfg = Config::assemble_from_layers(
        &args(&[]),
        env,
        file("program_name = \"FromFile\"\nedition = \"File Edition\""),
    );
    assert_eq!(cfg.program_name.as_deref(), Some("FromEnv"));
    assert_eq!(cfg.edition.as_deref(), Some("File Edition"));
    assert_eq!(
        cfg.warnings(),
        vec!["product identity overridden by the operator"]
    );
}

#[test]
fn edition_without_banner_rewrites_the_default_last_line() {
    let env = ConfigLayer {
        edition: Some("Custom Edition".into()),
        ..ConfigLayer::default()
    };
    let cfg = Config::assemble_from_layers(&args(&[]), env, None);
    let (program_name, banner, edition) = cfg.product_identity();
    assert!(program_name.is_none());
    assert_eq!(edition.as_deref(), Some("Custom Edition"));
    let banner = banner.expect("composed banner");
    assert!(banner.starts_with("VaubanDB"));
    assert!(banner.ends_with("\tCustom Edition"));
    assert!(!banner.contains("Microsoft"));
}

#[test]
fn explicit_banner_is_not_rewritten_by_edition() {
    let env = ConfigLayer {
        version_banner: Some("CustomBanner".into()),
        edition: Some("Custom Edition".into()),
        ..ConfigLayer::default()
    };
    let cfg = Config::assemble_from_layers(&args(&[]), env, None);
    let (_, banner, edition) = cfg.product_identity();
    assert_eq!(banner.as_deref(), Some("CustomBanner"));
    assert_eq!(edition.as_deref(), Some("Custom Edition"));
}

#[test]
fn toml_edition_without_banner_rewrites_the_last_line() {
    let cfg = Config::assemble(&args(&[]), None, file("edition = \"Custom Edition\""));
    let (_, banner, edition) = cfg.product_identity();
    assert_eq!(edition.as_deref(), Some("Custom Edition"));
    let banner = banner.expect("composed banner");
    assert!(banner.ends_with("\tCustom Edition"));
    assert_eq!(
        cfg.warnings(),
        vec!["product identity overridden by the operator"]
    );
}

/// A configuration the server would otherwise start with, `program_name` apart.
fn with_program_name(name: &str) -> Config {
    let env = ConfigLayer {
        program_name: Some(name.to_owned()),
        ..ConfigLayer::default()
    };
    Config::assemble_from_layers(&args(&["--in-memory", "--no-auth"]), env, None)
}

/// The boundary of the LOGINACK `ProgName`: 255 UTF-16 code units start, 256 do not.
/// Without this refusal, a 256-unit name cannot be encoded as a B_VARCHAR and the server
/// would close the first connection without an error token.
#[test]
fn program_name_of_255_units_starts_and_256_is_refused() {
    assert_eq!(with_program_name(&"x".repeat(255)).validate(), Ok(()));
    assert_eq!(
        with_program_name(&"x".repeat(256)).validate(),
        Err(ConfigError::ProgramNameTooLong(256))
    );
}

/// The message names both the length received and the limit.
#[test]
fn program_name_error_names_the_length_and_the_limit() {
    let err = with_program_name(&"x".repeat(256))
        .validate()
        .expect_err("256 units refused");
    assert_eq!(
        err.to_string(),
        "program_name is 256 UTF-16 code units long; LOGINACK carries at most 255"
    );
}

/// The count is in UTF-16 code units. Three vectors that a count in bytes or in
/// characters would get wrong:
///
/// - 255 `é` weigh 510 UTF-8 bytes and 255 units: accepted, so the count is not in bytes.
/// - 254 BMP characters plus one astral character are 255 characters and 256 units:
///   refused, so the count is not in characters.
/// - the same name without its last BMP character is 254 characters and 255 units:
///   accepted, so the astral character is what tips the previous one over.
#[test]
fn program_name_is_counted_in_utf16_code_units() {
    let accented = "\u{e9}".repeat(255);
    assert_eq!(accented.len(), 510);
    assert_eq!(with_program_name(&accented).validate(), Ok(()));

    // U+1F3F0 CASTLE: outside the basic multilingual plane, two UTF-16 code units.
    let astral = '\u{1F3F0}';
    assert_eq!(astral.len_utf16(), 2);
    let over = format!("{}{astral}", "x".repeat(254));
    assert_eq!(over.chars().count(), 255);
    assert_eq!(over.encode_utf16().count(), 256);
    assert_eq!(
        with_program_name(&over).validate(),
        Err(ConfigError::ProgramNameTooLong(256))
    );

    let under = format!("{}{astral}", "x".repeat(253));
    assert_eq!(under.encode_utf16().count(), 255);
    assert_eq!(with_program_name(&under).validate(), Ok(()));
}

/// The two other identity overrides are not checked: `@@VERSION` and the edition travel
/// as `nvarchar` values, not as a B_VARCHAR. Vector: 300 units each, accepted.
#[test]
fn banner_and_edition_of_300_units_are_accepted() {
    let env = ConfigLayer {
        version_banner: Some("b".repeat(300)),
        edition: Some("e".repeat(300)),
        ..ConfigLayer::default()
    };
    let cfg = Config::assemble_from_layers(&args(&["--in-memory", "--no-auth"]), env, None);
    assert_eq!(cfg.validate(), Ok(()));
}

#[test]
fn empty_identity_strings_count_as_unset() {
    let env = ConfigLayer {
        program_name: Some(String::new()),
        version_banner: Some(String::new()),
        edition: Some(String::new()),
        ..ConfigLayer::default()
    };
    let cfg = Config::assemble_from_layers(&args(&[]), env, None);
    assert!(cfg.program_name.is_none());
    assert!(cfg.version_banner.is_none());
    assert!(cfg.edition.is_none());
    assert!(cfg.warnings().is_empty());
    assert_eq!(cfg.product_identity(), (None, None, None));
}

impl Config {
    /// Merges the three sources in precedence order and applies the defaults.
    ///
    /// `env_password` is the value of `VAUBAN_SA_PASSWORD`; `file` is the parsed
    /// configuration file, if any. Identity overrides are not read here: tests pass
    /// them through [`Config::assemble_from_layers`].
    pub(crate) fn assemble(
        args: &ServeArgs,
        env_password: Option<String>,
        file: Option<ConfigLayer>,
    ) -> Self {
        Self::assemble_from_layers(args, ConfigLayer::from_env_password(env_password), file)
    }
}
