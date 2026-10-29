//! The document as one static page and its stylesheet, with no script: the API
//! reference mgmt serves at `/docs/` and `pipette-mgmt openapi --site` writes.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Context;
use pulldown_cmark::{CowStr, Event, HeadingLevel, Options, Parser, Tag, TagEnd, html};
use serde_json::{Map, Value};

use super::route_docs::doc_dir;

const STYLE: &str = include_str!("site.css");
const METHODS: [&str; 6] = ["get", "put", "post", "patch", "delete", "head"];
static NONE: Value = Value::Null;

/// The page's files, by name.
pub(super) fn files(document: &Value) -> anyhow::Result<[(&'static str, String); 2]> {
    let page = Page::new(document)?.render()?;
    Ok([("index.html", page), ("style.css", STYLE.to_owned())])
}

struct Operation<'a> {
    path: &'a str,
    method: &'a str,
    operation: &'a Value,
    anchor: String,
}

struct Section<'a> {
    tag: &'a Value,
    name: &'a str,
    slug: &'static str,
    operations: Vec<Operation<'a>>,
}

/// A schema past any lone `allOf`, null `anyOf` arm and `$ref`.
struct Resolved<'a> {
    schema: &'a Value,
    model: Option<&'a str>,
    /// The first description on the way, else the resolved schema's own.
    description: Option<&'a str>,
    nullable: bool,
}

struct Page<'a> {
    document: &'a Value,
    sections: Vec<Section<'a>>,
    /// The route and tag section ids a `docs/api/` link may become.
    anchors: BTreeSet<String>,
    schemas: &'a Map<String, Value>,
    responses: &'a Map<String, Value>,
}

