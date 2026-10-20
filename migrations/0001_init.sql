-- Money and quantities are TEXT holding exact decimals; never aggregate them in SQL.

CREATE TABLE accounts (
    id TEXT NOT NULL PRIMARY KEY CHECK (id IN ('regular', 'pea')),
    cash_account_number TEXT UNIQUE
);

CREATE TABLE raw_events (
    id TEXT NOT NULL PRIMARY KEY,
    ts TEXT NOT NULL,
    event_type TEXT,
    status TEXT,
    raw_json TEXT NOT NULL,
    imported_at TEXT NOT NULL
);

CREATE TABLE instruments (
    isin TEXT NOT NULL PRIMARY KEY,
    name TEXT NOT NULL
);

CREATE TABLE transactions (
    event_id TEXT NOT NULL REFERENCES raw_events (id) ON DELETE CASCADE,
    leg INTEGER NOT NULL,
    account TEXT NOT NULL REFERENCES accounts (id),
    ts TEXT NOT NULL,
    date TEXT NOT NULL,
    isin TEXT REFERENCES instruments (isin),
    kind TEXT NOT NULL,
    quantity TEXT,
    price TEXT,
    amount TEXT NOT NULL,
    fee TEXT NOT NULL,
    tax TEXT NOT NULL,
    saving_plan INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL CHECK (status IN ('active', 'cancelled')),
    PRIMARY KEY (event_id, leg)
);

CREATE INDEX transactions_date ON transactions (date);
CREATE INDEX transactions_isin ON transactions (isin);

CREATE TABLE sync_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at TEXT NOT NULL,
    finished_at TEXT,
    status TEXT NOT NULL CHECK (status IN ('running', 'ok', 'partial', 'failed')),
    step TEXT,
    error TEXT,
    complete INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE positions_snapshots (
    run_id INTEGER NOT NULL REFERENCES sync_runs (id),
    snapshot_date TEXT NOT NULL,
    account TEXT NOT NULL REFERENCES accounts (id),
    isin TEXT NOT NULL,
    quantity TEXT,
    avg_cost TEXT,
    PRIMARY KEY (run_id, account, isin)
);

CREATE TABLE cash_snapshots (
    run_id INTEGER NOT NULL REFERENCES sync_runs (id),
    snapshot_date TEXT NOT NULL,
    account TEXT NOT NULL REFERENCES accounts (id),
    balance TEXT NOT NULL,
    PRIMARY KEY (run_id, account)
);

CREATE TABLE reconciliation_issues (
    run_id INTEGER NOT NULL REFERENCES sync_runs (id),
    account TEXT NOT NULL,
    isin TEXT,
    derived TEXT,
    reported TEXT,
    issue TEXT NOT NULL
);

CREATE TABLE prices (
    isin TEXT NOT NULL,
    date TEXT NOT NULL,
    price TEXT NOT NULL,
    currency TEXT NOT NULL,
    basis TEXT NOT NULL CHECK (basis IN ('last', 'close')),
    observed_at TEXT NOT NULL,
    venue TEXT,
    source TEXT NOT NULL,
    PRIMARY KEY (isin, date)
);
