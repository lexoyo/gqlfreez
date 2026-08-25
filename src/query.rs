//! Parsing and rewriting of GraphQL query files.
//!
//! Rewriting is done by textual insertion on a lossless CST: we locate the insertion points
//! and edit the original source. Comments, formatting and spacing are preserved.

use apollo_parser::cst::{self, CstNode};
use apollo_parser::Parser;
use std::path::Path;

use crate::errors::Error;

pub const AUTO: &str = "auto";
pub const CURSOR_VAR: &str = "gqlfreez_cursor";

/// What the document holds.
#[derive(Debug, PartialEq)]
pub enum Kind {
    Operation,
    /// Fragments only, no operation: not a query, skipped.
    FragmentsOnly,
    Empty,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ListField {
    Nodes,
    Edges,
}

/// A Relay connection found in the query: a field whose selection holds `nodes` or `edges`.
#[derive(Debug, Clone)]
pub struct Connection {
    /// Response path, following aliases: `["repository", "issues"]`.
    pub path: Vec<String>,
    /// Response keys of the list fields present, following aliases.
    pub lists: Vec<(ListField, String)>,
    /// Response key of `pageInfo`, if the user selected it.
    pub page_info_key: Option<String>,
    /// True when this connection must be walked to the end.
    pub paginate: bool,
    /// Byte offsets of the whole field, in the original source.
    pub field_span: (usize, usize),
    /// Insertion point and existing arguments.
    args: Args,
    /// Byte offsets of the field's selection set.
    selection_span: (usize, usize),
    /// Ancestors' header text, outermost first: `repository(owner: "x", name: "y")`.
    ancestors: Vec<String>,
    /// Existing `after:` argument value, when the user wrote one.
    pub user_cursor_var: Option<String>,
}

#[derive(Debug, Clone)]
struct Args {
    /// Offset where `(…)` should be opened when the field has no arguments.
    open_at: usize,
    /// Offset of the closing paren when arguments exist.
    close_at: Option<usize>,
    /// Span of a `first: auto` value, to be replaced by the page size.
    auto_span: Option<(usize, usize)>,
    has_limit: bool,
}

#[derive(Debug)]
pub struct Analysis {
    pub kind: Kind,
    pub operation_name: Option<String>,
    pub connections: Vec<Connection>,
    /// Where to declare the cursor variable, and how.
    var_insert: Option<VarInsert>,
    source: String,
}

#[derive(Debug, Clone)]
enum VarInsert {
    /// Operation already declares variables: insert before the closing paren.
    Extend(usize),
    /// Operation has a keyword but no variables: insert after it.
    After(usize),
    /// Shorthand `{ … }`: replace the opening brace.
    Shorthand(usize),
}

pub fn analyze(source: &str, path: &Path) -> Result<Analysis, Error> {
    let cst = Parser::new(source).parse();

    // apollo-parser is error-resilient and always returns a tree: without this check a file
    // with a stray brace would look like "no operation" and be skipped in silence.
    if let Some(err) = cst.errors().next() {
        return Err(Error::Query {
            path: path.to_owned(),
            message: format!("syntax error at byte {}: {}", err.index(), err.message()),
        });
    }

    let doc = cst.document();
    let mut operations = vec![];
    let mut fragments = 0usize;
    for def in doc.definitions() {
        match def {
            cst::Definition::OperationDefinition(op) => operations.push(op),
            cst::Definition::FragmentDefinition(_) => fragments += 1,
            _ => {}
        }
    }

    if operations.is_empty() {
        return Ok(Analysis {
            kind: if fragments > 0 {
                Kind::FragmentsOnly
            } else {
                Kind::Empty
            },
            operation_name: None,
            connections: vec![],
            var_insert: None,
            source: source.to_owned(),
        });
    }
    if operations.len() > 1 {
        let names: Vec<String> = operations
            .iter()
            .map(|o| {
                o.name()
                    .map(|n| n.text().to_string())
                    .unwrap_or_else(|| "<anonymous>".into())
            })
            .collect();
        return Err(Error::Query {
            path: path.to_owned(),
            message: format!(
                "this file holds {} operations ({}) — gqlfreez expects one operation per file",
                operations.len(),
                names.join(", ")
            ),
        });
    }

    let op = &operations[0];
    let operation_name = op.name().map(|n| n.text().to_string());

    let selection_set = op.selection_set().ok_or_else(|| Error::Query {
        path: path.to_owned(),
        message: "the operation has no selection set".into(),
    })?;

    let mut connections = vec![];
    walk(
        &selection_set,
        &mut vec![],
        &mut vec![],
        &mut connections,
        source,
        path,
    )?;

    let var_insert = if let Some(vars) = op.variable_definitions() {
        let r = vars.syntax().text_range();
        Some(VarInsert::Extend(usize::from(r.end()) - 1))
    } else if let Some(ty) = op.operation_type() {
        let end = if let Some(name) = op.name() {
            usize::from(name.syntax().text_range().end())
        } else {
            usize::from(ty.syntax().text_range().end())
        };
        Some(VarInsert::After(end))
    } else {
        Some(VarInsert::Shorthand(usize::from(
            selection_set.syntax().text_range().start(),
        )))
    };

    Ok(Analysis {
        kind: Kind::Operation,
        operation_name,
        connections,
        var_insert,
        source: source.to_owned(),
    })
}

fn walk(
    set: &cst::SelectionSet,
    path: &mut Vec<String>,
    ancestors: &mut Vec<String>,
    out: &mut Vec<Connection>,
    source: &str,
    file: &Path,
) -> Result<(), Error> {
    for sel in set.selections() {
        let field = match sel {
            cst::Selection::Field(f) => f,
            cst::Selection::FragmentSpread(s) => {
                let name = s
                    .fragment_name()
                    .and_then(|n| n.name())
                    .map(|n| n.text().to_string())
                    .unwrap_or_default();
                return Err(Error::Query {
                    path: file.to_owned(),
                    message: format!(
                        "fragment spread `...{name}` — gqlfreez cannot yet read a connection \
                         through a fragment; inline it for now"
                    ),
                });
            }
            cst::Selection::InlineFragment(_) => {
                return Err(Error::Query {
                    path: file.to_owned(),
                    message: "inline fragment — gqlfreez cannot yet read a connection through a \
                              fragment; inline it for now"
                        .into(),
                })
            }
        };

        let Some(name) = field.name() else { continue };
        let schema_name = name.text().to_string();
        let response_key = field
            .alias()
            .and_then(|a| a.name())
            .map(|n| n.text().to_string())
            .unwrap_or_else(|| schema_name.clone());

        let Some(inner) = field.selection_set() else {
            continue;
        };

        // Direct children, by schema name, keeping the response key.
        let mut lists = vec![];
        let mut page_info_key = None;
        for child in inner.selections() {
            if let cst::Selection::Field(cf) = child {
                let Some(cn) = cf.name() else { continue };
                let key = cf
                    .alias()
                    .and_then(|a| a.name())
                    .map(|n| n.text().to_string())
                    .unwrap_or_else(|| cn.text().to_string());
                match cn.text().as_ref() {
                    "nodes" => lists.push((ListField::Nodes, key)),
                    "edges" => lists.push((ListField::Edges, key)),
                    "pageInfo" => page_info_key = Some(key),
                    _ => {}
                }
            }
        }

        path.push(response_key.clone());

        if lists.is_empty() {
            // Not a connection: keep walking down, carrying this field as an ancestor.
            ancestors.push(field_header(&field, source));
            walk(&inner, path, ancestors, out, source, file)?;
            ancestors.pop();
        } else {
            let args = read_args(&field, &name)?;
            let paginate = !args.has_limit || args.auto_span.is_some();
            let user_cursor_var = read_after_var(&field);
            let fr = field.syntax().text_range();
            let sr = inner.syntax().text_range();
            out.push(Connection {
                path: path.clone(),
                lists,
                page_info_key,
                paginate: paginate || user_cursor_var.is_some(),
                field_span: (usize::from(fr.start()), usize::from(fr.end())),
                args,
                selection_span: (usize::from(sr.start()), usize::from(sr.end())),
                ancestors: ancestors.clone(),
                user_cursor_var,
            });
        }

        path.pop();
    }
    Ok(())
}

/// The field text up to (but excluding) its selection set: `repository(owner: "x")`.
fn field_header(field: &cst::Field, source: &str) -> String {
    let r = field.syntax().text_range();
    let end = match field.selection_set() {
        Some(ss) => usize::from(ss.syntax().text_range().start()),
        None => usize::from(r.end()),
    };
    source[usize::from(r.start())..end].trim_end().to_string()
}

fn read_args(field: &cst::Field, name: &cst::Name) -> Result<Args, Error> {
    let mut args = Args {
        open_at: usize::from(name.syntax().text_range().end()),
        close_at: None,
        auto_span: None,
        has_limit: false,
    };
    if let Some(list) = field.arguments() {
        let r = list.syntax().text_range();
        args.close_at = Some(usize::from(r.end()) - 1);
        for a in list.arguments() {
            let Some(n) = a.name() else { continue };
            let key = n.text().to_string();
            if key == "first" || key == "last" {
                args.has_limit = true;
                if let Some(v) = a.value() {
                    let txt = v.syntax().text().to_string();
                    if txt.trim() == AUTO {
                        let vr = v.syntax().text_range();
                        args.auto_span = Some((usize::from(vr.start()), usize::from(vr.end())));
                    }
                }
            }
        }
    }
    Ok(args)
}

fn read_after_var(field: &cst::Field) -> Option<String> {
    let list = field.arguments()?;
    for a in list.arguments() {
        if a.name().map(|n| n.text().to_string()).as_deref() == Some("after") {
            if let Some(cst::Value::Variable(v)) = a.value() {
                return v.name().map(|n| n.text().to_string());
            }
        }
    }
    None
}

/// One textual edit on the source.
#[derive(Debug, Clone)]
struct Edit {
    at: (usize, usize),
    text: String,
}

impl Analysis {
    pub fn paginated(&self) -> impl Iterator<Item = (usize, &Connection)> {
        self.connections
            .iter()
            .enumerate()
            .filter(|(_, c)| c.paginate)
    }