impl<'a> Page<'a> {
    fn new(document: &'a Value) -> anyhow::Result<Self> {
        let mut by_tag: BTreeMap<&str, (&'static str, Vec<Operation>)> = BTreeMap::new();
        object(document, "/paths")?
            .iter()
            .flat_map(|(path, item)| {
                entries(item, "")
                    .filter(|(m, _)| METHODS.contains(&m.as_str()))
                    .map(move |(method, operation)| (path, method, operation))
            })
            .try_for_each(|(path, method, operation)| {
                let id = text(operation, "/operationId").context("an operation has no id")?;
                let tag = text(operation, "/tags/0").with_context(|| format!("{id} has no tag"))?;
                let slug = doc_dir(id).with_context(|| format!("{id} has no file in docs/api/"))?;
                let (dir, operations) = by_tag.entry(tag).or_insert((slug, Vec::new()));
                if *dir != slug {
                    anyhow::bail!("tag {tag} spans two directories of docs/api/");
                }
                operations.push(Operation {
                    path,
                    method,
                    operation,
                    anchor: format!("tag/{slug}/{id}"),
                });
                Ok(())
            })?;
        let sections = array(document, "/tags")
            .iter()
            .map(|tag| {
                let name = text(tag, "/name").context("a tag has no name")?;
                let (slug, operations) = by_tag
                    .remove(name)
                    .with_context(|| format!("tag {name} heads no operation"))?;
                Ok(Section {
                    tag,
                    name,
                    slug,
                    operations,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        if let Some(name) = by_tag.keys().next() {
            anyhow::bail!("operations are tagged {name}, which the document's tags leave out");
        }
        let anchors = sections
            .iter()
            .flat_map(|s| {
                s.operations
                    .iter()
                    .map(|o| o.anchor.clone())
                    .chain([format!("tag/{}", s.slug)])
            })
            .collect();
        Ok(Self {
            document,
            sections,
            anchors,
            schemas: object(document, "/components/schemas")?,
            responses: object(document, "/components/responses")?,
        })
    }

    fn render(&self) -> anyhow::Result<String> {
        let title = esc(text(self.document, "/info/title").unwrap_or("API"));
        let mut nav = String::from(
            r##"<li><a href="#introduction">Introduction</a></li><li><a href="#authentication">Authentication</a></li>"##,
        );
        let schemes = entries(self.document, "/components/securitySchemes")
            .map(|(name, scheme)| {
                let name = esc(name);
                Ok(format!(
                    r#"<h3 id="auth/{name}">{name}</h3><div class="prose">{}</div>"#,
                    self.markdown(text(scheme, "/description").unwrap_or_default(), 3)?
                ))
            })
            .collect::<anyhow::Result<String>>()?;
        let mut main = format!(
            r#"<section id="introduction"><h1>{title}</h1><div class="prose">{}</div></section><section id="authentication"><h2>Authentication</h2>{schemes}</section>"#,
            self.markdown(
                text(self.document, "/info/description").unwrap_or_default(),
                1
            )?
        );
        self.sections.iter().try_for_each(|section| {
            let (name, slug) = (esc(section.name), section.slug);
            let links: String = section
                .operations
                .iter()
                .map(|o| {
                    let summary = esc(text(o.operation, "/summary").unwrap_or(o.path));
                    let method = o.method;
                    let badge = method.to_uppercase();
                    format!(
                        r##"<li><a href="#{}">{summary}<span class="m {method}">{badge}</span></a></li>"##,
                        o.anchor
                    )
                })
                .collect();
            let operations = section
                .operations
                .iter()
                .map(|o| self.operation(o))
                .collect::<anyhow::Result<String>>()?;
            nav.push_str(&format!(
                r##"<li><details><summary>{name}</summary><ul><li><a href="#tag/{slug}">Overview</a></li>{links}</ul></details></li>"##
            ));
            main.push_str(&format!(
                r##"<section id="tag/{slug}"><h2><a class="self" href="#tag/{slug}">{name}</a></h2><div class="prose">{}</div></section>{operations}"##,
                self.markdown(text(section.tag, "/description").unwrap_or_default(), 1)?
            ));
            anyhow::Ok(())
        })?;
        nav.push_str(r##"<li><a href="#models">Models</a></li>"##);
        let models = self
            .schemas
            .iter()
            .map(|(name, schema)| {
                // The id sits inside, so a link to it opens the model.
                let name = esc(name);
                Ok(format!(
                    r#"<details class="model"><summary>{name}</summary><div id="model/{name}">{}</div></details>"#,
                    self.tree(schema)?
                ))
            })
            .collect::<anyhow::Result<String>>()?;
        main.push_str(&format!(
            r#"<section id="models"><h2>Models</h2>{models}</section>"#
        ));
        Ok(format!(
            r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'self'; base-uri 'none'; form-action 'none'">
<title>{title}</title>
<link rel="stylesheet" href="style.css">
</head>
<body>
<nav class="sidebar"><ul class="toc">{nav}</ul></nav>
<main>{main}</main>
</body>
</html>
"#
        ))
    }

    fn operation(&self, op: &Operation) -> anyhow::Result<String> {
        let o = op.operation;
        let summary = esc(text(o, "/summary").unwrap_or(op.path));
        let auth = array(o, "/security")
            .iter()
            .flat_map(|requirement| entries(requirement, ""))
            .map(|(scheme, names)| {
                let scheme = esc(scheme);
                let names = array(names, "").iter().filter_map(Value::as_str).map(esc);
                [format!(r##"<a href="#auth/{scheme}">{scheme}</a>"##)]
                    .into_iter()
                    .chain(names)
                    .collect::<Vec<_>>()
                    .join(" · ")
            })
            .collect::<Vec<_>>()
            .join(" or ");
        let auth = if auth.is_empty() {
            "unsigned".to_owned()
        } else {
            auth
        };
        let human = if o.get("x-human-only") == Some(&Value::Bool(true)) {
            r#"<span class="badge warn">human only</span>"#
        } else {
            ""
        };
        let (method, badge, path) = (op.method, op.method.to_uppercase(), esc(op.path));
        let mut out = format!(
            r##"<section class="op" id="{}"><h3><a class="self" href="#{0}">{summary}</a></h3><div class="badges"><span class="m {method}">{badge}</span><code>{path}</code><span class="badge">{auth}</span>{human}</div><div class="prose">{}</div>"##,
            op.anchor,
            self.markdown(text(o, "/description").unwrap_or_default(), 2)?
        );
        let parameters = [
            ("path", "Path parameters"),
            ("query", "Query parameters"),
            ("header", "Headers"),
        ]
        .into_iter()
        .map(|(place, title)| {
            let rows = array(o, "/parameters")
                .iter()
                .filter(|p| text(p, "/in") == Some(place))
                .map(|p| {
                    let name = text(p, "/name").unwrap_or_default();
                    let required = p.get("required") == Some(&Value::Bool(true));
                    let schema = p.get("schema").unwrap_or(&NONE);
                    self.prop(name, schema, text(p, "/description"), required, 0, &[])
                })
                .collect::<anyhow::Result<String>>()?;
            Ok(if rows.is_empty() {
                rows
            } else {
                format!(r#"<h4 class="sub">{title}</h4><ul class="props">{rows}</ul>"#)
            })
        })
        .collect::<anyhow::Result<String>>()?;
        out.push_str(&parameters);
        // The route and its examples: a sticky card beside the text on a wide screen.
        let mut card = format!(
            r#"<aside class="ex"><div class="ex-h"><span class="m {method}">{badge}</span> {path}</div>"#
        );
        if let Some(body) = o.get("requestBody") {
            let required = if body.get("required") == Some(&Value::Bool(true)) {
                r#" <span class="preq">required</span>"#
            } else {
                ""
            };
            entries(body, "/content").try_for_each(|(media, content)| {
                let media = esc(media);
                out.push_str(&format!(
                    r#"<h4 class="sub">Body <span class="mt">{media}</span>{required}</h4>{}"#,
                    self.tree(content.get("schema").unwrap_or(&NONE))?
                ));
                if let Some(example) = content.get("example") {
                    let example = match example.as_str() {
                        Some(text) => text.to_owned(),
                        None => serde_json::to_string_pretty(example)?,
                    };
                    card.push_str(&format!(
                        r#"<div class="mtl">Example · {media}</div><pre>{}</pre>"#,
                        esc(&example)
                    ));
                }
                anyhow::Ok(())
            })?;
        }
        let responses = entries(o, "/responses")
            .map(|(status, response)| {
                let shared = text(response, "/$ref")
                    .and_then(|r| r.strip_prefix("#/components/responses/"))
                    .and_then(|name| self.responses.get(name))
                    .unwrap_or(response);
                let description =
                    text(response, "/description").or_else(|| text(shared, "/description"));
                let bodies = entries(shared, "/content")
                    .map(|(media, content)| {
                        Ok(format!(
                            r#"<div class="mtl">{}</div>{}"#,
                            esc(media),
                            self.tree(content.get("schema").unwrap_or(&NONE))?
                        ))
                    })
                    .collect::<anyhow::Result<String>>()?;
                Ok(format!(
                    r#"<li><details><summary><code class="st">{}</code> <span class="rd">{}</span></summary><div class="resp-body">{bodies}</div></details></li>"#,
                    esc(status),
                    self.markdown(description.unwrap_or_default(), 0)?
                ))
            })
            .collect::<anyhow::Result<String>>()?;
        out.push_str(&format!(
            r#"{card}</aside><h4 class="sub">Responses</h4><ul class="responses">{responses}</ul></section>"#
        ));
        Ok(out)
    }

    /// A body, response or model: its name, its description, its fields.
    fn tree(&self, schema: &'a Value) -> anyhow::Result<String> {
        let resolved = self.resolve(schema);
        Ok(format!(
            r#"<div class="schema-name">{}</div>{}{}{}"#,
            self.label(schema),
            self.description(resolved.description)?,
            facts(schema, resolved.schema),
            self.children(schema, 0, &[])?
        ))
    }

    /// The fields and arms under a schema. A model shows once on a path, and
    /// past the top only as its link.
    fn children(
        &self,
        schema: &'a Value,
        depth: usize,
        seen: &[&'a str],
    ) -> anyhow::Result<String> {
        // A tagged arm adds its tag field beside the one model it flattens.
        if let ([base], Some(_)) = (array(schema, "/allOf"), schema.get("properties")) {
            return Ok(self.fields(schema, depth, seen)? + &self.children(base, depth, seen)?);
        }
        let resolved = self.resolve(schema);
        let s = resolved.schema;
        let mut seen = seen.to_vec();
        if let Some(model) = resolved.model {
            if depth > 0 || seen.contains(&model) {
                return Ok(String::new());
            }
            seen.push(model);
        }
        let mut out = self.fields(s, depth, &seen)?;
        let arms = match array(s, "/oneOf") {
            [] => array(s, "/anyOf"),
            arms => arms,
        };
        if !arms.is_empty() {
            let arms = arms
                .iter()
                .filter(|a| !is_null(a))
                .enumerate()
                .map(|(i, arm)| {
                    let inner = self.resolve(arm);
                    let title = text(arm, "/title")
                        .or_else(|| text(inner.schema, "/title"))
                        .or(inner.model);
                    let title = title.map_or_else(|| format!("option {}", i + 1), esc);
                    Ok(format!(
                        r#"<li class="arm"><span class="arm-t">{title}</span> <span class="ptype">{}</span>{}{}</li>"#,
                        self.label(arm),
                        self.description(inner.description)?,
                        self.children(arm, depth, &seen)?
                    ))
                })
                .collect::<anyhow::Result<String>>()?;
            out.push_str(&format!(r#"<ul class="arms">{arms}</ul>"#));
        }
        if !out.is_empty() {
            return Ok(out);
        }
        if let Some(items) = s.get("items") {
            return self.children(items, depth, &seen);
        }
        match s.get("additionalProperties") {
            Some(values @ Value::Object(_)) => self.children(values, depth, &seen),
            Some(_) | None => Ok(String::new()),
        }
    }

    fn fields(&self, s: &'a Value, depth: usize, seen: &[&'a str]) -> anyhow::Result<String> {
        let Some(properties) = s.get("properties").and_then(Value::as_object) else {
            return Ok(String::new());
        };
        let required: BTreeSet<&str> = array(s, "/required")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        let rows = properties
            .iter()
            .map(|(name, p)| {
                self.prop(
                    name,
                    p,
                    None,
                    required.contains(name.as_str()),
                    depth + 1,
                    seen,
                )
            })
            .collect::<anyhow::Result<String>>()?;
        Ok(format!(r#"<ul class="props">{rows}</ul>"#))
    }

    fn prop(
        &self,
        name: &str,
        schema: &'a Value,
        description: Option<&str>,
        required: bool,
        depth: usize,
        seen: &[&'a str],
    ) -> anyhow::Result<String> {
        let resolved = self.resolve(schema);
        let kids = self.children(schema, depth, seen)?;
        let kids = if kids.is_empty() {
            kids
        } else {
            format!(r#"<details class="kids"><summary>Fields</summary>{kids}</details>"#)
        };
        let required = if required {
            r#" <span class="preq">required</span>"#
        } else {
            ""
        };
        Ok(format!(
            r#"<li class="prop"><div class="prop-h"><code class="pname">{}</code> <span class="ptype">{}</span>{required}</div>{}{}{kids}</li>"#,
            esc(name),
            self.label(schema),
            self.description(description.or(resolved.description))?,
            facts(schema, resolved.schema)
        ))
    }

    /// A schema's type in a few words, a model as its link.
    fn label(&self, schema: &'a Value) -> String {
        let resolved = self.resolve(schema);
        let s = resolved.schema;
        let types: Vec<&str> = match s.get("type") {
            Some(Value::String(t)) => vec![t.as_str()],
            Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
            Some(_) | None => Vec::new(),
        };
        let nullable = resolved.nullable || types.contains(&"null");
        let types: Vec<&str> = types.into_iter().filter(|t| *t != "null").collect();
        let mut label = if let Some(model) = resolved.model {
            let model = esc(model);
            format!(r##"<a href="#model/{model}">{model}</a>"##)
        } else if let Some(value) = s.get("const") {
            format!("<code>{}</code>", esc(&value.to_string()))
        } else if let Some(items) = s.get("items") {
            format!("{}[]", self.label(items))
        } else if s.get("oneOf").is_some() {
            "one of".to_owned()
        } else if s.get("anyOf").is_some() {
            "any of".to_owned()
        } else if let (None, Some(values @ Value::Object(_))) =
            (s.get("properties"), s.get("additionalProperties"))
        {
            format!("map&lt;string, {}&gt;", self.label(values))
        } else if !types.is_empty() {
            esc(&types.join(" | "))
        } else if s.get("properties").is_some() {
            "object".to_owned()
        } else {
            "any".to_owned()
        };
        if let (None, Some(format)) = (resolved.model, text(s, "/format")) {
            label.push_str(&format!(" · {}", esc(format)));
        }
        if nullable {
            label.push_str(" · nullable");
        }
        label
    }

    fn resolve(&self, mut schema: &'a Value) -> Resolved<'a> {
        let mut found = Resolved {
            schema,
            model: None,
            description: None,
            nullable: false,
        };
        // A reference chain is short; the bound keeps a cyclic one from looping.
        for _ in 0..8 {
            if found.model.is_none() {
                found.description = found.description.or_else(|| text(schema, "/description"));
            }
            if let [only] = array(schema, "/allOf") {
                schema = only;
            } else if let [a, b] = array(schema, "/anyOf") {
                if !is_null(a) && !is_null(b) {
                    break;
                }
                found.nullable = true;
                schema = if is_null(a) { b } else { a };
            } else if let Some(model) =
                text(schema, "/$ref").and_then(|r| r.strip_prefix("#/components/schemas/"))
            {
                found.model = found.model.or(Some(model));
                schema = self.schemas.get(model).unwrap_or(&NONE);
            } else {
                break;
            }
        }
        found.description = found.description.or_else(|| text(schema, "/description"));
        found.schema = schema;
        found
    }

    fn description(&self, description: Option<&str>) -> anyhow::Result<String> {
        Ok(match description {
            Some(text) => format!(r#"<div class="pdesc">{}</div>"#, self.markdown(text, 3)?),
            None => String::new(),
        })
    }

    /// Markdown as HTML, its headings `shift` levels down and its links into
    /// `docs/api/` pointed at their section of the page.
    fn markdown(&self, markdown: &str, shift: usize) -> anyhow::Result<String> {
        let events = Parser::new_ext(markdown, Options::ENABLE_TABLES)
            .map(|event| self.event(event, shift))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let mut out = String::new();
        html::push_html(&mut out, events.into_iter());
        Ok(out)
    }

    fn event<'e>(&self, mut event: Event<'e>, shift: usize) -> anyhow::Result<Event<'e>> {
        if let Event::Start(Tag::Link { dest_url, .. }) = &mut event {
            *dest_url = self.link(std::mem::replace(dest_url, CowStr::Borrowed("")))?;
        }
        if let Event::Start(Tag::Heading { level, .. }) | Event::End(TagEnd::Heading(level)) =
            &mut event
        {
            *level = lower(*level, shift);
        }
        // The docs are ours, but raw HTML still shows as text rather than runs.
        if let Event::Html(raw) | Event::InlineHtml(raw) = event {
            return Ok(Event::Text(raw));
        }
        Ok(event)
    }

    /// A link to a route's or a tag's section, as the document writes it, must
    /// name a section of the page. Any other link stays as it is.
    fn link<'e>(&self, url: CowStr<'e>) -> anyhow::Result<CowStr<'e>> {
        if let Some(section) = url.strip_prefix('#').filter(|s| s.starts_with("tag/"))
            && !self.anchors.contains(section)
        {
            anyhow::bail!("no section of the page for #{section}");
        }
        Ok(url)
    }
}

/// A field's default and allowed values, from the schema as written or resolved.
fn facts(schema: &Value, resolved: &Value) -> String {
    let mut facts = Vec::new();
    if let Some(default) = schema.get("default").or_else(|| resolved.get("default")) {
        facts.push(format!(
            "default <code>{}</code>",
            esc(&default.to_string())
        ));
    }
    let values: Vec<_> = array(resolved, "/enum")
        .iter()
        .map(|v| format!("<code>{}</code>", esc(&v.to_string())))
        .collect();
    if !values.is_empty() {
        facts.push(format!("one of {}", values.join(", ")));
    }
    if facts.is_empty() {
        String::new()
    } else {
        format!(r#"<div class="facts">{}</div>"#, facts.join(" · "))
    }
}

fn lower(level: HeadingLevel, shift: usize) -> HeadingLevel {
    HeadingLevel::try_from(level as usize + shift).unwrap_or(HeadingLevel::H6)
}

fn is_null(schema: &Value) -> bool {
    text(schema, "/type") == Some("null")
}

fn text<'v>(value: &'v Value, pointer: &str) -> Option<&'v str> {
    value.pointer(pointer).and_then(Value::as_str)
}

fn array<'v>(value: &'v Value, pointer: &str) -> &'v [Value] {
    value
        .pointer(pointer)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn entries<'v>(value: &'v Value, pointer: &str) -> impl Iterator<Item = (&'v String, &'v Value)> {
    value
        .pointer(pointer)
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
}

fn object<'v>(value: &'v Value, pointer: &str) -> anyhow::Result<&'v Map<String, Value>> {
    value
        .pointer(pointer)
        .and_then(Value::as_object)
        .with_context(|| format!("the document has no {pointer}"))
}

fn esc(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::super::OpenApi;
    use super::*;

    fn page(openapi: &OpenApi) -> anyhow::Result<Page<'_>> {
        Page::new(openapi.document())
    }

    /// The model's part of the page, from its id to the next model.
    fn model<'h>(html: &'h str, name: &str) -> anyhow::Result<&'h str> {
        let (_, rest) = html
            .split_once(&format!(r#"id="model/{name}""#))
            .context("no model")?;
        Ok(rest
            .split_once(r#"<details class="model">"#)
            .map_or(rest, |(part, _)| part))
    }

    enum Link {
        Section,
        Unchanged,
        Fails,
    }

    #[rstest]
    #[case::a_route("#tag/plans/get_plan", Link::Section)]
    #[case::a_tag("#tag/plans", Link::Section)]
    #[case::a_slug_unlike_its_name("#tag/model-params", Link::Section)]
    #[case::the_web("https://example.test/x", Link::Unchanged)]
    #[case::another_fragment("#auth/userSignature", Link::Unchanged)]
    #[case::no_such_route("#tag/plans/no_such_route", Link::Fails)]
    #[case::another_tags_route("#tag/users/get_plan", Link::Fails)]
    #[case::no_such_tag("#tag/nope", Link::Fails)]
    fn test_a_section_link(#[case] link: &str, #[case] expected: Link) -> anyhow::Result<()> {
        let openapi = OpenApi::build()?;
        let out = page(&openapi)?.markdown(&format!("[x]({link})"), 0);
        match expected {
            Link::Section => {
                assert_eq!(out?.trim(), format!(r##"<p><a href="{link}">x</a></p>"##));
            }
            Link::Unchanged => assert!(out?.contains(&format!(r#"href="{link}""#))),
            Link::Fails => assert!(out.is_err()),
        }
        Ok(())
    }

    #[test]
    fn test_raw_html_and_code_show_as_text() -> anyhow::Result<()> {
        let openapi = OpenApi::build()?;
        let page = page(&openapi)?;
        let out = page.markdown("a <script>x</script>\n\n<div>b</div>\n", 0)?;
        assert!(!out.contains("<script>") && !out.contains("<div>"), "{out}");
        let code = "`[x](#tag/plans/nope)`";
        assert!(page.markdown(code, 0)?.contains("nope"));
        Ok(())
    }

    #[test]
    fn test_the_page_has_each_route_and_no_repository_link() -> anyhow::Result<()> {
        let openapi = OpenApi::build()?;
        let [(_, html), (_, css)] = files(openapi.document())?;
        let routes = object(openapi.document(), "/paths")?;
        routes
            .values()
            .filter_map(Value::as_object)
            .flatten()
            .try_for_each(|(_, operation)| {
                let id = text(operation, "/operationId").context("an operation has no id")?;
                let dir = doc_dir(id).context("no file")?;
                assert!(html.contains(&format!(r#"id="tag/{dir}/{id}""#)), "{id}");
                anyhow::Ok(())
            })?;
        assert!(!html.contains("github.com/Liquid4All"));
        assert!(!html.contains("<script") && !css.is_empty());
        assert!(html.contains(r#"id="auth/userSignature""#));
        // A route's examples sit in its card, ahead of its responses.
        let (_, create) = html
            .split_once(r#"id="tag/campaigns/create_campaign""#)
            .context("no create_campaign")?;
        let (before, _) = create
            .split_once(r#"<h4 class="sub">Responses"#)
            .context("no responses")?;
        let (_, card) = before
            .split_once(r#"<aside class="ex">"#)
            .context("no card")?;
        assert!(card.contains("<pre>"), "{card}");
        Ok(())
    }

    #[test]
    fn test_a_model_shows_its_fields_arms_and_tags() -> anyhow::Result<()> {
        let openapi = OpenApi::build()?;
        let [(_, html), _] = files(openapi.document())?;
        // Fields beside a oneOf: the arms still show.
        let ovms = model(&html, "Ovms")?;
        assert!(
            ovms.contains(">flavor<") && ovms.contains(">repository_url<"),
            "{ovms}"
        );
        // A tagged arm flattening one model: its tag field still shows.
        let runtime = openapi
            .document()
            .pointer("/components/schemas/Runtime")
            .context("Runtime")?;
        let arms = array(runtime, "/oneOf").len();
        let tags = model(&html, "Runtime")?
            .matches(r#"<code class="pname">type</code>"#)
            .count();
        assert!(
            arms > 0 && tags >= arms,
            "{tags} tag fields for {arms} arms"
        );
        Ok(())
    }

    #[test]
    fn test_a_model_that_holds_itself_renders_once() -> anyhow::Result<()> {
        let id = "list_plans";
        let document = json!({
            "info": {"title": "t"},
            "tags": [{"name": "Plans"}],
            "paths": {"/p": {"get": {"operationId": id, "tags": ["Plans"], "responses": {
                "200": {"description": "d", "content": {"application/json": {"schema": {"$ref": "#/components/schemas/A"}}}}
            }}}},
            "components": {"responses": {}, "schemas": {
                "A": {"type": "array", "items": {"oneOf": [{"$ref": "#/components/schemas/A"}, {"type": "string"}]}}
            }},
        });
        let [(_, html), _] = files(&document)?;
        assert!(html.contains(r#"id="model/A""#));
        Ok(())
    }
}
