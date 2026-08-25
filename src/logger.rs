/// Logging injected into the engine.
///
/// The library NEVER writes to stdout directly: in service mode stdout is the RPC channel,
/// and a stray `println!` would corrupt the protocol (see `pagefind/src/service/mod.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Silent,
    Quiet,
    Standard,
    Verbose,
}

#[derive(Debug, Clone)]
pub struct Logger {
    level: Level,
    to_terminal: bool,
}

impl Logger {
    pub fn new(level: Level, to_terminal: bool) -> Self {
        Self { level, to_terminal }
    }

    pub fn status(&self, msg: &str) {
        self.emit(Level::Standard, msg);
    }

    pub fn warn(&self, msg: &str) {
        self.emit(Level::Quiet, msg);
    }

    pub fn verbose(&self, msg: &str) {
        self.emit(Level::Verbose, msg);
    }

    fn emit(&self, required: Level, msg: &str) {
        if self.to_terminal && self.level >= required {
            eprintln!("{msg}");
        }
    }
}

impl Default for Logger {
    fn default() -> Self {
        Self::new(Level::Standard, true)
    }
}
