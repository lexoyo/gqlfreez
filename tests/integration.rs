//! End-to-end tests against a stub GraphQL server.
//!
//! For a tool whose contract is "give me a directory, I hand you files", unit tests prove
//! almost nothing: what matters is what lands on disk.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};

#[derive(Clone)]
enum Behaviour {
    /// Three pages, then done.
    Paginated,
    /// Always claims another page, with the same cursor: a server ignoring `after:`.
    StuckCursor,
    Simple,
    /// HTTP 200 with an HTML body, like a WAF or a maintenance page.
    HtmlIn200,
    /// HTTP 200 carrying a GraphQL error.
    GraphQlError(&'static str),
    /// A rate limit in HTTP 200, then success.
    ThrottleThenOk,
    /// Always this `data`.
    Data(Value),
}

struct Stub {
    port: u16,
    calls: Arc<AtomicUsize>,
    queries: Arc<std::sync::Mutex<Vec<String>>>,
    /// The `Authorization` header of each request, empty when there was none.
    auth: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Stub {
    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}/graphql", self.port)
    }
}

fn start(behaviour: Behaviour) -> Stub {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let calls = Arc::new(AtomicUsize::new(0));
    let queries = Arc::new(std::sync::Mutex::new(Vec::new()));
    let auth = Arc::new(std::sync::Mutex::new(Vec::new()));
    let c = calls.clone();
    let q = queries.clone();
    let a = auth.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let n = c.fetch_add(1, Ordering::SeqCst);
            let behaviour = behaviour.clone();
            let q = q.clone();
            let a = a.clone();
            std::thread::spawn(move || handle(stream, behaviour, n, q, a));
        }
    });
    Stub {
        port,
        calls,
        queries,
        auth,
    }
}

fn handle(
    mut stream: TcpStream,
    behaviour: Behaviour,
    call: usize,
    queries: Arc<std::sync::Mutex<Vec<String>>>,
    auth: Arc<std::sync::Mutex<Vec<String>>>,
) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut length = 0usize;
    let mut authorization = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = v.trim().parse().unwrap_or(0);
        }
        if line.to_ascii_lowercase().starts_with("authorization:") {
            authorization = line["authorization:".len()..].trim().to_string();
        }
    }
    let mut body = vec![0u8; length];
    let _ = reader.read_exact(&mut body);
    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let query = request["query"].as_str().unwrap_or_default().to_string();
    let cursor = request["variables"]
        .as_object()
        .and_then(|v| v.values().find(|x| x.is_string()))
        .and_then(Value::as_str)
        .map(str::to_owned);
    queries.lock().unwrap().push(query);
    auth.lock().unwrap().push(authorization);

    let (status, content_type, payload) = match behaviour {
        Behaviour::HtmlIn200 => (
            "200 OK",
            "text/html; charset=utf-8",
            "<html><body>Under maintenance</body></html>".to_string(),
        ),
        Behaviour::GraphQlError(message) => (
            "200 OK",
            "application/json",
            json!({"errors": [{"message": message}]}).to_string(),
        ),
        Behaviour::ThrottleThenOk if call == 0 => (
            "200 OK",
            "application/json",
            json!({"errors": [{"message": "throttled", "extensions": {"code": "THROTTLED"}}]})
                .to_string(),
        ),
        Behaviour::Data(data) => (
            "200 OK",
            "application/json",
            json!({ "data": data }).to_string(),
        ),
        Behaviour::ThrottleThenOk | Behaviour::Simple => (
            "200 OK",
            "application/json",
            json!({"data": {"posts": {"nodes": [{"title": "one"}]}}}).to_string(),
        ),
        Behaviour::StuckCursor => (
            "200 OK",
            "application/json",
            json!({"data": {"posts": {
                "nodes": [{"title": "loop"}],
                "pageInfo": {"hasNextPage": true, "endCursor": "same"}
            }}})
            .to_string(),
        ),
        Behaviour::Paginated => {
            let (nodes, has_next, end) = match cursor.as_deref() {
                None => (vec!["a", "b"], true, "c1"),
                Some("c1") => (vec!["c", "d"], true, "c2"),
                _ => (vec!["e"], false, "c3"),
            };
            (
                "200 OK",
                "application/json",
                json!({"data": {"posts": {
                    "nodes": nodes.iter().map(|t| json!({"title": t})).collect::<Vec<_>>(),
                    "pageInfo": {"hasNextPage": has_next, "endCursor": end}
                }}})
                .to_string(),
            )
        }
    };

    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gqlfreez-it-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &Path, name: &str, body: &str) {
    let p = dir.join(name);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

fn gqlfreez(dir: &Path, endpoint: &str, extra: &[&str]) -> std::process::Output {
    gqlfreez_from_configs(dir, &[&["--endpoint", endpoint], extra].concat())
}

fn read_json(dir: &Path, name: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(dir.join(name)).unwrap()).unwrap()
}

