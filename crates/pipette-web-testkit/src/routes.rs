//! The routes `router.rs` registers, read from its source: the tests that hold
//! a list or a table to the router share this one reading of it.

use anyhow::Context;

/// The `axum::routing` functions a chain registers a method with.
const METHODS: &[&str] = &["get", "post", "put", "patch", "delete", "head", "any"];

/// One method of one `.route(`, as the source spells it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RegisteredRoute {
    /// In capitals, as a request carries it.
    pub method: String,
    pub path: String,
    /// The handler's name, without its module path.
    pub handler: String,
    /// The expression its `gate(..)` takes, without whitespace, if it has one.
    pub gate: Option<String>,
}

/// Every route `source`, the text of `router.rs`, registers. A builder chain
/// spans lines, so each `.route(` is read to its matching close paren.
pub fn registered_routes(source: &str) -> anyhow::Result<Vec<RegisteredRoute>> {
    // Comments carry parens and route-like prose, so they go first.
    let code: String = source
        .lines()
        .map(|line| line.split("//").next().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    anyhow::ensure!(
        [".nest(", ".merge(", ".route_service("]
            .iter()
            .all(|call| !code.contains(call)),
        "a nested or merged router hides its routes from this scan"
    );
    let routes = code
        .match_indices(".route(")
        .map(|(start, call)| route(code.get(start + call.len()..).unwrap_or_default()))
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(routes.into_iter().flatten().collect())
}

/// The methods of the `.route(` whose arguments `rest` opens with.
fn route(rest: &str) -> anyhow::Result<Vec<RegisteredRoute>> {
    let arguments = enclosed(rest)?;
    let (path, chain) = arguments
        .trim_start()
        .strip_prefix('"')
        .and_then(|rest| rest.split_once('"'))
        .with_context(|| format!("a `.route(` that does not open with a path: {arguments}"))?;
    let routes = method_calls(chain)
        .map(|(method, start)| {
            let (handler, gate) = declared(chain.get(start..).unwrap_or_default())?;
            Ok(RegisteredRoute {
                method: method.to_uppercase(),
                path: path.to_owned(),
                handler,
                gate,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    anyhow::ensure!(!routes.is_empty(), "no method read for {path}: {chain}");
    Ok(routes)
}

/// `text` up to the paren that closes the one just before it.
fn enclosed(text: &str) -> anyhow::Result<&str> {
    let mut depth = 1usize;
    let end = text
        .char_indices()
        .find_map(|(at, c)| {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
            (depth == 0).then_some(at)
        })
        .context("an open paren with no close")?;
    Ok(text.get(..end).unwrap_or_default())
}

/// `text` split at its first comma outside any bracket.
fn first_argument(text: &str) -> (&str, &str) {
    let mut depth = 0i32;
    text.char_indices()
        .find(|(_, c)| {
            match c {
                '(' | '[' => depth += 1,
                ')' | ']' => depth -= 1,
                _ => {}
            }
            *c == ',' && depth == 0
        })
        .map_or((text, ""), |(at, _)| {
            (
                text.get(..at).unwrap_or_default(),
                text.get(at + 1..).unwrap_or_default(),
            )
        })
}

fn squeezed(text: &str) -> String {
    text.split_whitespace().collect()
}

/// Each `get(`, `post(` and the like in a route's method chain: its method,
/// and where its argument starts. A `module::fn(` path is no call.
fn method_calls(chain: &str) -> impl Iterator<Item = (&str, usize)> + '_ {
    chain.match_indices('(').filter_map(move |(open, _)| {
        let before = chain.get(..open)?;
        let name_at = before
            .rfind(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .map_or(0, |at| at + 1);
        let name = before.get(name_at..)?;
        let pathed = before.get(..name_at).is_some_and(|b| b.ends_with(':'));
        (METHODS.contains(&name) && !pathed).then_some((name, open + 1))
    })
}

/// The handler and the gate of the method call whose argument `rest` opens
/// with.
fn declared(rest: &str) -> anyhow::Result<(String, Option<String>)> {
    let argument = enclosed(rest)?.trim();
    let (gate, handler) = match argument.strip_prefix("gate(") {
        Some(gated) => {
            let (gate, handler) = first_argument(enclosed(gated)?);
            (Some(squeezed(gate)), handler)
        }
        None => (None, argument),
    };
    // rustfmt ends a call spread over lines with a comma.
    let handler = squeezed(handler);
    let handler = handler
        .trim_end_matches(',')
        .rsplit("::")
        .next()
        .unwrap_or_default()
        .to_owned();
    Ok((handler, gate))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = r#"
        router
            // .route("/commented", get(out))
            .route("/health", get(health::health))
            .route(
                "/api/v1/things/{thing_id}",
                get(gate(Needs(ReadThings), api_v1::things::read)).delete(gate(
                    AnyOf(&[ManageThings, ManageOrgs], "either"),
                    api_v1::things::delete,
                )),
            )
            .route(
                "/upload",
                post(upload).layer(DefaultBodyLimit::max(LIMIT)),
            )
    "#;

    fn route(method: &str, path: &str, handler: &str, gate: Option<&str>) -> RegisteredRoute {
        RegisteredRoute {
            method: method.to_owned(),
            path: path.to_owned(),
            handler: handler.to_owned(),
            gate: gate.map(str::to_owned),
        }
    }

    #[test]
    fn test_each_method_reads_with_its_handler_and_its_gate() -> anyhow::Result<()> {
        let thing = "/api/v1/things/{thing_id}";
        assert_eq!(
            registered_routes(SOURCE)?,
            [
                route("GET", "/health", "health", None),
                route("GET", thing, "read", Some("Needs(ReadThings)")),
                route(
                    "DELETE",
                    thing,
                    "delete",
                    Some(r#"AnyOf(&[ManageThings,ManageOrgs],"either")"#)
                ),
                route("POST", "/upload", "upload", None),
            ]
        );
        Ok(())
    }

    /// A source the scan cannot read is refused, naming why, rather than read
    /// as fewer routes.
    #[test]
    fn test_a_router_the_scan_cannot_read_is_refused() -> anyhow::Result<()> {
        let cases = [
            (r#"router.nest("/api/v1", inner())"#, "nested or merged"),
            ("router.route(PATH, get(read))", "does not open with a path"),
            (r#"router.route("/files", serve(files))"#, "no method read"),
            (r#"router.route("/files", get(read)"#, "no close"),
        ];
        let wrong: Vec<String> = cases
            .iter()
            .filter_map(|(source, reason)| {
                let refusal = registered_routes(source).err().map(|e| format!("{e:#}"));
                (!refusal.as_deref().is_some_and(|text| text.contains(reason)))
                    .then(|| format!("{source}: {refusal:?}, not a refusal naming {reason:?}"))
            })
            .collect();
        anyhow::ensure!(wrong.is_empty(), "{wrong:#?}");
        Ok(())
    }
}