    /// Whether the cursor variable name is free in this document.
    fn cursor_var_for(&self, index: usize, conn: &Connection) -> String {
        match &conn.user_cursor_var {
            Some(v) => v.clone(),
            None => {
                let mut name = format!("{CURSOR_VAR}_{index}");
                while self.source.contains(&format!("${name}")) {
                    name.push('_');
                }
                name
            }
        }
    }

    /// The document to send for page 1: the original source, with what is needed to paginate.
    pub fn page_one(&self, page_size: usize) -> String {
        let mut edits = vec![];
        let mut needs_vars = vec![];

        for (i, conn) in self.connections.iter().enumerate() {
            if !conn.paginate {
                continue;
            }
            let var = self.cursor_var_for(i, conn);
            if conn.user_cursor_var.is_none() {
                needs_vars.push(var.clone());
            }
            self.connection_edits(conn, &var, page_size, &mut edits);
        }

        if !needs_vars.is_empty() {
            let decls: Vec<String> = needs_vars.iter().map(|v| format!("${v}: String")).collect();
            match self.var_insert {
                Some(VarInsert::Extend(at)) => edits.push(Edit {
                    at: (at, at),
                    text: format!(", {}", decls.join(", ")),
                }),
                Some(VarInsert::After(at)) => edits.push(Edit {
                    at: (at, at),
                    text: format!("({})", decls.join(", ")),
                }),
                Some(VarInsert::Shorthand(at)) => edits.push(Edit {
                    at: (at, at),
                    text: format!("query gqlfreez({}) ", decls.join(", ")),
                }),
                None => {}
            }
        }

        apply(&self.source, edits)
    }

