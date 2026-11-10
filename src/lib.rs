//! Freeze GraphQL query results into JSON files, next to the queries.
//!
//! This library is the engine. It never decides to stop the program: it returns one result
//! per file, and the caller (the CLI, or a future service mode) decides what to do with them.
//! Failing fast is a CLI policy, not an engine behaviour.

pub mod config;
pub mod errors;
pub mod logger;
pub mod options;

mod collection;
mod discover;
mod endpoint;
mod fetch;
mod output;
mod paginate;
mod query;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::Value;

pub use collection::Report as CollectionReport;
pub use errors::Error;
pub use logger::{Level, Logger};
pub use options::Options;

/// Outcome for one `.graphql` file, successful or not.
#[derive(Debug)]
pub struct FileResult {
    /// Position in the discovery order, so failures can be reported deterministically.
    pub index: usize,
    /// The source file.
    pub source: PathBuf,
    /// The output file it maps to.
    pub output: PathBuf,
    pub outcome: Outcome,
    /// Pages written from the query's collection, when it has one.
    pub collection: Option<CollectionReport>,
}

#[derive(Debug)]
pub enum Outcome {
    /// Written: the result differs from what was on disk.
    Frozen {
        pages: usize,
    },
    /// Not written: identical to what was already there (incremental build).
    Unchanged,
    /// Would have been written — `--check` / `--dry-run`.
    WouldChange,
    /// Skipped: the file holds no operation (fragments only).
    Skipped {
        reason: String,
    },
    Failed(Error),
}

impl FileResult {
    pub fn is_failure(&self) -> bool {
        matches!(self.outcome, Outcome::Failed(_))
    }
}

/// Run every query found under `options.root`.
///
/// Results come back in discovery order. The engine never aborts on its own — inspect
/// `Outcome::Failed` on each result.
///
/// The pages of a collection are only ever written, or removed, under `options.root`.
pub async fn run(options: &Options) -> Result<Vec<FileResult>, Error> {
    options.validate()?;

    let root = options.root.canonicalize().map_err(|source| Error::Io {
        path: options.root.clone(),
        source,
    })?;
    let queries = discover::find(&root, &options.glob)?;
    if queries.is_empty() {
        return Err(Error::NoQueries {
            root: options.root.clone(),
            glob: options.glob.clone(),
        });
    }
    // Configs are read before any request: a typo in one must not cost a full fetch.
    let configs = endpoint::configs(&queries)?;
    let collections = collection::resolve(&queries, &configs, &root)?;
    let targets = endpoint::resolve(&queries, &configs, options)?;
    if !options.check && !options.dry_run {
        output::sweep_temporaries(&queries.iter().map(|q| q.output.clone()).collect::<Vec<_>>());
    }

    options.logger.status(&format!(
        "Freezing {} quer{}…",
        queries.len(),
        if queries.len() == 1 { "y" } else { "ies" }
    ));

    let client = fetch::client(options)?;
    let semaphore = Arc::new(tokio::sync::Semaphore::new(options.concurrency));
    let stop = Arc::new(AtomicBool::new(false));

    let mut tasks = tokio::task::JoinSet::new();
    let jobs = queries.iter().cloned().zip(targets).zip(collections);
    for (index, ((q, target), collection)) in jobs.enumerate() {
        let permit = semaphore.clone();
        let client = client.clone();
        let options = options.clone();
        let stop = stop.clone();
        tasks.spawn(async move {
            let _guard = permit.acquire().await.expect("semaphore is never closed");
            // Fail fast: once something has gone wrong, do not keep hammering the API.
            if stop.load(Ordering::Relaxed) {
                return None;
            }
            let (outcome, plan) = one(&client, &options, &target, &q, collection.as_deref()).await;
            if matches!(outcome, Outcome::Failed(_)) {
                stop.store(true, Ordering::Relaxed);
            }
            Some((
                FileResult {
                    index,
                    source: q.source,
                    output: q.output,
                    collection: plan.as_ref().map(collection::Plan::report),
                    outcome,
                },
                plan,
            ))
        });
    }

    let mut results = vec![];
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(Some(r)) => results.push(r),
            Ok(None) => {}
            Err(e) => {
                return Err(Error::Config(format!("internal task failure: {e}")));
            }
        }
    }
    // Discovery order, so the reported failure is the same on every run.
    results.sort_by_key(|(r, _)| r.index);
    let (mut results, plans): (Vec<FileResult>, Vec<Option<collection::Plan>>) =
        results.into_iter().unzip();

    commit(&mut results, &plans, options);
    Ok(results)
}

/// Fetch one file, and plan its pages if it has a collection. The pages are kept in memory
/// until the commit, so that nothing is written unless every query succeeded. A disk error
/// while the files are moved into place can still leave some of them updated.
async fn one(
    client: &reqwest::Client,
    options: &Options,
    target: &endpoint::Target,
    q: &discover::Query,
    collection: Option<&collection::Collection>,
) -> (Outcome, Option<collection::Plan>) {
    let (data, outcome) = match (fetch(client, options, target, q).await, collection) {
        (Fetched::Data(data, outcome), _) => (data, outcome),
        (Fetched::Done(Outcome::Skipped { reason }), Some(collection)) => {
            let error =
                collection.error(format!("no list to read, the query is skipped ({reason})"));
            return (Outcome::Failed(error), None);
        }
        (Fetched::Done(outcome), _) => return (outcome, None),
    };
    let Some(collection) = collection else {
        return (outcome, None);
    };
    match collection.plan(&data, &options.logger) {
        Ok(plan) => (outcome, Some(plan)),
        Err(e) => (Outcome::Failed(e), None),
    }
}

