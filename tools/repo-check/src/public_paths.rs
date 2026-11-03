//! The ingress's public paths (decision 27) against the services that serve them.
//!
//! The chart's plan table (`trustiot.ingress.publicPathTable` in [`INGRESS_TEMPLATE`]) lists,
//! for each path that skips the cookie check, the only methods the ingress admits there
//! (Laysi/trustiot-v2#859). Those methods are each service's contract, so a service that starts
//! serving another method there would be refused at the ingress with no test failing. These
//! functions read both sides from the files, so `tests/repository.rs` fails when they disagree
//! (Laysi/trustiot-v2#904):
//! - [`parse_public_path_table`] reads the chart's table;
//! - [`openapi_methods`] reads a path's operations from the generated OpenAPI document;
//! - [`route_methods`] reads the method router that axum's `.route(PATH, ...)` registers for a
//!   path in a crate's sources;
//! - [`compared_methods`] reads the method a handler compares against (`method != Method::GET`),
//!   for a service that takes every path and method through a fallback.
//!
//! The source readers are textual. Forms they know they cannot read (a method router built some
//! other way, `.route_service` of the path, a `.nest` that may lead to it, a second method
//! comparison) are errors that ask for the check to be extended. Paths built in ways the text does
//! not show (an alias constant `const P: &str = LOGIN_PATH;`, `Router::route(r, ...)`,
//! `format!`) are not seen: a reviewer of such a change reads the table too.

use std::collections::BTreeSet;

/// The chart template that holds the plan table.
pub const INGRESS_TEMPLATE: &str = "deploy/chart/templates/_ingress.tpl";
/// The OpenAPI document generated from the services' code (`cargo run -p openapi`).
pub const OPENAPI: &str = "api/openapi.json";

const TABLE_DEFINE: &str = "{{- define \"trustiot.ingress.publicPathTable\" -}}";
const TABLE_START: &str = "{{- list";
const TABLE_END: &str = "| toYaml -}}";

/// One row of the plan table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicPathRow {
    /// The key under `ingress.publicPaths`.
    pub key: String,
    /// `app`, `gateway` or `ota`.
    pub host: String,
    /// The backend, a key under `services`.
    pub service: String,
    /// The only methods the ingress admits on the path.
    pub methods: BTreeSet<String>,
    /// The only value the path may take, when the chart fixes it.
    pub fixed: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Expr {
    Str(String),
    Call(String, Vec<Expr>),
}

struct Reader<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
}

impl Reader<'_> {
    fn skip_space(&mut self) {
        while self.chars.next_if(|c| c.is_whitespace()).is_some() {}
    }

    fn word(&mut self) -> String {
        let mut word = String::new();
        while let Some(c) = self
            .chars
            .next_if(|c| c.is_ascii_alphanumeric() || *c == '_')
        {
            word.push(c);
        }
        word
    }

    /// A Go template string: no escapes are used in the table, so one is an error.
    fn string(&mut self) -> Result<String, String> {
        let mut text = String::new();
        loop {
            match self.chars.next() {
                Some('"') => return Ok(text),
                Some('\\') => return Err("escapes in a string are not supported".to_owned()),
                Some(c) => text.push(c),
                None => return Err("a string is not closed".to_owned()),
            }
        }
    }

    fn expr(&mut self) -> Result<Expr, String> {
        self.skip_space();
        match self.chars.next() {
            Some('"') => self.string().map(Expr::Str),
            Some('(') => {
                let name = self.word();
                if name.is_empty() {
                    return Err("a `(` is not followed by a function name".to_owned());
                }
                let mut arguments = Vec::new();
                loop {
                    self.skip_space();
                    if self.chars.next_if_eq(&')').is_some() {
                        return Ok(Expr::Call(name, arguments));
                    }
                    if self.chars.peek().is_none() {
                        return Err(format!("`({name}` is not closed"));
                    }
                    arguments.push(self.expr()?);
                }
            }
            Some(c) => Err(format!("unexpected `{c}`")),
            None => Err("unexpected end".to_owned()),
        }
    }
}

