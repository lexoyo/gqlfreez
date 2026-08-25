//! Walking Relay connections to the end, and merging the pages.
//!
//! Driven by the query AST, never by scanning the response: `pageInfo` only appears in a
//! response if it was asked for, so scanning cannot catch the naive
//! `posts(first: 2000) { nodes { title } }` that a server silently truncates.

use std::path::Path;

use serde_json::{Map, Value};

use crate::errors::Error;
use crate::fetch;
use crate::options::Options;
use crate::query::{Analysis, Connection};

/// Run one query file to completion and return the merged `data`.
pub async fn run(
    client: &reqwest::Client,
    options: &Options,
    file: &Path,
    analysis: &Analysis,
) -> Result<(Value, usize), Error> {
    // Page 1 is the user's query, with whatever pagination plumbing it was missing.
    // Page 1 starts every cursor at null, whether the variable is the user's or ours.
    let mut variables = Map::new();
    for (index, _) in analysis.paginated() {
        variables.insert(analysis.cursor_var(index), Value::Null);
    }

    let first_query = analysis.page_one(options.page_size);
    let mut merged = fetch::execute(
        client,
        options,
        file,
        &first_query,
        analysis.operation_name.as_deref(),
        variables.clone(),
    )
    .await?;
    let mut pages = 1usize;

    for (index, conn) in analysis.paginated() {
        pages += walk_connection(client, options, file, analysis, index, conn, &mut merged).await?;
    }

    // Strip what we injected: the user never asked for it.
    for (_, conn) in analysis.paginated() {
        if conn.page_info_key.is_none() {
            if let Some(node) = follow_mut(&mut merged, &conn.path, file)? {
                if let Some(map) = node.as_object_mut() {
                    map.remove("pageInfo");
                }
            }
        }
    }

    Ok((merged, pages))
}

/// Follow one connection to the end, appending into `merged`.
async fn walk_connection(
    client: &reqwest::Client,
    options: &Options,
    file: &Path,
    analysis: &Analysis,
    index: usize,
    conn: &Connection,
    merged: &mut Value,
) -> Result<usize, Error> {
    let cursor_var = analysis.cursor_var(index);
    let page_info_key = conn.page_info_key.as_deref().unwrap_or("pageInfo");

    let Some(node) = follow_mut(merged, &conn.path, file)? else {
        return Ok(0);
    };
    let Some(state) = read_page_info(node, page_info_key) else {
        // No pageInfo in the response: the field was not the connection we took it for.
        return Ok(0);
    };
    let (mut has_next, mut cursor) = state;
    let start_cursor = node
        .get(page_info_key)
        .and_then(|p| p.get("startCursor"))
        .cloned();
    let has_previous = node
        .get(page_info_key)
        .and_then(|p| p.get("hasPreviousPage"))
        .cloned();

    let mut extra_pages = 0usize;
    let mut seen_cursors: Vec<String> = vec![];

    while has_next {
        let Some(current) = cursor.clone() else {
            return Err(Error::GraphQl {
                path: file.to_owned(),
                message: format!(
                    "{}: the server says there is a next page but gives no endCursor",
                    conn.path.join(".")
                ),
            });
        };

        // A server that ignores the cursor loops forever; catch it on the second call
        // rather than after --max-pages requests.
        if seen_cursors.contains(&current) {
            return Err(Error::GraphQl {
                path: file.to_owned(),
                message: format!(
                    "{}: the server returned the same endCursor twice ({current:?}) — \
                     it is ignoring `after:`, so pagination cannot advance",
                    conn.path.join(".")
                ),
            });
        }
        seen_cursors.push(current.clone());

        if extra_pages + 1 >= options.max_pages {
            return Err(Error::GraphQl {
                path: file.to_owned(),
                message: format!(
                    "{}: stopped at --max-pages ({}). Raise it, or narrow the query.",
                    conn.path.join("."),
                    options.max_pages
                ),
            });
        }

        if !options.delay.is_zero() {
            tokio::time::sleep(options.delay).await;
        }

        let mut variables = Map::new();
        variables.insert(cursor_var.clone(), Value::String(current));
        let derived = analysis.derived(index, options.page_size);
        let page =
            fetch::execute(client, options, file, &derived, Some("gqlfreez"), variables).await?;
        extra_pages += 1;

        let mut page = page;
        let Some(page_node) = follow_mut(&mut page, &conn.path, file)? else {
            break;
        };
        let next = read_page_info(page_node, page_info_key);

        // Append this page's items into the skeleton kept from page 1.
        let target = follow_mut(merged, &conn.path, file)?.expect("checked above");
        for (_, key) in &conn.lists {
            let items = match page_node.get_mut(key).map(Value::take) {
                Some(Value::Array(items)) => items,
                // Never invent a key the response did not have.
                _ => continue,
            };
            if let Some(Value::Array(existing)) = target.get_mut(key) {
                existing.extend(items);
            }
        }

        match next {
            Some((n, c)) => {
                has_next = n;
                cursor = c;
            }
            None => break,
        }
    }

    // Rebuild a pageInfo that describes the merged result. `hasNextPage` is copied from the
    // last page actually fetched — never forced to false, which would be a lie.
    if let Some(node) = follow_mut(merged, &conn.path, file)? {
        if let Some(map) = node.as_object_mut() {
            if let Some(Value::Object(info)) = map.get_mut(page_info_key) {
                info.insert("hasNextPage".into(), Value::Bool(has_next));
                if let Some(c) = cursor {
                    info.insert("endCursor".into(), Value::String(c));
                }
                if let Some(s) = start_cursor {
                    info.insert("startCursor".into(), s);
                }
                if let Some(p) = has_previous {
                    info.insert("hasPreviousPage".into(), p);
                }
            }
        }
    }

    Ok(extra_pages)
}