#[test]
fn writes_data_next_to_the_query() {
    let dir = scratch("basic");
    let stub = start(Behaviour::Simple);
    write(
        &dir,
        "posts.graphql",
        "{ posts(first: 5) { nodes { title } } }",
    );
    let out = gqlfreez(&dir, &stub.endpoint(), &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        read_json(&dir, "posts.json"),
        json!({"posts": {"nodes": [{"title": "one"}]}})
    );
}

#[test]
fn walks_every_page_and_merges_nodes() {
    let dir = scratch("paginate");
    let stub = start(Behaviour::Paginated);
    write(&dir, "all.graphql", "{ posts { nodes { title } } }");
    let out = gqlfreez(&dir, &stub.endpoint(), &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let value = read_json(&dir, "all.json");
    let titles: Vec<&str> = value["posts"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, vec!["a", "b", "c", "d", "e"]);
    // pageInfo was injected by gqlfreez, so it must not leak into the output.
    assert!(value["posts"].get("pageInfo").is_none(), "{value}");
    assert_eq!(stub.calls.load(Ordering::SeqCst), 3);
}

#[test]
fn an_explicit_first_is_a_limit_not_a_page_size() {
    let dir = scratch("limit");
    let stub = start(Behaviour::Paginated);
    write(
        &dir,
        "few.graphql",
        "{ posts(first: 2) { nodes { title } } }",
    );
    let out = gqlfreez(&dir, &stub.endpoint(), &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(stub.calls.load(Ordering::SeqCst), 1, "must not paginate");
    assert!(!stub.queries.lock().unwrap()[0].contains("after"));
}

#[test]
fn a_user_written_page_info_is_kept_and_rebuilt() {
    let dir = scratch("keepinfo");
    let stub = start(Behaviour::Paginated);
    write(
        &dir,
        "all.graphql",
        "{ posts { nodes { title } pageInfo { hasNextPage endCursor } } }",
    );
    let out = gqlfreez(&dir, &stub.endpoint(), &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value = read_json(&dir, "all.json");
    assert_eq!(value["posts"]["pageInfo"]["hasNextPage"], json!(false));
    assert_eq!(value["posts"]["pageInfo"]["endCursor"], json!("c3"));
}

#[test]
fn a_stuck_cursor_is_caught_on_the_second_call() {
    let dir = scratch("stuck");
    let stub = start(Behaviour::StuckCursor);
    write(&dir, "loop.graphql", "{ posts { nodes { title } } }");
    let out = gqlfreez(&dir, &stub.endpoint(), &["--max-pages", "50"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("same endCursor twice"), "{err}");
    assert!(
        stub.calls.load(Ordering::SeqCst) <= 3,
        "must not loop 50 times"
    );
    assert!(
        !dir.join("loop.json").exists(),
        "nothing is written on failure"
    );
}

#[test]
fn html_in_a_200_says_what_came_back() {
    let dir = scratch("html");
    let stub = start(Behaviour::HtmlIn200);
    write(&dir, "q.graphql", "{ posts(first: 1) { nodes { title } } }");
    let out = gqlfreez(&dir, &stub.endpoint(), &["--retries", "0"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("expected JSON"), "{err}");
    assert!(err.contains("Under maintenance"), "{err}");
}

#[test]
fn a_graphql_error_fails_the_build_without_retrying() {
    let dir = scratch("gqlerr");
    let stub = start(Behaviour::GraphQlError("Cannot query field \"nope\""));
    write(&dir, "q.graphql", "{ posts(first: 1) { nodes { nope } } }");
    let out = gqlfreez(&dir, &stub.endpoint(), &[]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("Cannot query field"));
    assert_eq!(
        stub.calls.load(Ordering::SeqCst),
        1,
        "deterministic errors are not retried"
    );
}

#[test]
fn a_throttle_in_a_200_is_retried() {
    let dir = scratch("throttle");
    let stub = start(Behaviour::ThrottleThenOk);
    write(&dir, "q.graphql", "{ posts(first: 1) { nodes { title } } }");
    let out = gqlfreez(&dir, &stub.endpoint(), &[]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(stub.calls.load(Ordering::SeqCst), 2);
}

#[test]
fn unchanged_output_is_not_rewritten() {
    let dir = scratch("unchanged");
    let stub = start(Behaviour::Simple);
    write(&dir, "q.graphql", "{ posts(first: 1) { nodes { title } } }");
    assert!(gqlfreez(&dir, &stub.endpoint(), &[]).status.success());
    let first = std::fs::metadata(dir.join("q.json"))
        .unwrap()
        .modified()
        .unwrap();

    std::thread::sleep(std::time::Duration::from_millis(1100));
    let out = gqlfreez(&dir, &stub.endpoint(), &[]);
    assert!(out.status.success());
    let second = std::fs::metadata(dir.join("q.json"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        first, second,
        "the file must not be touched when nothing changed"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("1 unchanged"));
}

#[test]
fn check_reports_without_writing() {
    let dir = scratch("check");
    let stub = start(Behaviour::Simple);
    write(&dir, "q.graphql", "{ posts(first: 1) { nodes { title } } }");
    let out = gqlfreez(&dir, &stub.endpoint(), &["--check"]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!dir.join("q.json").exists());
}

#[test]
fn a_syntax_error_is_reported_not_skipped() {
    let dir = scratch("syntax");
    let stub = start(Behaviour::Simple);
    write(&dir, "broken.graphql", "{ posts { nodes { title } ");
    let out = gqlfreez(&dir, &stub.endpoint(), &[]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("syntax error"));
}

#[test]
fn envelope_restores_the_graphql_shape() {
    let dir = scratch("envelope");
    let stub = start(Behaviour::Simple);
    write(&dir, "q.graphql", "{ posts(first: 1) { nodes { title } } }");
    assert!(gqlfreez(&dir, &stub.endpoint(), &["--envelope"])
        .status
        .success());
    assert!(read_json(&dir, "q.json").get("data").is_some());
}

#[test]
fn no_matching_file_is_an_error() {
    let dir = scratch("empty");
    let stub = start(Behaviour::Simple);
    let out = gqlfreez(&dir, &stub.endpoint(), &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no query file"));
}

// Collections: one file per item of a list.

const ARTICLE_QUERY: &str = "{ posts(first: 10) { nodes { slug title tags author { name } } } }";

fn read(dir: &Path, rel: &str) -> String {
    std::fs::read_to_string(dir.join(rel)).unwrap()
}

fn posts(slugs: &[&str]) -> Value {
    json!({"posts": {"nodes": slugs.iter().map(|s| json!({
        "slug": s,
        "title": format!("About {s}"),
        "tags": ["a", null, "b"],
        "author": {"name": "Ada", "avatar": null},
    })).collect::<Vec<_>>()}})
}

/// A graphql-config with these collections and, when given, an endpoint and its token.
fn graphqlrc_json(collections: Value, endpoint: Option<(&str, &str)>) -> String {
    let mut extensions = json!({ "collections": collections });
    if let Some((url, token)) = endpoint {
        extensions["endpoints"] = json!({"default": {
            "url": url,
            "headers": {"Authorization": format!("Bearer {token}")}
        }});
    }
    serde_json::to_string_pretty(&json!({ "extensions": extensions })).unwrap()
}

/// A site with the queries under data/ and the pages under content/.
fn site(name: &str, collection: Value) -> PathBuf {
    let dir = scratch(name);
    write(&dir, "data/wp/article.graphql", ARTICLE_QUERY);
    write(
        &dir,
        "data/wp/.graphqlrc.json",
        &graphqlrc_json(json!({ "article.graphql": collection }), None),
    );
    write(
        &dir,
        "content/article/_index.md",
        "+++\ntitle = \"Blog\"\n+++\n",
    );
    dir
}

fn article_collection() -> Value {
    json!({
        "list": "posts.nodes",
        "dir": "../../content/article",
        "file": "{{item.slug}}.md",
        "frontMatter": {
            "template": "article.html",
            "path": "/blog/{{item.slug}}/",
            "extra": {"item": "{{item}}"}
        }
    })
}

fn code(out: &std::process::Output) -> (Option<i32>, String) {
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn ok(out: &std::process::Output) -> String {
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "{err}");
    err
}

fn pages(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir.join("content/article"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn collection_writes_one_page_per_item() {
    let dir = site("collection-basic", article_collection());
    let stub = start(Behaviour::Data(posts(&["hello", "world"])));
    let err = ok(&gqlfreez(&dir, &stub.endpoint(), &[]));
    assert!(err.contains("2 page(s) written, 0 removed"), "{err}");
    assert_eq!(pages(&dir), ["_index.md", "hello.md", "world.md"]);
    assert_eq!(
        read(&dir, "content/article/hello.md"),
        "+++\n\
         # generated by gqlfreez from ../../data/wp/article.graphql, do not edit\n\
         template = \"article.html\"\n\
         path = \"/blog/hello/\"\n\
         \n\
         [extra.item]\n\
         slug = \"hello\"\n\
         tags = [\"a\", \"b\"]\n\
         title = \"About hello\"\n\
         \n\
         [extra.item.author]\n\
         name = \"Ada\"\n\
         +++\n"
    );
    // The frozen JSON is still written, untouched by the collection.
    assert!(read_json(&dir, "data/wp/article.json")["posts"]["nodes"][0]["tags"][1].is_null());
}

#[test]
fn collection_in_yaml_drops_nulls_too() {
    let dir = scratch("collection-yaml");
    write(&dir, "data/wp/article.graphql", ARTICLE_QUERY);
    write(
        &dir,
        "data/wp/.graphqlrc.yml",
        "# comments are welcome here\n\
         extensions:\n\
         \x20 collections:\n\
         \x20   article.graphql:\n\
         \x20     list: posts.nodes\n\
         \x20     dir: ../../content/article\n\
         \x20     file: \"{{index}}.md\"\n\
         \x20     format: yaml\n\
         \x20     frontMatter:\n\
         \x20       title: \"{{item.title}}\"\n\
         \x20       author: \"{{ item.author }}\"\n",
    );
    let stub = start(Behaviour::Data(posts(&["hello"])));
    ok(&gqlfreez(&dir, &stub.endpoint(), &[]));
    assert_eq!(
        read(&dir, "content/article/0.md"),
        "---\n\
         # generated by gqlfreez from ../../data/wp/article.graphql, do not edit\n\
         title: 'About hello'\n\
         author:\n\
         \x20 name: 'Ada'\n\
         ---\n"
    );
}

#[test]
fn a_lone_slot_keeps_its_type_and_text_mixes_slots() {
    let mut collection = article_collection();
    collection["frontMatter"] = json!({
        "weight": "{{index}}",
        "label": "#{{index}}: {{item.title}}",
        "author": "{{ item.author }}"
    });
    let dir = site("collection-types", collection);
    let stub = start(Behaviour::Data(posts(&["hello"])));
    ok(&gqlfreez(&dir, &stub.endpoint(), &[]));
    assert_eq!(
        read(&dir, "content/article/hello.md"),
        "+++\n\
         # generated by gqlfreez from ../../data/wp/article.graphql, do not edit\n\
         weight = 0\n\
         label = \"#0: About hello\"\n\
         \n\
         [author]\n\
         name = \"Ada\"\n\
         +++\n"
    );
}

#[test]
fn collection_keeps_hand_written_files_and_removes_its_own() {
    let dir = site("collection-keep", article_collection());
    write(
        &dir,
        "content/article/notes.md",
        "+++\ntitle = \"by hand\"\n+++\n",
    );
    let stub = start(Behaviour::Data(posts(&["hello", "world"])));
    ok(&gqlfreez(&dir, &stub.endpoint(), &[]));

    let stub = start(Behaviour::Data(posts(&["hello"])));
    let err = ok(&gqlfreez(&dir, &stub.endpoint(), &[]));
    assert!(err.contains("0 page(s) written, 1 removed"), "{err}");
    assert_eq!(pages(&dir), ["_index.md", "hello.md", "notes.md"]);
    assert_eq!(
        read(&dir, "content/article/notes.md"),
        "+++\ntitle = \"by hand\"\n+++\n"
    );
}

#[test]
fn collection_of_an_empty_list_removes_its_pages_only() {
    let dir = site("collection-empty", article_collection());
    let stub = start(Behaviour::Data(posts(&["hello", "world"])));
    ok(&gqlfreez(&dir, &stub.endpoint(), &[]));
    let stub = start(Behaviour::Data(posts(&[])));
    let err = ok(&gqlfreez(&dir, &stub.endpoint(), &[]));
    assert!(err.contains("2 removed"), "{err}");
    assert_eq!(pages(&dir), ["_index.md"]);
}

#[test]
fn collection_does_not_rewrite_pages_when_the_list_is_reordered() {
    let dir = site("collection-reorder", article_collection());
    let stub = start(Behaviour::Data(posts(&["hello", "world"])));
    ok(&gqlfreez(&dir, &stub.endpoint(), &[]));
    let mtime = |name: &str| {
        std::fs::metadata(dir.join("content/article").join(name))
            .unwrap()
            .modified()
            .unwrap()
    };
    let before = (mtime("hello.md"), mtime("world.md"));

    std::thread::sleep(std::time::Duration::from_millis(1100));
    let stub = start(Behaviour::Data(posts(&["world", "hello"])));
    let err = ok(&gqlfreez(&dir, &stub.endpoint(), &[]));
    assert!(err.contains("0 page(s) written, 0 removed"), "{err}");
    assert_eq!((mtime("hello.md"), mtime("world.md")), before);
}

#[test]
fn collection_slug_collision_writes_nothing() {
    let dir = site("collection-collision", article_collection());
    let stub = start(Behaviour::Data(posts(&["hello", "Hello"])));
    let out = gqlfreez(&dir, &stub.endpoint(), &[]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("items 0 and 1 both give the file"), "{err}");
    assert_eq!(pages(&dir), ["_index.md"]);
    assert!(!dir.join("data/wp/article.json").exists(), "all or nothing");
}

#[test]
fn collection_never_overwrites_a_hand_written_file() {
    let dir = site("collection-handwritten", article_collection());
    write(&dir, "content/article/hello.md", "mine\n");
    let stub = start(Behaviour::Data(posts(&["hello", "world"])));
    let out = gqlfreez(&dir, &stub.endpoint(), &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("was not generated from this query"));
    assert_eq!(read(&dir, "content/article/hello.md"), "mine\n");
    assert!(!dir.join("content/article/world.md").exists());
}

#[test]
fn collection_missing_field_names_the_item() {
    let mut collection = article_collection();
    collection["frontMatter"]["path"] = json!("/blog/{{item.author.handle}}/");
    let dir = site("collection-missing", collection);
    let stub = start(Behaviour::Data(posts(&["hello"])));
    let out = gqlfreez(&dir, &stub.endpoint(), &[]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("item 0: {{item.author.handle}}: no item.author.handle"),
        "{err}"
    );
    assert_eq!(pages(&dir), ["_index.md"]);
}

#[test]
fn collection_cannot_write_outside_the_root() {
    let mut collection = article_collection();
    collection["dir"] = json!("../../../elsewhere");
    let dir = site("collection-outside", collection);
    let stub = start(Behaviour::Data(posts(&["hello"])));
    let out = gqlfreez(&dir, &stub.endpoint(), &[]);
    assert_eq!(out.status.code(), Some(2), "a configuration problem");
    assert!(String::from_utf8_lossy(&out.stderr).contains("outside"));
    assert_eq!(
        stub.calls.load(Ordering::SeqCst),
        0,
        "refused before any request"
    );
}

#[test]
fn a_collection_with_an_unknown_key_is_refused_before_any_request() {
    let mut collection = article_collection();
    collection["frontmatter"] = collection["frontMatter"].take();
    collection.as_object_mut().unwrap().remove("frontMatter");
    let dir = site("collection-typo", collection);
    let stub = start(Behaviour::Data(posts(&["hello"])));
    let out = gqlfreez(&dir, &stub.endpoint(), &[]);
    assert_eq!(out.status.code(), Some(2), "a configuration problem");
    assert!(String::from_utf8_lossy(&out.stderr).contains("frontmatter"));
    assert_eq!(stub.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn an_unreadable_config_is_a_configuration_problem() {
    let dir = site("collection-unreadable", article_collection());
    std::fs::write(dir.join("data/wp/.graphqlrc.json"), b"\xff\xfe").unwrap();
    let stub = start(Behaviour::Data(posts(&["hello"])));
    let out = gqlfreez(&dir, &stub.endpoint(), &[]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{err}");
    assert!(err.contains(".graphqlrc.json: cannot be read"), "{err}");
    assert_eq!(stub.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn collection_check_and_dry_run_count_pages_without_writing() {
    let dir = site("collection-check", article_collection());
    let stub = start(Behaviour::Data(posts(&["hello", "world"])));
    ok(&gqlfreez(&dir, &stub.endpoint(), &[]));
    std::fs::remove_file(dir.join("content/article/world.md")).unwrap();

    for (flag, code) in [("--check", 3), ("--dry-run", 0)] {
        let out = gqlfreez(&dir, &stub.endpoint(), &[flag]);
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(code), "{flag}: {err}");
        assert!(
            err.contains("1 page(s) would be written, 0 would be removed"),
            "{flag}: {err}"
        );
    }
    assert_eq!(
        pages(&dir),
        ["_index.md", "hello.md"],
        "no temporary file left either"
    );
}

#[test]
fn collection_two_queries_writing_the_same_page_write_nothing() {
    let dir = site("collection-overlap", article_collection());
    write(&dir, "data/wp/copy.graphql", ARTICLE_QUERY);
    write(
        &dir,
        "data/wp/.graphqlrc.json",
        &graphqlrc_json(
            json!({ "article.graphql": article_collection(), "copy.graphql": article_collection() }),
            None,
        ),
    );
    let stub = start(Behaviour::Data(posts(&["hello"])));
    let out = gqlfreez(&dir, &stub.endpoint(), &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("written by two queries"));
    assert_eq!(pages(&dir), ["_index.md"]);
}

#[test]
fn collection_of_a_null_list_writes_and_removes_nothing() {
    let dir = site("collection-null-list", article_collection());
    let stub = start(Behaviour::Data(posts(&["hello"])));
    ok(&gqlfreez(&dir, &stub.endpoint(), &[]));
    let before = read(&dir, "data/wp/article.json");

    let stub = start(Behaviour::Data(json!({"posts": {"nodes": null}})));
    let out = gqlfreez(&dir, &stub.endpoint(), &[]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("posts.nodes is null") && err.contains("article.graphql"),
        "{err}"
    );
    assert_eq!(pages(&dir), ["_index.md", "hello.md"]);
    assert_eq!(read(&dir, "data/wp/article.json"), before);
}

#[test]
fn collection_yaml_quotes_strings() {
    let dir = scratch("collection-yaml-quotes");
    write(&dir, "data/countries.graphql", "{ countries { code } }");
    write(
        &dir,
        "data/.graphqlrc.yml",
        "extensions:\n\
         \x20 collections:\n\
         \x20   countries.graphql:\n\
         \x20     list: countries\n\
         \x20     dir: ../content\n\
         \x20     file: \"{{item.code}}.md\"\n\
         \x20     format: yaml\n\
         \x20     frontMatter:\n\
         \x20       code: \"{{item.code}}\"\n",
    );
    let stub = start(Behaviour::Data(json!({"countries": [{"code": "NO"}]})));
    ok(&gqlfreez(&dir, &stub.endpoint(), &[]));
    assert_eq!(
        read(&dir, "content/NO.md"),
        "---\n\
         # generated by gqlfreez from ../data/countries.graphql, do not edit\n\
         code: 'NO'\n\
         ---\n"
    );
}

#[test]
fn a_collection_key_with_no_query_is_refused_before_any_request() {
    let dir = site("collection-no-query", article_collection());
    write(
        &dir,
        "data/wp/.graphqlrc.json",
        &graphqlrc_json(
            json!({ "article.graphql": article_collection(), "gone.graphql": article_collection() }),
            None,
        ),
    );
    let stub = start(Behaviour::Data(posts(&["hello"])));
    let (status, err) = code(&gqlfreez(&dir, &stub.endpoint(), &[]));
    assert_eq!(status, Some(2), "{err}");
    assert!(
        err.contains(".graphqlrc.json") && err.contains("\"gone.graphql\" names no query file"),
        "{err}"
    );
    assert_eq!(stub.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn a_collection_key_for_a_query_of_another_config_is_refused() {
    let dir = scratch("collection-other-config");
    write(&dir, "data/wp/article.graphql", ARTICLE_QUERY);
    write(
        &dir,
        "data/wp/.graphqlrc.json",
        &graphqlrc_json(json!({}), None),
    );
    let mut collection = article_collection();
    collection["dir"] = json!("../content/article");
    write(
        &dir,
        "data/.graphqlrc.json",
        &graphqlrc_json(json!({ "wp/article.graphql": collection }), None),
    );
    let stub = start(Behaviour::Data(posts(&["hello"])));
    let (status, err) = code(&gqlfreez(&dir, &stub.endpoint(), &[]));
    assert_eq!(status, Some(2), "{err}");
    assert!(
        err.contains("data/.graphqlrc.json") && err.contains("\"wp/article.graphql\" names"),
        "{err}"
    );
    assert!(err.contains("wp/.graphqlrc.json instead"), "{err}");
    assert_eq!(stub.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn a_collection_key_in_a_config_no_query_uses_is_still_checked() {
    let dir = site("collection-unused-config", article_collection());
    write(
        &dir,
        ".graphqlrc.yml",
        "extensions:\n\
         \x20 collections:\n\
         \x20   typo.graphql:\n\
         \x20     list: posts.nodes\n\
         \x20     dir: content/article\n\
         \x20     file: \"{{item.slug}}.md\"\n\
         \x20     frontMatter: {}\n",
    );
    let stub = start(Behaviour::Data(posts(&["hello"])));
    let (status, err) = code(&gqlfreez(&dir, &stub.endpoint(), &[]));
    assert_eq!(status, Some(2), "{err}");
    assert!(
        err.contains("\"typo.graphql\" names no query file"),
        "{err}"
    );
    assert_eq!(stub.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn a_collection_key_for_a_query_outside_the_root_is_left_to_its_own_run() {
    let dir = scratch("collection-key-outside-root");
    write(&dir, "data/article.graphql", ARTICLE_QUERY);
    write(&dir, "other/article.graphql", ARTICLE_QUERY);
    let mut collection = article_collection();
    collection["dir"] = json!("content/article");
    write(
        &dir,
        ".graphqlrc.json",
        &graphqlrc_json(json!({ "other/article.graphql": collection }), None),
    );
    let stub = start(Behaviour::Data(posts(&["hello"])));
    ok(&gqlfreez(&dir.join("data"), &stub.endpoint(), &[]));
    assert!(dir.join("data/article.json").exists());
    assert!(!dir.join("content").exists());
}

#[test]
fn a_collection_key_must_be_relative_with_forward_slashes() {
    let dir = site("collection-key-shape", article_collection());
    let absolute = dir.join("data/wp/article.graphql");
    for key in [absolute.to_str().unwrap(), "sub\\article.graphql"] {
        write(
            &dir,
            "data/wp/.graphqlrc.json",
            &graphqlrc_json(json!({ key: article_collection() }), None),
        );
        let stub = start(Behaviour::Data(posts(&["hello"])));
        let (status, err) = code(&gqlfreez(&dir, &stub.endpoint(), &[]));
        assert_eq!(status, Some(2), "{key}: {err}");
        assert!(
            err.contains("must be relative to the config file's folder, with forward slashes"),
            "{err}"
        );
        assert_eq!(stub.calls.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn dir_is_relative_to_the_config_folder() {
    let dir = scratch("collection-config-folder");
    write(&dir, "data/wp/article.graphql", ARTICLE_QUERY);
    let mut collection = article_collection();
    collection["dir"] = json!("content/article");
    write(
        &dir,
        ".graphqlrc.json",
        &graphqlrc_json(json!({ "data/wp/article.graphql": collection }), None),
    );
    let stub = start(Behaviour::Data(posts(&["hello"])));
    ok(&gqlfreez(&dir, &stub.endpoint(), &[]));
    assert!(read(&dir, "content/article/hello.md")
        .contains("# generated by gqlfreez from ../../data/wp/article.graphql, do not edit"));
}

#[test]
fn a_query_in_a_subfolder_is_keyed_by_its_relative_path() {
    let dir = scratch("collection-subfolder");
    write(&dir, "data/blog/posts.graphql", ARTICLE_QUERY);
    write(
        &dir,
        "data/.graphqlrc.toml",
        "[extensions.collections.\"blog/posts.graphql\"]\n\
         list = \"posts.nodes\"\n\
         dir = \"../content/article\"\n\
         file = \"{{item.slug}}.md\"\n\
         \n\
         [extensions.collections.\"blog/posts.graphql\".frontMatter]\n\
         title = \"{{item.title}}\"\n",
    );
    let stub = start(Behaviour::Data(posts(&["hello"])));
    ok(&gqlfreez(&dir, &stub.endpoint(), &[]));
    assert_eq!(
        read(&dir, "content/article/hello.md"),
        "+++\n\
         # generated by gqlfreez from ../../data/blog/posts.graphql, do not edit\n\
         title = \"About hello\"\n\
         +++\n"
    );
}

#[test]
fn a_date_in_a_toml_config_is_written_as_text() {
    let dir = scratch("collection-toml-date");
    write(&dir, "data/blog/posts.graphql", ARTICLE_QUERY);
    write(
        &dir,
        "data/.graphqlrc.toml",
        "[extensions.collections.\"blog/posts.graphql\"]\n\
         list = \"posts.nodes\"\n\
         dir = \"../content/article\"\n\
         file = \"{{item.slug}}.md\"\n\
         \n\
         [extensions.collections.\"blog/posts.graphql\".frontMatter]\n\
         date = 2024-05-01\n\
         updated = [2024-05-02T10:00:00Z]\n",
    );
    let stub = start(Behaviour::Data(posts(&["hello"])));
    ok(&gqlfreez(&dir, &stub.endpoint(), &[]));
    assert_eq!(
        read(&dir, "content/article/hello.md"),
        "+++\n\
         # generated by gqlfreez from ../../data/blog/posts.graphql, do not edit\n\
         date = \"2024-05-01\"\n\
         updated = [\"2024-05-02T10:00:00Z\"]\n\
         +++\n"
    );
}

#[test]
fn a_collection_works_with_the_endpoint_flag_without_the_config_headers() {
    let dir = site("collection-endpoint-flag", article_collection());
    let configured = start(Behaviour::Simple);
    write(
        &dir,
        "data/wp/.graphqlrc.json",
        &graphqlrc_json(
            json!({ "article.graphql": article_collection() }),
            Some((&configured.endpoint(), "config-token")),
        ),
    );
    let forced = start(Behaviour::Data(posts(&["hello"])));
    ok(&gqlfreez(&dir, &forced.endpoint(), &[]));
    assert_eq!(pages(&dir), ["_index.md", "hello.md"]);
    assert_eq!(auth(&forced), [""]);
    assert_eq!(configured.calls.load(Ordering::SeqCst), 0);
}

// One run, several data sources: each query takes the graphql-config closest to it.

fn graphqlrc(dir: &Path, rel: &str, endpoint: &str, token: &str) {
    write(
        dir,
        rel,
        &format!(
            "extensions:\n  endpoints:\n    default:\n      url: {endpoint}\n      headers:\n        Authorization: Bearer {token}\n"
        ),
    );
}

/// No `--endpoint`: the addresses come from the configs.
fn gqlfreez_from_configs(dir: &Path, extra: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_gqlfreez"))
        .arg(dir)
        .arg("--no-dotenv")
        .args(extra)
        .output()
        .expect("run gqlfreez")
}

fn auth(stub: &Stub) -> Vec<String> {
    stub.auth.lock().unwrap().clone()
}

/// A site with the queries under data/ and the pages under content/: two sources, one
/// graphql-config each; `wp` has a collection.
fn two_sources(name: &str) -> (PathBuf, Stub, Stub) {
    let dir = site(name, article_collection());
    let wp = start(Behaviour::Data(posts(&["hello", "world"])));
    let shop = start(Behaviour::Data(
        json!({"products": {"nodes": [{"name": "mug"}]}}),
    ));
    write(
        &dir,
        "data/wp/.graphqlrc.json",
        &graphqlrc_json(
            json!({ "article.graphql": article_collection() }),
            Some((&wp.endpoint(), "wp-token")),
        ),
    );
    write(
        &dir,
        "data/shop/products.graphql",
        "{ products(first: 5) { nodes { name } } }",
    );
    graphqlrc(
        &dir,
        "data/shop/.graphqlrc.yml",
        &shop.endpoint(),
        "shop-token",
    );
    (dir, wp, shop)
}

#[test]
fn each_folder_takes_its_own_graphqlrc_in_one_run() {
    let (dir, wp, shop) = two_sources("sources-two");
    ok(&gqlfreez_from_configs(&dir, &[]));

    assert_eq!(
        read_json(&dir, "data/shop/products.json"),
        json!({"products": {"nodes": [{"name": "mug"}]}})
    );
    assert_eq!(
        read_json(&dir, "data/wp/article.json")["posts"]["nodes"][0]["slug"],
        "hello"
    );
    assert_eq!(pages(&dir), ["_index.md", "hello.md", "world.md"]);
    // Each token only ever goes to its own address.
    assert_eq!(auth(&wp), ["Bearer wp-token"]);
    assert_eq!(auth(&shop), ["Bearer shop-token"]);
}

#[test]
fn a_folder_without_config_takes_the_closest_one_above() {
    let dir = scratch("sources-closest");
    let top = start(Behaviour::Simple);
    let inner = start(Behaviour::Simple);
    graphqlrc(&dir, "src/.graphqlrc.yml", &top.endpoint(), "top");
    graphqlrc(&dir, "src/blog/.graphqlrc.yml", &inner.endpoint(), "inner");
    write(
        &dir,
        "src/home/posts.graphql",
        "{ posts(first: 1) { nodes { title } } }",
    );
    write(
        &dir,
        "src/blog/deep/posts.graphql",
        "{ posts(first: 1) { nodes { title } } }",
    );

    // The root is `src/`: its own config counts, and so does one above it, as before.
    ok(&gqlfreez_from_configs(&dir.join("src"), &[]));
    assert_eq!(auth(&top), ["Bearer top"]);
    assert_eq!(auth(&inner), ["Bearer inner"]);
}

#[test]
fn a_config_above_the_root_still_applies_when_nothing_is_closer() {
    let dir = scratch("sources-above-root");
    let stub = start(Behaviour::Simple);
    graphqlrc(&dir, ".graphqlrc.yml", &stub.endpoint(), "project");
    write(
        &dir,
        "src/posts.graphql",
        "{ posts(first: 1) { nodes { title } } }",
    );

    ok(&gqlfreez_from_configs(&dir.join("src"), &[]));
    assert_eq!(auth(&stub), ["Bearer project"]);
}

#[test]
fn the_endpoint_flag_wins_over_every_config() {
    let (dir, wp, shop) = two_sources("sources-flag");
    let forced = start(Behaviour::Data(posts(&["forced"])));
    ok(&gqlfreez(&dir, &forced.endpoint(), &[]));

    assert_eq!(forced.calls.load(Ordering::SeqCst), 2);
    assert_eq!(wp.calls.load(Ordering::SeqCst), 0);
    assert_eq!(shop.calls.load(Ordering::SeqCst), 0);
    // A per-folder token is not sent to an address it was not written for.
    assert_eq!(auth(&forced), ["", ""]);
}

#[test]
fn the_endpoint_flag_takes_no_url_or_header_from_a_config() {
    let dir = scratch("sources-flag-root");
    let configured = start(Behaviour::Simple);
    graphqlrc(&dir, ".graphqlrc", &configured.endpoint(), "root-token");
    write(
        &dir,
        "posts.graphql",
        "{ posts(first: 1) { nodes { title } } }",
    );
    let forced = start(Behaviour::Simple);

    ok(&gqlfreez(&dir, &forced.endpoint(), &[]));
    assert_eq!(auth(&forced), [""]);
    ok(&gqlfreez(
        &dir,
        &forced.endpoint(),
        &["-H", "Authorization: Bearer cli"],
    ));
    assert_eq!(auth(&forced), ["", "Bearer cli"]);
    assert_eq!(configured.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn a_header_flag_goes_to_every_source() {
    let (dir, wp, shop) = two_sources("sources-header");
    ok(&gqlfreez_from_configs(
        &dir,
        &["-H", "Authorization: Bearer cli"],
    ));
    assert_eq!(auth(&wp), ["Bearer cli"]);
    assert_eq!(auth(&shop), ["Bearer cli"]);
}

#[test]
fn a_query_with_no_endpoint_is_named_before_any_request() {
    let (dir, wp, shop) = two_sources("sources-missing");
    // A config with no endpoint at the root stops the lookup there, whatever is above.
    write(&dir, ".graphqlrc.yml", "schema: ./schema.graphql\n");
    write(
        &dir,
        "data/orphan/q.graphql",
        "{ posts(first: 1) { nodes { title } } }",
    );

    let out = gqlfreez_from_configs(&dir, &[]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("orphan/q.graphql") && err.contains("no GraphQL endpoint"),
        "{err}"
    );
    assert_eq!(
        wp.calls.load(Ordering::SeqCst) + shop.calls.load(Ordering::SeqCst),
        0
    );
    assert_eq!(pages(&dir), ["_index.md"]);
}
