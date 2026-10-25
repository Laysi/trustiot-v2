//! Steps 1 to 3 of the check: the size, the characters, and every token, before anything is
//! parsed. The rules close the places where sqlparser and ClickHouse tokenize the same text
//! differently (backslash escapes, hex and binary numbers, quoted identifiers, Unicode spaces),
//! so both read the same tokens. How they group operators is the walk's part
//! ([`super::check`]).

use contract::api::telemetry_query::{MAX_SQL_BYTES, SqlRefusal};
use sqlparser::dialect::ClickHouseDialect;
use sqlparser::keywords::Keyword;
use sqlparser::tokenizer::{Token, TokenWithSpan, Tokenizer, Whitespace};

/// The most tokens other than spaces and comments a query may have.
pub const MAX_TOKENS: usize = 4_096;

/// The longest identifier, in bytes.
pub const MAX_IDENTIFIER_BYTES: usize = 128;

/// The tokens of `sql`, after the size, character and token rules.
///
/// # Errors
///
/// The refusal for the first rule `sql` breaks.
pub fn tokens(sql: &str) -> Result<Vec<TokenWithSpan>, SqlRefusal> {
    if sql.len() > MAX_SQL_BYTES as usize {
        return Err(SqlRefusal::TooLong {
            max_bytes: MAX_SQL_BYTES,
        });
    }
    characters(sql)?;
    let tokens = Tokenizer::new(&ClickHouseDialect {}, sql)
        .tokenize_with_location()
        .map_err(|error| SqlRefusal::Syntax {
            line: u32::try_from(error.location.line).ok(),
            column: u32::try_from(error.location.column).ok(),
        })?;
    let significant: Vec<&TokenWithSpan> = tokens
        .iter()
        .filter(|token| !matches!(token.token, Token::Whitespace(_) | Token::EOF))
        .collect();
    if significant.len() > MAX_TOKENS {
        return Err(SqlRefusal::TooComplex);
    }
    for (index, token) in tokens.iter().enumerate() {
        allowed(token, tokens.get(index + 1))?;
    }
    match significant.first().map(|token| &token.token) {
        Some(Token::Word(word))
            if word.quote_style.is_none()
                && matches!(word.keyword, Keyword::SELECT | Keyword::WITH) => {}
        Some(Token::LParen) => {}
        _ => return Err(SqlRefusal::NotASelect),
    }
    // A `;` may only end the text: anything after one is a second statement, refused before
    // the parser could read (or fail to read) it.
    if let Some(semicolon) = significant
        .iter()
        .position(|token| token.token == Token::SemiColon)
        && semicolon + 1 < significant.len()
    {
        return Err(SqlRefusal::MultipleStatements);
    }
    Ok(tokens)
}

/// Step 2: no backslash, no control character but tab, line feed and carriage return, no
/// space other than ASCII's, and no invisible formatting character, anywhere (strings and
/// comments included).
fn characters(sql: &str) -> Result<(), SqlRefusal> {
    let (mut line, mut column) = (1_u32, 0_u32);
    for character in sql.chars() {
        if character == '\n' {
            line = line.saturating_add(1);
            column = 0;
            continue;
        }
        column = column.saturating_add(1);
        let refused = character == '\\'
            || (character.is_control() && !matches!(character, '\t' | '\r'))
            || (!character.is_ascii() && character.is_whitespace())
            || is_format_character(character);
        if refused {
            return Err(SqlRefusal::CharacterNotAllowed { line, column });
        }
    }
    Ok(())
}

/// Unicode's invisible formatting characters (general category Cf) and the other characters
/// that render as nothing: zero-width spaces and joiners, bidirectional controls, the soft
/// hyphen, the byte order mark, invisible operators, tag characters and variation selectors.
fn is_format_character(character: char) -> bool {
    matches!(
        u32::from(character),
        0x00AD
            | 0x034F
            | 0x061C
            | 0x115F..=0x1160
            | 0x17B4..=0x17B5
            | 0x180B..=0x180F
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x206F
            | 0x3164
            | 0xFE00..=0xFE0F
            | 0xFEFF
            | 0xFFA0
            | 0xFFF0..=0xFFFB
            | 0x1D173..=0x1D17A
            | 0xE0000..=0xE0FFF
    )
}