fn read_page_info(node: &Value, key: &str) -> Option<(bool, Option<String>)> {
    let info = node.get(key)?;
    let has_next = info.get("hasNextPage")?.as_bool().unwrap_or(false);
    let cursor = info
        .get("endCursor")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some((has_next, cursor))
}

/// Walk a response path. A list in the middle means the connection is not addressable.
fn follow_mut<'a>(
    root: &'a mut Value,
    path: &[String],
    file: &Path,
) -> Result<Option<&'a mut Value>, Error> {
    let mut current = root;
    for (depth, key) in path.iter().enumerate() {
        if current.is_array() {
            return Err(Error::GraphQl {
                path: file.to_owned(),
                message: format!(
                    "{}: a list sits on the path to this connection — gqlfreez cannot paginate \
                     a connection nested inside a list",
                    path[..depth].join(".")
                ),
            });
        }
        match current.get_mut(key) {
            Some(next) => current = next,
            None => return Ok(None),
        }
    }
    Ok(Some(current))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn follow_reaches_a_nested_node() {
        let mut v = json!({"repository": {"issues": {"nodes": [1]}}});
        let n = follow_mut(
            &mut v,
            &["repository".into(), "issues".into()],
            Path::new("t"),
        )
        .unwrap();
        assert!(n.unwrap().get("nodes").is_some());
    }

    #[test]
    fn follow_refuses_a_list_on_the_path() {
        let mut v = json!({"authors": [{"books": {"nodes": []}}]});
        let e =
            follow_mut(&mut v, &["authors".into(), "books".into()], Path::new("t")).unwrap_err();
        assert!(format!("{e}").contains("a list sits on the path"), "{e}");
    }

    #[test]
    fn page_info_is_read_through_its_alias() {
        let v = json!({"info": {"hasNextPage": true, "endCursor": "c1"}});
        assert_eq!(read_page_info(&v, "info"), Some((true, Some("c1".into()))));
        assert_eq!(read_page_info(&v, "pageInfo"), None);
    }
}