    fn connection_edits(
        &self,
        conn: &Connection,
        var: &str,
        page_size: usize,
        edits: &mut Vec<Edit>,
    ) {
        // `first: auto` → `first: <page_size>`
        if let Some(span) = conn.args.auto_span {
            edits.push(Edit {
                at: span,
                text: page_size.to_string(),
            });
        }

        let mut additions: Vec<String> = vec![];
        if !conn.args.has_limit {
            additions.push(format!("first: {page_size}"));
        }
        if conn.user_cursor_var.is_none() {
            additions.push(format!("after: ${var}"));
        }
        if !additions.is_empty() {
            match conn.args.close_at {
                Some(close) => edits.push(Edit {
                    at: (close, close),
                    text: format!(", {}", additions.join(", ")),
                }),
                None => edits.push(Edit {
                    at: (conn.args.open_at, conn.args.open_at),
                    text: format!("({})", additions.join(", ")),
                }),
            }
        }

        // Inject pageInfo when the user did not ask for it; it is stripped from the output.
        if conn.page_info_key.is_none() {
            let open = conn.selection_span.0 + 1;
            edits.push(Edit {
                at: (open, open),
                text: " pageInfo { hasNextPage endCursor }".into(),
            });
        }
    }

    /// A derived query holding only this connection (and its ancestors' path).
    ///
    /// Sidesteps unused variables, empty selection sets, and re-fetching unrelated fields
    /// on every page.
    pub fn derived(&self, index: usize, page_size: usize) -> String {
        let conn = &self.connections[index];
        let var = self.cursor_var_for(index, conn);

        let mut edits = vec![];
        self.connection_edits(conn, &var, page_size, &mut edits);
        let field = apply_within(&self.source, conn.field_span, edits);

        let mut body = field;
        for header in conn.ancestors.iter().rev() {
            body = format!("{header} {{ {body} }}");
        }
        format!("query gqlfreez(${var}: String) {{ {body} }}")
    }