/// Step 3 for one token; `next` is the token right after it, spaces included.
fn allowed(token: &TokenWithSpan, next: Option<&TokenWithSpan>) -> Result<(), SqlRefusal> {
    let refused = || SqlRefusal::LiteralNotAllowed {
        line: u32::try_from(token.span.start.line).unwrap_or(u32::MAX),
        column: u32::try_from(token.span.start.column).unwrap_or(u32::MAX),
    };
    match &token.token {
        Token::Word(word) => {
            // A word right before a string is a prefixed string to ClickHouse (`x'41'`,
            // `b'01'`) or to sqlparser (`e'…'`, `N'…'`).
            let prefixes =
                next.is_some_and(|next| matches!(next.token, Token::SingleQuotedString(_)));
            if word.quote_style.is_some() || !is_identifier(&word.value) || prefixes {
                return Err(refused());
            }
        }
        Token::Number(text, long) => {
            // A number must not run into a word or another number: `0b101` and `1abc` are
            // one token to ClickHouse and two to sqlparser.
            let touches = next.is_some_and(|next| {
                matches!(
                    next.token,
                    Token::Word(_) | Token::Number(..) | Token::Period
                )
            });
            if *long || !is_decimal(text) || touches {
                return Err(refused());
            }
        }
        Token::Whitespace(whitespace) => match whitespace {
            Whitespace::Space
            | Whitespace::Newline
            | Whitespace::Tab
            | Whitespace::MultiLineComment(_) => {}
            Whitespace::SingleLineComment { prefix, .. } => {
                if prefix != "--" {
                    return Err(refused());
                }
            }
        },
        Token::SingleQuotedString(_)
        | Token::Comma
        | Token::DoubleEq
        | Token::Eq
        | Token::Neq
        | Token::Lt
        | Token::Gt
        | Token::LtEq
        | Token::GtEq
        | Token::Plus
        | Token::Minus
        | Token::Mul
        | Token::Div
        | Token::Mod
        | Token::StringConcat
        | Token::LParen
        | Token::RParen
        | Token::Period
        | Token::LBracket
        | Token::RBracket
        | Token::Arrow
        | Token::DoubleColon
        | Token::SemiColon
        | Token::EOF => {}
        _ => return Err(refused()),
    }
    Ok(())
}

