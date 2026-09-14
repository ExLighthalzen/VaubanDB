//! Where the lexer tests live, and why they are not here.
//!
//! This file holds no test, because it cannot hold one: an integration test is a separate
//! crate, and everything the lexer exposes (`lexer::tokenize`, `token::Token`,
//! `token::TokenKind`, `keyword::Keyword`) is `pub(crate)`.
//!
//! The lexer tests therefore sit next to the code they exercise, as unit tests, in
//! `src/lexer.rs` (`lex_*` and `unclosed_quotation_mark_comes_from_the_catalog`) and in
//! `src/keyword.rs` (`keyword_*`). The lexical behaviour a client can see -- error 105 on
//! `SELECT 'abc;`, error 102 on an unknown character -- is asserted through `parse_batch`
//! in `tests/syntax_errors.rs`.