fn strings(key: &str, value: &Expr) -> Result<BTreeSet<String>, String> {
    let Expr::Call(name, items) = value else {
        return Err(format!("`{key}` must be a (list ...)"));
    };
    if name != "list" {
        return Err(format!("`{key}` must be a (list ...), not ({name} ...)"));
    }
    let mut set = BTreeSet::new();
    for item in items {
        let Expr::Str(text) = item else {
            return Err(format!("`{key}` must list strings only"));
        };
        if !set.insert(text.clone()) {
            return Err(format!("`{key}` lists {text} twice"));
        }
    }
    Ok(set)
}

fn row(expr: &Expr) -> Result<PublicPathRow, String> {
    let Expr::Call(name, arguments) = expr else {
        return Err("each row must be a (dict ...)".to_owned());
    };
    if name != "dict" || !arguments.len().is_multiple_of(2) {
        return Err(format!(
            "each row must be a (dict ...) of key-value pairs, not ({name} ...)"
        ));
    }
    let (mut key, mut host, mut service, mut methods, mut fixed) = (None, None, None, None, None);
    for pair in arguments.chunks(2) {
        let [Expr::Str(field), value] = pair else {
            return Err("a (dict ...) field name must be a string".to_owned());
        };
        let text = || match value {
            Expr::Str(text) => Ok(text.clone()),
            Expr::Call(..) => Err(format!("`{field}` must be a string")),
        };
        let slot = match field.as_str() {
            "key" => &mut key,
            "host" => &mut host,
            "service" => &mut service,
            "fixed" => &mut fixed,
            "methods" => {
                if methods.replace(strings(field, value)?).is_some() {
                    return Err("`methods` is given twice".to_owned());
                }
                continue;
            }
            // Fail closed: a new field may change what the ingress admits.
            other => {
                return Err(format!(
                    "unknown field `{other}`; extend repo-check to read it"
                ));
            }
        };
        if slot.replace(text()?).is_some() {
            return Err(format!("`{field}` is given twice"));
        }
    }
    let key = key.ok_or_else(|| "a row has no `key`".to_owned())?;
    let missing = |field: &str| format!("row {key} has no `{field}`");
    Ok(PublicPathRow {
        host: host.ok_or_else(|| missing("host"))?,
        service: service.ok_or_else(|| missing("service"))?,
        methods: methods.ok_or_else(|| missing("methods"))?,
        fixed,
        key,
    })
}

/// Reads the plan table `trustiot.ingress.publicPathTable` from the ingress template: the
/// `(list (dict ...) ...)` between the `define` and `| toYaml`.
///
/// # Errors
///
/// When the table is missing, empty, or holds anything but rows of known string fields and a
/// method list; the message says what to fix.
pub fn parse_public_path_table(template: &str) -> Result<Vec<PublicPathRow>, String> {
    let context = |message: String| format!("{INGRESS_TEMPLATE}: publicPathTable: {message}");
    let (_, after) = template
        .split_once(TABLE_DEFINE)
        .ok_or_else(|| context("no `define`".to_owned()))?;
    let (_, list) = after
        .split_once(TABLE_START)
        .ok_or_else(|| context("no `{{- list`".to_owned()))?;
    let (list, _) = list
        .split_once(TABLE_END)
        .ok_or_else(|| context("the list does not end with `| toYaml -}}`".to_owned()))?;
    let mut reader = Reader {
        chars: list.chars().peekable(),
    };
    let mut rows: Vec<PublicPathRow> = Vec::new();
    loop {
        reader.skip_space();
        if reader.chars.peek().is_none() {
            break;
        }
        let row = reader.expr().and_then(|expr| row(&expr)).map_err(context)?;
        if rows.iter().any(|seen| seen.key == row.key) {
            return Err(context(format!("{} is listed twice", row.key)));
        }
        rows.push(row);
    }
    if rows.is_empty() {
        return Err(context("no rows".to_owned()));
    }
    Ok(rows)
}

/// axum's method-router functions and the methods each registers. `get` also answers `HEAD`
/// with the same status and headers and no body.
fn axum_methods(function: &str) -> Option<&'static [&'static str]> {
    Some(match function {
        "get" => &["GET", "HEAD"],
        "head" => &["HEAD"],
        "post" => &["POST"],
        "put" => &["PUT"],
        "delete" => &["DELETE"],
        "patch" => &["PATCH"],
        "options" => &["OPTIONS"],
        "trace" => &["TRACE"],
        "connect" => &["CONNECT"],
        _ => return None,
    })
}

