use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use chrono::{NaiveDate, Utc};
use rust_decimal::Decimal;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

use crate::error::Error;
use crate::event::RawEvent;
use crate::ledger::{
    Account, AccountMap, Kind, Posting, Status, postings_for, resolve_corporate_actions,
};

pub async fn connect(path: &Path) -> Result<SqlitePool, Error> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(10))
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await?;
    sqlx::migrate!().run(&pool).await?;
    Ok(pool)
}

fn now() -> String {
    Utc::now().to_rfc3339()
}

fn parse_decimal(column: &str, value: &str) -> Result<Decimal, Error> {
    Decimal::from_str(value).map_err(|e| {
        Error::Database(sqlx::Error::Decode(
            format!("{column}={value:?}: {e}").into(),
        ))
    })
}

fn parse_optional_decimal(column: &str, value: Option<String>) -> Result<Option<Decimal>, Error> {
    value.map(|v| parse_decimal(column, &v)).transpose()
}

fn decode_error(message: String) -> Error {
    Error::Database(sqlx::Error::Decode(message.into()))
}

pub async fn load_account_map(pool: &SqlitePool) -> Result<AccountMap, Error> {
    let rows = sqlx::query!("SELECT id, cash_account_number FROM accounts")
        .fetch_all(pool)
        .await?;
    let mut map = AccountMap::default();
    for row in rows {
        match row.id.as_str() {
            "pea" => map.pea_cash_account = row.cash_account_number,
            _ => map.regular_cash_account = row.cash_account_number,
        }
    }
    Ok(map)
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ImportStats {
    pub events: usize,
    pub postings: usize,
    pub ignored: usize,
}

/// Writes all events and their postings in one transaction. Re-importing the same events
/// replaces their postings, so a revised event is updated in place.
pub async fn import_events(
    pool: &SqlitePool,
    raw: &[serde_json::Value],
) -> Result<ImportStats, Error> {
    let events: Vec<RawEvent> = raw
        .iter()
        .map(|v| serde_json::from_value(v.clone()))
        .collect::<Result<_, _>>()?;

    let accounts = AccountMap::detect(&events, &load_account_map(pool).await?)?;
    let mut parsed = Vec::with_capacity(events.len());
    for event in &events {
        parsed.push(postings_for(event, &accounts)?);
    }
    resolve_corporate_actions(&events, &mut parsed);

    let mut tx = pool.begin().await?;
    for (id, cash) in [
        ("regular", &accounts.regular_cash_account),
        ("pea", &accounts.pea_cash_account),
    ] {
        sqlx::query!(
            "INSERT INTO accounts (id, cash_account_number) VALUES (?, ?)
             ON CONFLICT (id) DO UPDATE SET cash_account_number = COALESCE(excluded.cash_account_number, accounts.cash_account_number)",
            id,
            cash
        )
        .execute(&mut *tx)
        .await?;
    }

    let imported_at = now();
    let mut stats = ImportStats {
        events: events.len(),
        ..ImportStats::default()
    };
    for ((event, value), postings) in events.iter().zip(raw).zip(&parsed) {
        let raw_json = value.to_string();
        sqlx::query!(
            "INSERT INTO raw_events (id, ts, event_type, status, raw_json, imported_at) VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT (id) DO UPDATE SET ts = excluded.ts, event_type = excluded.event_type,
                 status = excluded.status, raw_json = excluded.raw_json, imported_at = excluded.imported_at",
            event.id,
            event.timestamp,
            event.event_type,
            event.status,
            raw_json,
            imported_at
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!("DELETE FROM transactions WHERE event_id = ?", event.id)
            .execute(&mut *tx)
            .await?;

        if postings.is_empty() {
            stats.ignored += 1;
        }
        for p in postings {
            if let (Some(isin), Some(name)) = (&p.isin, &p.instrument_name) {
                sqlx::query!(
                    "INSERT INTO instruments (isin, name) VALUES (?, ?) ON CONFLICT (isin) DO UPDATE SET name = excluded.name",
                    isin,
                    name
                )
                .execute(&mut *tx)
                .await?;
            }
            let account = p.account.as_str();
            let date = p.date.to_string();
            let kind = p.kind.as_str();
            let quantity = p.quantity.map(|q| q.normalize().to_string());
            let price = p.price.map(|q| q.normalize().to_string());
            let amount = p.amount.to_string();
            let fee = p.fee.to_string();
            let tax = p.tax.to_string();
            let status = p.status.as_str();
            sqlx::query!(
                "INSERT INTO transactions (event_id, leg, account, ts, date, isin, kind, quantity, price, amount, fee, tax, saving_plan, status)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                p.event_id,
                p.leg,
                account,
                p.ts,
                date,
                p.isin,
                kind,
                quantity,
                price,
                amount,
                fee,
                tax,
                p.saving_plan,
                status
            )
            .execute(&mut *tx)
            .await?;
            stats.postings += 1;
        }
    }
    tx.commit().await?;
    Ok(stats)
}

pub async fn load_postings(pool: &SqlitePool) -> Result<Vec<Posting>, Error> {
    let rows = sqlx::query!(
        r#"SELECT t.event_id, t.leg, t.account, t.ts, t.date, t.isin, i.name AS "instrument_name?", t.kind,
                  t.quantity, t.price, t.amount, t.fee, t.tax, t.saving_plan, t.status
           FROM transactions t LEFT JOIN instruments i ON i.isin = t.isin
           ORDER BY t.ts, t.event_id, t.leg"#
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|r| {
            Ok(Posting {
                account: Account::from_str(&r.account).map_err(decode_error)?,
                date: NaiveDate::from_str(&r.date).map_err(|e| decode_error(e.to_string()))?,
                kind: Kind::from_str(&r.kind).map_err(decode_error)?,
                quantity: parse_optional_decimal("quantity", r.quantity)?,
                price: parse_optional_decimal("price", r.price)?,
                amount: parse_decimal("amount", &r.amount)?,
                fee: parse_decimal("fee", &r.fee)?,
                tax: parse_decimal("tax", &r.tax)?,
                saving_plan: r.saving_plan != 0,
                status: if r.status == "cancelled" {
                    Status::Cancelled
                } else {
                    Status::Active
                },
                event_id: r.event_id,
                leg: r.leg,
                ts: r.ts,
                isin: r.isin,
                instrument_name: r.instrument_name,
            })
        })
        .collect()
}

pub async fn instrument_names(pool: &SqlitePool) -> Result<BTreeMap<String, String>, Error> {
    Ok(sqlx::query!("SELECT isin, name FROM instruments")
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|r| (r.isin, r.name))
        .collect())
}

#[derive(Debug, Clone, PartialEq)]
pub struct PriceRow {
    pub isin: String,
    pub date: NaiveDate,
    pub price: Decimal,
    pub currency: String,
    pub basis: String,
    pub observed_at: String,
    pub venue: Option<String>,
    pub source: String,
}

pub async fn upsert_price(pool: &SqlitePool, p: &PriceRow) -> Result<(), Error> {
    let date = p.date.to_string();
    let price = p.price.normalize().to_string();
    sqlx::query!(
        "INSERT INTO prices (isin, date, price, currency, basis, observed_at, venue, source) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (isin, date) DO UPDATE SET price = excluded.price, currency = excluded.currency, basis = excluded.basis,
             observed_at = excluded.observed_at, venue = excluded.venue, source = excluded.source",
        p.isin,
        date,
        price,
        p.currency,
        p.basis,
        p.observed_at,
        p.venue,
        p.source
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn load_prices(pool: &SqlitePool) -> Result<Vec<PriceRow>, Error> {
    sqlx::query!("SELECT isin, date, price, currency, basis, observed_at, venue, source FROM prices ORDER BY isin, date")
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|r| {
            Ok(PriceRow {
                date: NaiveDate::from_str(&r.date).map_err(|e| decode_error(e.to_string()))?,
                price: parse_decimal("price", &r.price)?,
                isin: r.isin,
                currency: r.currency,
                basis: r.basis,
                observed_at: r.observed_at,
                venue: r.venue,
                source: r.source,
            })
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Ok,
    Partial,
    Failed,
}

impl RunStatus {
    fn as_str(self) -> &'static str {
        match self {
            RunStatus::Ok => "ok",
            RunStatus::Partial => "partial",
            RunStatus::Failed => "failed",
        }
    }
}

pub async fn start_run(pool: &SqlitePool) -> Result<i64, Error> {
    let started = now();
    let id = sqlx::query!(
        "INSERT INTO sync_runs (started_at, status) VALUES (?, 'running')",
        started
    )
    .execute(pool)
    .await?
    .last_insert_rowid();
    Ok(id)
}

pub async fn set_run_step(pool: &SqlitePool, run_id: i64, step: &str) -> Result<(), Error> {
    sqlx::query!("UPDATE sync_runs SET step = ? WHERE id = ?", step, run_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn finish_run(
    pool: &SqlitePool,
    run_id: i64,
    status: RunStatus,
    error: Option<&str>,
    complete: bool,
) -> Result<(), Error> {
    let finished = now();
    let status = status.as_str();
    sqlx::query!(
        "UPDATE sync_runs SET finished_at = ?, status = ?, error = ?, complete = ? WHERE id = ?",
        finished,
        status,
        error,
        complete,
        run_id
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunInfo {
    pub id: i64,
    pub finished_at: Option<String>,
    pub status: String,
    pub complete: bool,
    pub error: Option<String>,
}

pub async fn last_successful_run(pool: &SqlitePool) -> Result<Option<RunInfo>, Error> {
    Ok(sqlx::query!(
        "SELECT id, finished_at, status, complete, error FROM sync_runs
         WHERE status IN ('ok', 'partial') ORDER BY id DESC LIMIT 1"
    )
    .fetch_optional(pool)
    .await?
    .map(|r| RunInfo {
        id: r.id,
        finished_at: r.finished_at,
        status: r.status,
        complete: r.complete != 0,
        error: r.error,
    }))
}

#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotPosition {
    pub isin: String,
    pub quantity: Decimal,
    pub avg_cost: Option<Decimal>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PortfolioSnapshot {
    pub positions: Vec<SnapshotPosition>,
    pub cash: Option<Decimal>,
}

pub async fn save_snapshot(
    pool: &SqlitePool,
    run_id: i64,
    date: NaiveDate,
    account: Account,
    snapshot: &PortfolioSnapshot,
) -> Result<(), Error> {
    let date = date.to_string();
    let account = account.as_str();
    let mut tx = pool.begin().await?;
    for p in &snapshot.positions {
        let quantity = p.quantity.normalize().to_string();
        let avg_cost = p.avg_cost.map(|q| q.normalize().to_string());
        sqlx::query!(
            "INSERT INTO positions_snapshots (run_id, snapshot_date, account, isin, quantity, avg_cost) VALUES (?, ?, ?, ?, ?, ?)",
            run_id,
            date,
            account,
            p.isin,
            quantity,
            avg_cost
        )
        .execute(&mut *tx)
        .await?;
    }
    if let Some(cash) = snapshot.cash {
        let balance = cash.to_string();
        sqlx::query!(
            "INSERT INTO cash_snapshots (run_id, snapshot_date, account, balance) VALUES (?, ?, ?, ?)",
            run_id,
            date,
            account,
            balance
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// The snapshots from the most recent run that recorded any, one per account.
pub async fn load_latest_snapshots(
    pool: &SqlitePool,
) -> Result<Vec<(Account, PortfolioSnapshot)>, Error> {
    let run_id = sqlx::query_scalar!(
        "SELECT max(run_id) FROM (SELECT run_id FROM positions_snapshots UNION SELECT run_id FROM cash_snapshots)"
    )
    .fetch_one(pool)
    .await?;
    let Some(run_id) = run_id else {
        return Ok(Vec::new());
    };

    let mut snapshots: BTreeMap<Account, PortfolioSnapshot> = BTreeMap::new();
    for r in sqlx::query!(
        "SELECT account, isin, quantity, avg_cost FROM positions_snapshots WHERE run_id = ? ORDER BY account, isin",
        run_id
    )
    .fetch_all(pool)
    .await?
    {
        let account = Account::from_str(&r.account).map_err(decode_error)?;
        let quantity = r.quantity.ok_or_else(|| decode_error(format!("snapshot of {} without quantity", r.isin)))?;
        snapshots
            .entry(account)
            .or_insert_with(|| PortfolioSnapshot { positions: Vec::new(), cash: None })
            .positions
            .push(SnapshotPosition {
                quantity: parse_decimal("quantity", &quantity)?,
                avg_cost: parse_optional_decimal("avg_cost", r.avg_cost)?,
                isin: r.isin,
            });
    }
    for r in sqlx::query!(
        "SELECT account, balance FROM cash_snapshots WHERE run_id = ?",
        run_id
    )
    .fetch_all(pool)
    .await?
    {
        let account = Account::from_str(&r.account).map_err(decode_error)?;
        snapshots
            .entry(account)
            .or_insert_with(|| PortfolioSnapshot {
                positions: Vec::new(),
                cash: None,
            })
            .cash = Some(parse_decimal("balance", &r.balance)?);
    }
    Ok(snapshots.into_iter().collect())
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ReconciliationIssue {
    pub account: Account,
    pub isin: Option<String>,
    pub derived: Option<Decimal>,
    pub reported: Option<Decimal>,
    pub issue: String,
}

pub async fn save_issues(
    pool: &SqlitePool,
    run_id: i64,
    issues: &[ReconciliationIssue],
) -> Result<(), Error> {
    let mut tx = pool.begin().await?;
    for i in issues {
        let account = i.account.as_str();
        let derived = i.derived.map(|d| d.normalize().to_string());
        let reported = i.reported.map(|d| d.normalize().to_string());
        sqlx::query!(
            "INSERT INTO reconciliation_issues (run_id, account, isin, derived, reported, issue) VALUES (?, ?, ?, ?, ?, ?)",
            run_id,
            account,
            i.isin,
            derived,
            reported,
            i.issue
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn load_issues(
    pool: &SqlitePool,
    run_id: i64,
) -> Result<Vec<ReconciliationIssue>, Error> {
    sqlx::query!(
        "SELECT account, isin, derived, reported, issue FROM reconciliation_issues WHERE run_id = ?",
        run_id
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|r| {
        Ok(ReconciliationIssue {
            account: Account::from_str(&r.account).map_err(decode_error)?,
            isin: r.isin,
            derived: parse_optional_decimal("derived", r.derived)?,
            reported: parse_optional_decimal("reported", r.reported)?,
            issue: r.issue,
        })
    })
    .collect()
}
