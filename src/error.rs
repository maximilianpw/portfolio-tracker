#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("pytr session expired: run `pytr login --store_credentials --v2` as the service user")]
    SessionExpired,
    #[error("pytr {command} failed: {message}")]
    Pytr {
        command: &'static str,
        message: String,
    },
    #[error("cannot start pytr at {path}: {message}")]
    PytrUnavailable { path: String, message: String },
    #[error("pytr {command} timed out after {seconds}s")]
    PytrTimeout { command: &'static str, seconds: u64 },
    #[error("cannot parse event {event_id} ({event_type}): {reason}")]
    Parse {
        event_id: String,
        event_type: String,
        reason: String,
    },
    #[error(
        "unknown event type {event_type:?} in event {event_id} ({title}); add it to the ledger or the ignore list"
    )]
    UnknownEvent {
        event_id: String,
        event_type: String,
        title: String,
    },
    #[error("account detection failed: {0}")]
    Accounts(String),
    #[error("no TradingView symbol in {path} for: {isins}")]
    UnmappedInstruments { path: String, isins: String },
    #[error(
        "no cost per share for transferred shares of {isins}; add `price` from the old bank's statement to the corrections file"
    )]
    MissingCost { isins: String },
    #[error("invalid instruments file {path}: {message}")]
    InstrumentsFile { path: String, message: String },
    #[error("cannot use pytr's session cookies at {path}: {message}")]
    Cookies { path: String, message: String },
    #[error("Trade Republic: {0}")]
    TradeRepublic(String),
    #[error("invalid corrections file {path}: {message}")]
    CorrectionsFile { path: String, message: String },
    #[error("unreadable pytr output: {0}")]
    PytrOutput(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Migration(#[from] sqlx::migrate::MigrateError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Csv(#[from] csv::Error),
}
