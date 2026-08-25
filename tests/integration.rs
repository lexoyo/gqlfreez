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
}

struct Stub {
    port: u16,
    calls: Arc<AtomicUsize>,
    queries: Arc<std::sync::Mutex<Vec<String>>>,
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
    let c = calls.clone();
    let q = queries.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let n = c.fetch_add(1, Ordering::SeqCst);
            let behaviour = behaviour.clone();
            let q = q.clone();
            std::thread::spawn(move || handle(stream, behaviour, n, q));
        }
    });
    Stub {
        port,
        calls,
        queries,
    }
}

fn handle(
    mut stream: TcpStream,
    behaviour: Behaviour,
    call: usize,
    queries: Arc<std::sync::Mutex<Vec<String>>>,
) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut length = 0usize;
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
    std::fs::write(dir.join(name), body).unwrap();
}

fn gqlfreez(dir: &Path, endpoint: &str, extra: &[&str]) -> std::process::Output {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_gqlfreez"));
    cmd.arg(dir)
        .arg("--endpoint")
        .arg(endpoint)
        .arg("--no-dotenv");
    cmd.args(extra);
    cmd.output().expect("run gqlfreez")
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