/// Reads a path's operations from an OpenAPI document. An operation `get` counts `HEAD` too,
/// because the services route with axum, whose `get` answers `HEAD`.
///
/// # Errors
///
/// When the document does not parse or has no operation on the path.
pub fn openapi_methods(openapi: &str, path: &str) -> Result<BTreeSet<String>, String> {
    let document: serde_json::Value =
        serde_json::from_str(openapi).map_err(|error| format!("{OPENAPI}: {error}"))?;
    let item = document
        .get("paths")
        .and_then(|paths| paths.get(path))
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| format!("{OPENAPI}: no paths.{path}"))?;
    let methods: BTreeSet<String> = item
        .keys()
        .filter_map(|name| axum_methods(name))
        .flatten()
        .map(|method| (*method).to_owned())
        .collect();
    if methods.is_empty() {
        return Err(format!("{OPENAPI}: paths.{path} has no operation"));
    }
    Ok(methods)
}

/// The code a check reads: what comes before the first inline `#[cfg(test)] mod name {`
/// (clippy's `items_after_test_module` keeps an inline test module at the end of its file),
/// without `//` comment lines. A test module in its own file (`#[cfg(test)] mod name;`) ends
/// nothing: runtime code may follow it. Other `#[cfg(test)]` items stay in too: reading more code
/// can only add methods, which makes the comparison fail rather than pass.
pub fn runtime_code(source: &str) -> String {
    const TEST_CFG: &str = "#[cfg(test)]";
    let end = source
        .match_indices(TEST_CFG)
        .map(|(start, _)| start)
        .find(|start| {
            let item = source
                .get(start + TEST_CFG.len()..)
                .unwrap_or_default()
                .trim_start();
            let item = item.strip_prefix("pub ").unwrap_or(item);
            item.starts_with("mod ")
                && item
                    .find('{')
                    .is_some_and(|brace| item.find(';').is_none_or(|semicolon| brace < semicolon))
        })
        .unwrap_or(source.len());
    let code = source.get(..end).unwrap_or(source);
    code.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Returns the arguments of the call whose `(` comes right before `text`, up to its `)`; None
/// when it is not closed. Parentheses inside string literals do not count.
fn call_arguments(text: &str) -> Option<&str> {
    let mut depth = 0_usize;
    let mut chars = text.char_indices();
    while let Some((index, c)) = chars.next() {
        match c {
            '"' => {
                while let Some((_, c)) = chars.next() {
                    match c {
                        '\\' => {
                            chars.next();
                        }
                        '"' => break,
                        _ => {}
                    }
                }
            }
            '(' => depth += 1,
            ')' if depth == 0 => return text.get(..index),
            ')' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// The top-level function names of a method-router expression such as `get(a).post(b)` or
/// `axum::routing::post(handler)`: each name called outside any argument list. A trailing comma
/// (rustfmt adds one when it puts the arguments of `.route` on their own lines) is dropped.
fn top_level_calls(expression: &str) -> Result<Vec<String>, String> {
    let mut calls = Vec::new();
    let mut rest = expression.trim().trim_end_matches(',').trim_end();
    while !rest.is_empty() {
        let name_end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == ':'))
            .unwrap_or(rest.len());
        let (path, after) = rest.split_at(name_end);
        let after = after.trim_start();
        let Some(arguments) = after.strip_prefix('(') else {
            return Err(format!(
                "cannot read the method router `{}`",
                expression.trim()
            ));
        };
        let inner = call_arguments(arguments)
            .ok_or_else(|| format!("`{path}(` is not closed in `{}`", expression.trim()))?;
        let name = path.rsplit("::").next().unwrap_or(path);
        calls.push(name.to_owned());
        let after_call = arguments
            .get(inner.len() + 1..)
            .unwrap_or_default()
            .trim_start();
        rest = match after_call.strip_prefix('.') {
            Some(next) => next.trim_start(),
            None if after_call.is_empty() => after_call,
            None => {
                return Err(format!(
                    "cannot read the method router `{}`",
                    expression.trim()
                ));
            }
        };
    }
    Ok(calls)
}

/// Router calls that bind a path, as the check reads them: `.route` with a method router, and
/// the forms whose methods it cannot read.
const ROUTE_CALL: &str = ".route(";
const OPAQUE_CALLS: [&str; 3] = [".route_service(", ".nest(", ".nest_service("];

/// The first argument of every `.route(`, `.route_service(`, `.nest(` and `.nest_service(` call
/// in the code, with the call and the rest of its arguments.
fn path_bindings(code: &str) -> Result<Vec<(&'static str, String, String)>, String> {
    let mut found = Vec::new();
    for call in std::iter::once(ROUTE_CALL).chain(OPAQUE_CALLS) {
        for (start, _) in code.match_indices(call) {
            let after = code.get(start + call.len()..).unwrap_or_default();
            let arguments =
                call_arguments(after).ok_or_else(|| format!("`{call}` is not closed"))?;
            let (first, rest) = arguments.split_once(',').unwrap_or((arguments, ""));
            found.push((call, first.trim().to_owned(), rest.trim().to_owned()));
        }
    }
    Ok(found)
}

/// Reads the methods that the crate's code registers for one path with axum: every
/// `.route(<constant, also qualified as a::CONSTANT, or literal>, <method router>)`, the method
/// routers joined. `sources` are
/// the files' contents; each is read up to its unit tests, without comment lines.
///
/// # Errors
///
/// When no `.route` registers the path, when a `.route_service` binds it or a `.nest` or
/// `.nest_service` may lead to it (a prefix of the path, or one that is not a literal), since
/// their methods cannot be read, or when a method router is not a chain of axum's
/// method functions (`get(a).post(b)`).
pub fn route_methods<'a>(
    sources: impl IntoIterator<Item = &'a str>,
    constant: &str,
    path: &str,
) -> Result<BTreeSet<String>, String> {
    let literal = format!("\"{path}\"");
    let qualified = format!("::{constant}");
    let mut methods = BTreeSet::new();
    let mut routes = 0_usize;
    for source in sources {
        for (call, first, router) in path_bindings(&runtime_code(source))? {
            let nest = call == ".nest(" || call == ".nest_service(";
            // A nested router's paths are the prefix joined with its own, so any nest that may
            // lead to the path is unreadable: a prefix that is not a literal, or one of the path's.
            let nested_here = nest
                && first
                    .strip_prefix('"')
                    .and_then(|f| f.strip_suffix('"'))
                    .is_none_or(|prefix| {
                        path == prefix
                            || path
                                .strip_prefix(prefix)
                                .is_some_and(|tail| tail.starts_with('/'))
                    });
            let names_path = first == constant || first.ends_with(&qualified) || first == literal;
            if !(names_path || nested_here) {
                continue;
            }
            if call != ROUTE_CALL {
                return Err(format!(
                    "{path} is bound with `{call}`, whose methods this check cannot read; \
                     register it with `.route({constant}, ...)` or extend repo-check"
                ));
            }
            for function in top_level_calls(&router)? {
                let registered = axum_methods(&function).ok_or_else(|| {
                    format!(
                        "`.route({first}, ...)` calls `{function}`, which is not one of axum's \
                         method functions (get, post, ...); extend repo-check to read it"
                    )
                })?;
                methods.extend(registered.iter().map(|method| (*method).to_owned()));
            }
            routes += 1;
        }
    }
    if routes == 0 {
        return Err(format!("no `.route({constant}, ...)` registers {path}"));
    }
    Ok(methods)
}

/// Lists the paths that the code binds with `.route`, `.route_service`, `.nest` or
/// `.nest_service`, for a service that must take every path through its fallback.
///
/// # Errors
///
/// When a call is not closed.
pub fn bound_paths(source: &str) -> Result<Vec<String>, String> {
    Ok(path_bindings(&runtime_code(source))?
        .into_iter()
        .map(|(call, first, _)| format!("{call}{first}"))
        .collect())
}

/// Reads the one method a handler admits from its comparison `method != Method::<NAME>`.
///
/// # Errors
///
/// When the code names `Method::` anywhere else or more than once, so the admitted methods
/// cannot be read off one comparison.
pub fn compared_methods(source: &str) -> Result<BTreeSet<String>, String> {
    let code = runtime_code(source);
    let mut methods = BTreeSet::new();
    for (start, _) in code.match_indices("Method::") {
        let (before, after) = code.split_at(start);
        let name: String = after
            .trim_start_matches("Method::")
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect();
        if !before.trim_end().ends_with("method !=") || name.is_empty() || !methods.is_empty() {
            return Err(format!(
                "`Method::{name}` is used other than in the one comparison `method != Method::X`; \
                 extend repo-check to read the methods it admits"
            ));
        }
        methods.insert(name);
    }
    if methods.is_empty() {
        return Err("no `method != Method::X` comparison".to_owned());
    }
    Ok(methods)
}

/// Reads the value of `const <name>: &str = "...";` from the code.
///
/// # Errors
///
/// When the constant is not defined exactly once in that form.
pub fn string_constant(source: &str, name: &str) -> Result<String, String> {
    let declaration = format!("const {name}: &str = \"");
    let code = runtime_code(source);
    let mut values = code.match_indices(&declaration).map(|(start, _)| {
        code.get(start + declaration.len()..)
            .and_then(|rest| rest.split_once('"'))
            .map(|(value, _)| value.to_owned())
    });
    match (values.next(), values.next()) {
        (Some(Some(value)), None) => Ok(value),
        _ => Err(format!("`{declaration}...\";` is not defined exactly once")),
    }
}

/// Compares the methods the table admits for `key` with what `source` serves.
///
/// # Errors
///
/// When they differ; the message names both sides.
pub fn compare_methods(
    row: &PublicPathRow,
    source: &str,
    served: &BTreeSet<String>,
) -> Result<(), String> {
    if &row.methods == served {
        return Ok(());
    }
    let list = |set: &BTreeSet<String>| set.iter().cloned().collect::<Vec<_>>().join(", ");
    Err(format!(
        "ingress.publicPaths.{}: the plan table in {INGRESS_TEMPLATE} admits [{}] but {source} \
         serves [{}]; change the table together with the service (deploy/chart/README.md, \
         「入口 gateway」), but a method that needs the session belongs on a cookie route, never \
         in this table",
        row.key,
        list(&row.methods),
        list(served)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(methods: &[&str]) -> BTreeSet<String> {
        methods.iter().map(|method| (*method).to_owned()).collect()
    }

    fn template(rows: &str) -> String {
        format!(
            "{{{{- define \"other\" -}}}}\n{{{{- list (dict \"key\" \"x\") | toYaml -}}}}\n\
             {{{{- end -}}}}\n{TABLE_DEFINE}\n{{{{- /*\ncomment (dict \"key\" \"no\")\n*/ -}}}}\n\
             {{{{- list\n{rows} | toYaml -}}}}\n{{{{- end -}}}}\n"
        )
    }

    #[test]
    fn the_table_is_read_row_by_row() {
        let rows = parse_public_path_table(&template(
            "  (dict \"key\" \"login\" \"host\" \"app\" \"service\" \"identity\" \"methods\" (list \"POST\") \"fixed\" \"/auth/login\")\n  \
             (dict \"key\" \"mqtt\" \"host\" \"gateway\" \"service\" \"mqtt-bridge\" \"methods\" (list \"GET\" \"HEAD\"))",
        ))
        .expect("the table parses");
        assert_eq!(
            rows,
            vec![
                PublicPathRow {
                    key: "login".to_owned(),
                    host: "app".to_owned(),
                    service: "identity".to_owned(),
                    methods: set(&["POST"]),
                    fixed: Some("/auth/login".to_owned()),
                },
                PublicPathRow {
                    key: "mqtt".to_owned(),
                    host: "gateway".to_owned(),
                    service: "mqtt-bridge".to_owned(),
                    methods: set(&["GET", "HEAD"]),
                    fixed: None,
                },
            ]
        );
    }

    #[test]
    fn a_table_it_cannot_read_fails() {
        let row = |fields: &str| {
            format!("(dict \"key\" \"k\" \"host\" \"app\" \"service\" \"s\" {fields})")
        };
        for (rows, expected) in [
            (row(""), "row k has no `methods`"),
            (
                row("\"methods\" \"POST\""),
                "`methods` must be a (list ...)",
            ),
            (
                row("\"methods\" (list \"POST\" \"POST\")"),
                "`methods` lists POST twice",
            ),
            (
                row("\"methods\" (list \"POST\") \"verbs\" \"GET\""),
                "unknown field `verbs`",
            ),
            (
                row("\"methods\" (list \"POST\") \"fixed\" (list \"/a\")"),
                "`fixed` must be a string",
            ),
            (
                row("\"methods\" (list \"POST\") \"host\" \"gateway\""),
                "`host` is given twice",
            ),
            (row("\"methods\" (list \"POST\""), "is not closed"),
            (
                format!("{0}\n{0}", row("\"methods\" (list \"POST\")")),
                "k is listed twice",
            ),
            ("(list \"a\")".to_owned(), "must be a (dict ...)"),
            (String::new(), "no rows"),
        ] {
            let error = parse_public_path_table(&template(&rows)).expect_err(&rows);
            assert!(error.contains(expected), "{rows}: {error}");
        }
        assert!(parse_public_path_table("{{- define \"x\" -}}").is_err());
    }

    #[test]
    fn openapi_operations_are_read_with_head_for_get() {
        let document = r#"{"paths": {"/a": {"post": {}, "parameters": []}, "/b": {"get": {}, "delete": {}}, "/c": {"parameters": []}}}"#;
        assert_eq!(openapi_methods(document, "/a"), Ok(set(&["POST"])));
        assert_eq!(
            openapi_methods(document, "/b"),
            Ok(set(&["DELETE", "GET", "HEAD"]))
        );
        assert!(openapi_methods(document, "/c").is_err());
        assert!(openapi_methods(document, "/d").is_err());
        assert!(openapi_methods("{", "/a").is_err());
    }

    const ROUTES: &str = r#"
//! `.route(PATH, put(x))` in a comment is not code.
use axum::routing::{get, post};
pub const PATH: &str = "/a";
pub fn routes() -> Router {
    Router::new()
        .route(PATH, get(read))
        .route("/b", post(write))
        .route_layer(axum::middleware::map_response(no_store))
}
#[cfg(test)]
mod tests {
    fn extra() -> Router { Router::new().route(PATH, delete(gone)) }
}
"#;

    #[test]
    fn route_methods_read_the_method_router_of_the_path() {
        assert_eq!(
            route_methods([ROUTES], "PATH", "/a"),
            Ok(set(&["GET", "HEAD"]))
        );
        assert_eq!(route_methods([ROUTES], "B_PATH", "/b"), Ok(set(&["POST"])));
        assert_eq!(string_constant(ROUTES, "PATH"), Ok("/a".to_owned()));
        // Only a test module ends the code: a test-only item before the routes hides nothing.
        let helper_first = format!("#[cfg(test)]\nfn helper() {{}}\n{ROUTES}");
        assert_eq!(
            route_methods([helper_first.as_str()], "PATH", "/a"),
            Ok(set(&["GET", "HEAD"]))
        );
        assert!(string_constant(ROUTES, "OTHER").is_err());
    }

    // The mutations the repository test must catch: a method chained on the route, a second
    // registration of the path (by constant or literal, in another file), and forms it cannot read.
    #[test]
    fn a_method_added_anywhere_changes_the_answer_or_fails() {
        let chained = ROUTES.replace("get(read)", "get(read).post(write)");
        assert_eq!(
            route_methods([chained.as_str()], "PATH", "/a"),
            Ok(set(&["GET", "HEAD", "POST"]))
        );
        let multiline = ROUTES.replace(
            "get(read)",
            "get(read)\n            .put(|| async { \")\" })",
        );
        assert_eq!(
            route_methods([multiline.as_str()], "PATH", "/a"),
            Ok(set(&["GET", "HEAD", "PUT"]))
        );
        let other =
            "fn more() -> Router { Router::new().route(\"/a\", axum::routing::patch(edit)) }";
        assert_eq!(
            route_methods([ROUTES, other], "PATH", "/a"),
            Ok(set(&["GET", "HEAD", "PATCH"]))
        );
        // A qualified constant names the path too.
        let qualified =
            "fn q() -> Router { Router::new().route(crate::paths::PATH, delete(gone)) }";
        assert_eq!(
            route_methods([ROUTES, qualified], "PATH", "/a"),
            Ok(set(&["DELETE", "GET", "HEAD"]))
        );
        // A test module in its own file ends nothing: the code after it is read.
        let after_module = format!(
            "#[cfg(test)]\nmod test_keys;\nfn late() -> Router {{ Router::new().route(PATH, put(x)) }}\n{ROUTES}"
        );
        assert_eq!(
            route_methods([after_module.as_str()], "PATH", "/a"),
            Ok(set(&["GET", "HEAD", "PUT"]))
        );
        // rustfmt's layout of a long route, with a trailing comma.
        let wrapped = ROUTES.replace(
            ".route(PATH, get(read))",
            ".route(\n            PATH,\n            get(read).post(write),\n        )",
        );
        assert_eq!(
            route_methods([wrapped.as_str()], "PATH", "/a"),
            Ok(set(&["GET", "HEAD", "POST"]))
        );
        // A nest under another literal prefix cannot lead to the path; one under its prefix, or
        // under a prefix that is not a literal, can.
        let nests =
            |prefix: &str| format!("Router::new().nest({prefix}, inner).route(\"/x/a\", get(h))");
        assert_eq!(
            route_methods([nests("\"/y\"").as_str()], "A", "/x/a"),
            Ok(set(&["GET", "HEAD"]))
        );
        for prefix in ["\"/x\"", "\"/x/a\"", "PREFIX"] {
            let error = route_methods([nests(prefix).as_str()], "A", "/x/a").expect_err(prefix);
            assert!(error.contains("`.nest(`"), "{error}");
        }
        for (source, expected) in [
            (ROUTES.replace("get(read)", "any(read)"), "calls `any`"),
            (
                ROUTES.replace("get(read)", "on(MethodFilter::GET, read)"),
                "calls `on`",
            ),
            (
                ROUTES.replace("get(read)", "router"),
                "cannot read the method router",
            ),
            (
                ROUTES.replace("get(read)", "get(read).layer(x)"),
                "calls `layer`",
            ),
            (
                ROUTES.replace(".route(PATH, get(read))", ".route_service(PATH, svc)"),
                "`.route_service(`",
            ),
            (
                ROUTES.replace(".route(PATH, get(read))", ".nest(PATH, inner)"),
                "`.nest(`",
            ),
            (
                ROUTES.replace(".route(PATH, get(read))", ""),
                "no `.route(PATH, ...)`",
            ),
        ] {
            let error = route_methods([source.as_str()], "PATH", "/a").expect_err(&source);
            assert!(error.contains(expected), "{error}");
        }
    }

    const HANDSHAKE: &str = r"
use axum::http::{Method, Version};
pub fn accept(method: &Method, version: Version) -> Result<(), Error> {
    if method != Method::GET {
        return Err(Error::Method);
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    fn post() { accept(&Method::POST, Version::HTTP_11); }
}
";

    #[test]
    fn the_compared_method_is_read_and_any_other_use_fails() {
        assert_eq!(compared_methods(HANDSHAKE), Ok(set(&["GET"])));
        for source in [
            HANDSHAKE.replace(
                "method != Method::GET",
                "method != Method::GET && method != Method::CONNECT",
            ),
            HANDSHAKE.replace("method != Method::GET", "method == Method::GET"),
            HANDSHAKE.replace(
                "Ok(())",
                "if method == Method::HEAD { return Ok(()); }\n    Ok(())",
            ),
            HANDSHAKE.replace("if method != Method::GET {", "if false {"),
        ] {
            assert!(compared_methods(&source).is_err(), "{source}");
        }
    }

    #[test]
    fn bound_paths_list_every_binding() {
        assert_eq!(
            bound_paths(ROUTES),
            Ok(vec![".route(PATH".to_owned(), ".route(\"/b\"".to_owned()])
        );
        assert_eq!(
            bound_paths("Router::new().fallback(upgrade)"),
            Ok(Vec::new())
        );
        assert_eq!(
            bound_paths("Router::new().nest_service(\"/x\", svc)"),
            Ok(vec![".nest_service(\"/x\"".to_owned()])
        );
    }

    #[test]
    fn differing_methods_fail_with_both_sides() {
        let row = PublicPathRow {
            key: "login".to_owned(),
            host: "app".to_owned(),
            service: "identity".to_owned(),
            methods: set(&["POST"]),
            fixed: None,
        };
        assert_eq!(compare_methods(&row, "x", &set(&["POST"])), Ok(()));
        let error = compare_methods(&row, "api/openapi.json", &set(&["GET", "HEAD", "POST"]))
            .expect_err("differs");
        assert!(
            error.contains("admits [POST] but api/openapi.json serves [GET, HEAD, POST]"),
            "{error}"
        );
    }
}
