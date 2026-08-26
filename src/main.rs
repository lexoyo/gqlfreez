use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::Parser;
use gqlfreez::options::defaults;
use gqlfreez::{Error, Level, Logger, Options, Outcome};

/// Exit codes, so a CI script never has to grep stderr.
mod exit {
    pub const OK: u8 = 0;
    pub const FAILED: u8 = 1;
    pub const CONFIG: u8 = 2;
    pub const CHANGED: u8 = 3;
}

/// Freeze GraphQL query results into JSON files, next to the queries.
#[derive(Parser, Debug)]
#[command(name = "gqlfreez", version, about, long_about = None)]
struct Cli {
    /// Directory to scan.
    #[arg(default_value = ".")]
    root: PathBuf,

    /// GraphQL endpoint. Otherwise resolved from graphql-config.
    #[arg(long)]
    endpoint: Option<String>,

    /// Glob matching the query files.
    #[arg(long, default_value = defaults::GLOB)]
    glob: String,

    /// Extra request header, repeatable: --header "Authorization: Bearer …"
    #[arg(long = "header", short = 'H', value_name = "NAME: VALUE")]
    headers: Vec<String>,

    /// Queries run concurrently. Keep it low against shared hosting.
    #[arg(long, default_value_t = defaults::CONCURRENCY)]
    concurrency: usize,

    /// Wait this many milliseconds between paginated requests.
    #[arg(long, default_value_t = 0)]
    delay: u64,

    /// Timeout for a single HTTP request, in seconds.
    #[arg(long, default_value_t = defaults::REQUEST_TIMEOUT.as_secs())]
    timeout: u64,

    /// Timeout for connecting, in seconds.
    #[arg(long, default_value_t = defaults::CONNECT_TIMEOUT.as_secs())]
    connect_timeout: u64,

    /// Timeout for one query file, paginating included, in seconds.
    #[arg(long, default_value_t = defaults::FILE_TIMEOUT.as_secs())]
    file_timeout: u64,

    /// Retries on transient failures (timeouts, 5xx, rate limits).
    #[arg(long, default_value_t = defaults::RETRIES)]
    retries: u32,

    /// Nodes requested per page when paginating.
    #[arg(long, default_value_t = defaults::PAGE_SIZE)]
    page_size: usize,

    /// Maximum number of pages for a paginated query.
    #[arg(long, default_value_t = defaults::MAX_PAGES)]
    max_pages: usize,

    /// Largest response accepted, in megabytes.
    #[arg(long, default_value_t = defaults::MAX_RESPONSE_BYTES / (1024 * 1024))]
    max_response_mb: u64,

    /// Write the full GraphQL response instead of the contents of `data` alone.
    #[arg(long)]
    envelope: bool,

    /// Accept a partial response (`data` present alongside a non-empty `errors`).
    #[arg(long)]
    allow_partial: bool,

    /// Fail if anything would change, without writing. For CI.
    #[arg(long)]
    check: bool,

    /// Run everything but write nothing.
    #[arg(long)]
    dry_run: bool,

    /// Do not load `.env` / `.env.local`.
    #[arg(long)]
    no_dotenv: bool,

    #[arg(long, short)]
    verbose: bool,

    #[arg(long, short)]
    quiet: bool,

    #[arg(long)]
    silent: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let start = Instant::now();

    if !cli.no_dotenv {
        let _ = dotenvy::from_filename(".env.local");
        let _ = dotenvy::dotenv();
    }

    let level = if cli.silent {
        Level::Silent
    } else if cli.quiet {
        Level::Quiet
    } else if cli.verbose {
        Level::Verbose
    } else {
        Level::Standard
    };
    let logger = Logger::new(level, true);

    let options = match build_options(&cli, logger.clone()) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("gqlfreez: {e}");
            return ExitCode::from(exit::CONFIG);
        }
    };

    let results = match gqlfreez::run(&options).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("gqlfreez: {e}");
            return ExitCode::from(match e {
                Error::Config(_) => exit::CONFIG,
                _ => exit::FAILED,
            });
        }
    };

    // Failing fast is a CLI policy: the engine itself returns one result per file.
    if let Some(failed) = results.iter().find(|r| r.is_failure()) {
        if let Outcome::Failed(err) = &failed.outcome {
            eprintln!("gqlfreez: {err}");
        }
        return ExitCode::from(exit::FAILED);
    }

    let mut frozen = 0;
    let mut unchanged = 0;
    let mut would = 0;
    let mut skipped = 0;
    for r in &results {
        match &r.outcome {
            Outcome::Frozen { pages } => {
                frozen += 1;
                if *pages > 1 {
                    logger.verbose(&format!("{}: {pages} pages", r.source.display()));
                }
            }
            Outcome::Unchanged => unchanged += 1,
            Outcome::WouldChange => would += 1,
            Outcome::Skipped { reason } => {
                skipped += 1;
                logger.verbose(&format!("{}: skipped — {reason}", r.source.display()));
            }
            Outcome::Failed(_) => {}
        }
    }

    let elapsed = start.elapsed();
    let mut summary = if cli.check || cli.dry_run {
        format!("{would} would change, {unchanged} unchanged")
    } else {
        format!("{frozen} frozen, {unchanged} unchanged")
    };
    if skipped > 0 {
        summary.push_str(&format!(", {skipped} skipped"));
    }
    logger.status(&format!(
        "{summary} — finished in {}.{:03}s",
        elapsed.as_secs(),
        elapsed.subsec_millis()
    ));

    if cli.check && would > 0 {
        eprintln!("gqlfreez: {would} file(s) are out of date — run gqlfreez to refresh them");
        return ExitCode::from(exit::CHANGED);
    }
    ExitCode::from(exit::OK)
}

fn build_options(cli: &Cli, logger: Logger) -> Result<Options, Error> {
    let root = cli.root.canonicalize().map_err(|source| Error::Io {
        path: cli.root.clone(),
        source,
    })?;

    let config = gqlfreez::config::discover(&root)?;

    let endpoint = cli.endpoint.clone().or(config.endpoint).unwrap_or_default();

    let mut headers = config.headers;
    for raw in &cli.headers {
        let (name, value) = raw.split_once(':').ok_or_else(|| {
            Error::Config(format!("--header expects \"Name: value\", got {raw:?}"))
        })?;
        headers.push((name.trim().to_owned(), value.trim().to_owned()));
    }

    Ok(Options {
        root,
        glob: cli.glob.clone(),
        endpoint,
        headers,
        concurrency: cli.concurrency,
        page_size: cli.page_size,
        retries: cli.retries,
        delay: Duration::from_millis(cli.delay),
        connect_timeout: Duration::from_secs(cli.connect_timeout),
        request_timeout: Duration::from_secs(cli.timeout),
        file_timeout: Duration::from_secs(cli.file_timeout),
        max_pages: cli.max_pages,
        max_response_bytes: cli.max_response_mb * 1024 * 1024,
        allow_partial: cli.allow_partial,
        envelope: cli.envelope,
        check: cli.check,
        dry_run: cli.dry_run,
        logger,
    })
}