/// `[A-Za-z_][A-Za-z0-9_]*`, at most [`MAX_IDENTIFIER_BYTES`].
pub fn is_identifier(text: &str) -> bool {
    let mut bytes = text.bytes();
    let starts = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_');
    starts
        && text.len() <= MAX_IDENTIFIER_BYTES
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// `[0-9]+(\.[0-9]+)?([eE][+-]?[0-9]+)?`.
pub fn is_decimal(text: &str) -> bool {
    let (mantissa, exponent) = match text.find(['e', 'E']) {
        Some(at) => (text.get(..at), text.get(at + 1..)),
        None => (Some(text), None),
    };
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    let mantissa_ok = mantissa.is_some_and(|mantissa| match mantissa.split_once('.') {
        Some((whole, fraction)) => digits(whole) && digits(fraction),
        None => digits(mantissa),
    });
    // `1e` leaves an empty exponent, which is no digits.
    let exponent_ok = exponent
        .is_none_or(|exponent| digits(exponent.strip_prefix(['+', '-']).unwrap_or(exponent)));
    mantissa_ok && exponent_ok
}

/// `[0-9]+`.
pub fn is_integer(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refusal(sql: &str) -> SqlRefusal {
        tokens(sql).expect_err(sql)
    }

    #[test]
    fn plain_selects_pass() {
        for sql in [
            "SELECT 1",
            "select 1;",
            "WITH a AS (SELECT 1) SELECT * FROM a",
            "(SELECT 1)",
            "SELECT 'it''s', 1.5e-3, 2E+7, -- a comment\n x /* another\n one */ FROM t",
            "SELECT arrayMap(x -> x * 2, [1, 2])[1], CAST(x AS Float64), x::String",
            "SELECT '設備一號', '🙂'",
            "SELECT\tx\r\nFROM t",
        ] {
            assert!(tokens(sql).is_ok(), "{sql}");
        }
    }

    // The size limits, the identifier length limit and the statements other than one SELECT are
    // tested in tests/sql_check.rs: their inputs are SQL built in the test or statements that
    // sql-check keeps out of `src` (tools/sql-check, decision 29).

    #[test]
    fn characters_are_refused_where_they_are() {
        for (sql, line, column) in [
            ("SELECT 'a\\', 1", 1, 10),
            ("SELECT 1\n-- \\ in a comment", 2, 4),
            ("SELECT '\u{0}'", 1, 9),
            ("SELECT 1\u{0B}", 1, 9),
            ("SELECT\u{A0}1", 1, 7),
            ("SELECT\u{3000}1", 1, 7),
            ("SELECT '\u{202E}'", 1, 9),
            ("SELECT 1 -- \u{200B}", 1, 13),
            ("\u{FEFF}SELECT 1", 1, 1),
            ("SELECT '\u{E0041}'", 1, 9),
        ] {
            assert_eq!(
                refusal(sql),
                SqlRefusal::CharacterNotAllowed { line, column },
                "{sql:?}"
            );
        }
    }

    #[test]
    fn token_forms_are_limited() {
        for (sql, column) in [
            ("SELECT \"x\" FROM t", 8),
            ("SELECT `x` FROM t", 8),
            ("SELECT 0x1F", 8),
            ("SELECT 0b101", 8),
            ("SELECT 1abc", 8),
            ("SELECT .5", 8),
            ("SELECT 1.", 8),
            ("SELECT 1_000", 8),
            ("SELECT 1e", 8),
            ("SELECT x'41'", 8),
            ("SELECT e'x'", 8),
            ("SELECT LIKE'x'", 8),
            ("SELECT N'x'", 8),
            ("SELECT {p:String}", 8),
            ("SELECT ?", 8),
            ("SELECT $1", 8),
            ("SELECT @x", 8),
            ("SELECT 1 # comment", 10),
            ("SELECT a ? b : c", 10),
            ("SELECT a & b", 10),
            ("SELECT a | b", 10),
            ("SELECT a ^ b", 10),
            ("SELECT ~a", 8),
            ("SELECT !a", 8),
            ("SELECT a <=> b", 10),
            ("SELECT a => b", 10),
            ("SELECT a := b", 10),
            ("SELECT a ->> b", 10),
            ("SELECT x_ü", 10),
            ("SELECT (1, 2).1", 14),
        ] {
            assert_eq!(
                refusal(sql),
                SqlRefusal::LiteralNotAllowed { line: 1, column },
                "{sql}"
            );
        }
    }

    #[test]
    fn numbers_and_identifiers_follow_their_patterns() {
        for text in ["0", "12", "1.5", "1e5", "1.5E-3", "2e+7"] {
            assert!(is_decimal(text), "{text}");
        }
        for text in [
            "", ".5", "1.", "1e", "1e+", "0x1", "1_0", "1.2.3", "e5", "1ee5",
        ] {
            assert!(!is_decimal(text), "{text}");
        }
        assert!(is_integer("007"));
        assert!(!is_integer("1.0"));
        assert!(is_identifier("_a1"));
        assert!(!is_identifier("1a"));
        assert!(!is_identifier(""));
    }
}