    pub fn cursor_var(&self, index: usize) -> String {
        self.cursor_var_for(index, &self.connections[index])
    }
}

fn apply(source: &str, mut edits: Vec<Edit>) -> String {
    edits.sort_by_key(|e| std::cmp::Reverse(e.at.0));
    let mut out = source.to_string();
    for e in edits {
        out.replace_range(e.at.0..e.at.1, &e.text);
    }
    out
}

fn apply_within(source: &str, span: (usize, usize), edits: Vec<Edit>) -> String {
    let shifted: Vec<Edit> = edits
        .into_iter()
        .map(|e| Edit {
            at: (e.at.0 - span.0, e.at.1 - span.0),
            text: e.text,
        })
        .collect();
    apply(&source[span.0..span.1], shifted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn an(src: &str) -> Analysis {
        analyze(src, Path::new("t.graphql")).expect("analyze")
    }

    #[test]
    fn no_limit_paginates() {
        let a = an("{ posts { nodes { title } } }");
        assert_eq!(a.connections.len(), 1);
        assert!(a.connections[0].paginate);
        assert_eq!(
            a.page_one(100),
            "query gqlfreez($gqlfreez_cursor_0: String) { posts(first: 100, after: $gqlfreez_cursor_0) { pageInfo { hasNextPage endCursor } nodes { title } } }"
        );
    }

    #[test]
    fn explicit_first_is_a_limit() {
        let a = an("{ posts(first: 3) { nodes { title } pageInfo { hasNextPage } } }");
        assert!(!a.connections[0].paginate);
        assert_eq!(
            a.page_one(100),
            "{ posts(first: 3) { nodes { title } pageInfo { hasNextPage } } }"
        );
    }

    #[test]
    fn auto_marker_paginates_and_is_replaced() {
        let a = an("{ posts(first: auto) { nodes { title } } }");
        assert!(a.connections[0].paginate);
        let q = a.page_one(50);
        assert!(q.contains("first: 50"), "{q}");
        assert!(q.contains("after: $gqlfreez_cursor_0"), "{q}");
        assert!(!q.contains("auto"), "{q}");
    }

    #[test]
    fn user_cursor_is_reused_untouched() {
        let src = "query P($cursor: String) { posts(first: 100, after: $cursor) { nodes { t } pageInfo { hasNextPage endCursor } } }";
        let a = an(src);
        assert!(a.connections[0].paginate);
        assert_eq!(a.connections[0].user_cursor_var.as_deref(), Some("cursor"));
        assert_eq!(a.page_one(100), src, "nothing to rewrite");
    }

    #[test]
    fn existing_variables_are_extended_not_replaced() {
        let a = an("query P($lang: String) { posts { nodes { t } } }");
        let q = a.page_one(100);
        assert!(
            q.starts_with("query P($lang: String, $gqlfreez_cursor_0: String)"),
            "{q}"
        );
    }

    #[test]
    fn anonymous_operation_with_keyword() {
        let a = an("query { posts { nodes { t } } }");
        let q = a.page_one(100);
        assert!(q.starts_with("query($gqlfreez_cursor_0: String)"), "{q}");
    }

    #[test]
    fn aliases_are_followed() {
        let a = an(
            "{ myPosts: posts { items: nodes { t } info: pageInfo { hasNextPage endCursor } } }",
        );
        let c = &a.connections[0];
        assert_eq!(c.path, vec!["myPosts"]);
        assert_eq!(c.lists[0].1, "items");
        assert_eq!(c.page_info_key.as_deref(), Some("info"));
    }

    #[test]
    fn nested_connection_keeps_ancestor_path() {
        let a = an(r#"{ repository(owner: "x", name: "y") { issues { nodes { title } } } }"#);
        let c = &a.connections[0];
        assert_eq!(c.path, vec!["repository", "issues"]);
        let d = a.derived(0, 100);
        assert!(d.contains(r#"repository(owner: "x", name: "y")"#), "{d}");
        assert!(
            d.contains("issues(first: 100, after: $gqlfreez_cursor_0)"),
            "{d}"
        );
        assert!(
            d.starts_with("query gqlfreez($gqlfreez_cursor_0: String)"),
            "{d}"
        );
    }

    #[test]
    fn two_connections_get_distinct_variables() {
        let a = an("{ posts { nodes { t } } pages { nodes { t } } }");
        assert_eq!(a.connections.len(), 2);
        let q = a.page_one(100);
        assert!(
            q.contains("$gqlfreez_cursor_0: String, $gqlfreez_cursor_1: String"),
            "{q}"
        );
        assert!(
            q.contains("posts(first: 100, after: $gqlfreez_cursor_0)"),
            "{q}"
        );
        assert!(
            q.contains("pages(first: 100, after: $gqlfreez_cursor_1)"),
            "{q}"
        );
    }

    #[test]
    fn derived_query_holds_only_its_connection() {
        let a = an("{ posts { nodes { t } } pages { nodes { t } } }");
        let d = a.derived(1, 100);
        assert!(d.contains("pages(first: 100"), "{d}");
        assert!(!d.contains("posts"), "{d}");
        assert!(d.contains("$gqlfreez_cursor_1"), "{d}");
    }

    #[test]
    fn field_without_nodes_is_not_a_connection() {
        let a = an("{ user { name } }");
        assert!(a.connections.is_empty());
    }

    #[test]
    fn directives_keep_arguments_before_them() {
        let a = an("{ posts @include(if: true) { nodes { t } } }");
        let q = a.page_one(100);
        assert!(
            q.contains("posts(first: 100, after: $gqlfreez_cursor_0) @include"),
            "{q}"
        );
    }

    #[test]
    fn syntax_error_is_reported_not_skipped() {
        let e = analyze("{ posts { nodes { t } } ", Path::new("t.graphql")).unwrap_err();
        assert!(format!("{e}").contains("syntax error"), "{e}");
    }

    #[test]
    fn fragments_only_is_not_an_operation() {
        let a = an("fragment F on Query { posts { nodes { t } } }");
        assert_eq!(a.kind, Kind::FragmentsOnly);
    }

    #[test]
    fn fragment_spread_is_refused() {
        let e = analyze("{ posts { ...Conn } }", Path::new("t.graphql")).unwrap_err();
        assert!(format!("{e}").contains("fragment"), "{e}");
    }

    #[test]
    fn several_operations_are_refused() {
        let e = analyze("query A { a } query B { b }", Path::new("t.graphql")).unwrap_err();
        assert!(format!("{e}").contains("2 operations"), "{e}");
    }

    #[test]
    fn cursor_name_collision_is_avoided() {
        let a = an("query P($gqlfreez_cursor_0: Int) { posts { nodes { t } } }");
        let q = a.page_one(100);
        assert!(q.contains("$gqlfreez_cursor_0_: String"), "{q}");
    }

    #[test]
    fn comments_and_formatting_survive() {
        let src = "# keep me\n{\n  posts {\n    nodes { title }\n  }\n}\n";
        let q = an(src).page_one(100);
        assert!(q.starts_with("# keep me\n"), "{q}");
        assert!(q.contains("\n  posts(first: 100"), "{q}");
    }
}