enum Fetched {
    /// The query ran. The result is the one before `--envelope`, which is what a collection
    /// reads, so that its `list` keeps one meaning whatever the flags.
    Data(Value, Outcome),
    /// Skipped or failed: no list to read.
    Done(Outcome),
}

/// The rendered JSON is staged in a temporary file. A result with a collection also stays in
/// memory, with its pages, until the commit.
async fn fetch(
    client: &reqwest::Client,
    options: &Options,
    target: &endpoint::Target,
    q: &discover::Query,
) -> Fetched {
    use Fetched::Done;
    let source = match std::fs::read_to_string(&q.source) {
        Ok(s) => s,
        Err(source_err) => {
            return Done(Outcome::Failed(Error::Io {
                path: q.source.clone(),
                source: source_err,
            }))
        }
    };

    let analysis = match query::analyze(&source, &q.source) {
        Ok(a) => a,
        Err(e) => return Done(Outcome::Failed(e)),
    };
    match analysis.kind {
        query::Kind::FragmentsOnly => {
            return Done(Outcome::Skipped {
                reason: "fragments only, no operation".into(),
            })
        }
        query::Kind::Empty => {
            return Done(Outcome::Skipped {
                reason: "no definition".into(),
            })
        }
        query::Kind::Operation => {}
    }

    let deadline = tokio::time::timeout(
        options.file_timeout,
        paginate::run(
            fetch::Remote { client, target },
            options,
            &q.source,
            &analysis,
        ),
    );
    let (data, pages) = match deadline.await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Done(Outcome::Failed(e)),
        Err(_) => {
            return Done(Outcome::Failed(Error::Response {
                path: q.source.clone(),
                message: format!(
                    "gave up after --file-timeout ({}s)",
                    options.file_timeout.as_secs()
                ),
            }))
        }
    };

    // `--envelope` restores the GraphQL shape. `extensions` is dropped either way: Shopify
    // puts request costs in it and Apollo puts tracing, which would make every diff noisy.
    let rendered = if options.envelope {
        output::render(&serde_json::json!({ "data": data }))
    } else {
        output::render(&data)
    };
    let outcome = match output::stage(&q.output, &rendered) {
        Ok(output::Staged::Unchanged) => Outcome::Unchanged,
        Ok(output::Staged::Ready) if options.dry_run || options.check => Outcome::WouldChange,
        Ok(output::Staged::Ready) => Outcome::Frozen { pages },
        Err(e) => return Done(Outcome::Failed(e)),
    };
    Fetched::Data(data, outcome)
}

/// Move every staged file into place, then write the pages of the collections. Nothing is
/// written before this point, so a failure in the fetch phase leaves the tree untouched.
fn commit(results: &mut [FileResult], plans: &[Option<collection::Plan>], options: &Options) {
    let with_plans: Vec<(usize, &collection::Plan)> = plans
        .iter()
        .enumerate()
        .filter_map(|(i, p)| p.as_ref().map(|p| (i, p)))
        .collect();
    if let Some((i, message)) = collection::check_overlap(&with_plans) {
        let path = results[i].source.clone();
        results[i].outcome = Outcome::Failed(Error::Collection { path, message });
    }

    let pages: Vec<(usize, &PathBuf, &String)> = with_plans
        .iter()
        .flat_map(|(i, plan)| plan.writes.iter().map(move |(path, page)| (*i, path, page)))
        .collect();
    let discard = |results: &[FileResult]| {
        for r in results {
            output::discard(&r.output);
        }
        for (_, path, _) in &pages {
            output::discard(path);
        }
    };

    let failed = results.iter().any(FileResult::is_failure);
    if failed || options.check || options.dry_run {
        discard(results);
        return;
    }
    for (i, path, page) in &pages {
        if let Err(e) = output::write_temp(path, page) {
            results[*i].outcome = Outcome::Failed(e);
            discard(results);
            return;
        }
    }
    for r in results.iter_mut() {
        if matches!(r.outcome, Outcome::Frozen { .. }) {
            if let Err(e) = output::commit(&r.output) {
                r.outcome = Outcome::Failed(e);
            }
        }
    }
    if let Err((i, e)) = collection::remove_stale(&with_plans) {
        results[i].outcome = Outcome::Failed(e);
        discard(results);
        return;
    }
    for (i, path, _) in &pages {
        if let Err(e) = output::commit(path) {
            results[*i].outcome = Outcome::Failed(e);
            discard(results);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use options::defaults;

    #[tokio::test]
    async fn an_empty_root_is_a_typed_error() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/tmp/gqlfreez-test-no-queries");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let options = Options {
            root: root.clone(),
            glob: defaults::GLOB.into(),
            endpoint: Some("http://127.0.0.1:9/graphql".into()),
            headers: vec![],
            concurrency: defaults::CONCURRENCY,
            page_size: defaults::PAGE_SIZE,
            retries: defaults::RETRIES,
            delay: defaults::DELAY,
            connect_timeout: defaults::CONNECT_TIMEOUT,
            request_timeout: defaults::REQUEST_TIMEOUT,
            file_timeout: defaults::FILE_TIMEOUT,
            max_pages: defaults::MAX_PAGES,
            max_response_bytes: defaults::MAX_RESPONSE_BYTES,
            allow_partial: false,
            envelope: false,
            check: false,
            dry_run: false,
            logger: Logger::new(Level::Silent, false),
        };
        let err = run(&options).await.unwrap_err();
        assert!(
            matches!(&err, Error::NoQueries { root: r, glob } if *r == root && glob == defaults::GLOB)
        );
        assert_eq!(
            err.to_string(),
            format!(
                "config: no query file under {} matching {:?}",
                root.display(),
                defaults::GLOB
            )
        );
    }
}
