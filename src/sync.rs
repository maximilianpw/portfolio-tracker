use std::path::{Path, PathBuf};

use chrono::{NaiveDate, Utc};
use chrono_tz::Europe::Berlin;
use sqlx::SqlitePool;
use tracing::{error, info, warn};

use crate::corrections::{self, Correction};
use crate::db::{self, ImportStats, PortfolioSnapshot, RunStatus};
use crate::error::Error;
use crate::instruments::Instruments;
use crate::ledger::{Account, AccountMap, Posting};
use crate::prices::{self, PytrRates};
use crate::pytr::Pytr;
use crate::reconcile::{consistency, reconcile};
use crate::trade_republic::{self, Cookies, Endpoints};
use crate::tradingview;
use crate::valuation::Ledger;

pub struct SyncConfig {
    pub data_dir: PathBuf,
    pub pytr: Pytr,
    pub endpoints: Endpoints,
    pub cookies: Option<PathBuf>,
    pub instruments: Option<PathBuf>,
    pub corrections: Option<PathBuf>,
    pub recent_days: u32,
}

pub fn today() -> NaiveDate {
    Utc::now().with_timezone(&Berlin).date_naive()
}

pub async fn import_file(pool: &SqlitePool, path: &Path) -> Result<ImportStats, Error> {
    let raw: Vec<serde_json::Value> = serde_json::from_slice(&tokio::fs::read(path).await?)?;
    db::import_events(pool, &raw).await
}

pub async fn load_postings(
    pool: &SqlitePool,
    corrections: &[Correction],
) -> Result<Vec<Posting>, Error> {
    let mut postings = db::load_postings(pool).await?;
    postings.extend(corrections::to_postings(corrections));
    Ok(postings)
}

pub async fn load_ledger(pool: &SqlitePool, corrections: &[Correction]) -> Result<Ledger, Error> {
    Ok(Ledger::new(
        load_postings(pool, corrections).await?,
        db::load_prices(pool).await?,
        db::instrument_names(pool).await?,
    ))
}

async fn write_atomically(path: &Path, contents: &str) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, contents).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

pub async fn export_tradingview(
    pool: &SqlitePool,
    instruments: &Path,
    corrections: &[Correction],
    out_dir: &Path,
) -> Result<Vec<(Account, usize)>, Error> {
    let instruments = Instruments::load(instruments)?;
    let postings = load_postings(pool, corrections).await?;
    let mut written = Vec::new();
    for account in Account::ALL {
        let export = tradingview::export(&postings, account, &instruments)?;
        for skipped in &export.skipped {
            warn!(%account, skipped, "no TradingView equivalent, left out of the CSV");
        }
        write_atomically(&out_dir.join(format!("{account}.csv")), &export.csv).await?;
        written.push((account, export.rows));
    }
    Ok(written)
}

/// Holdings and cash per account, straight from Trade Republic, using pytr's saved session.
pub async fn fetch_account_snapshots(
    pool: &SqlitePool,
    endpoints: &Endpoints,
    cookies: Option<&Path>,
) -> Result<Vec<(Account, PortfolioSnapshot)>, Error> {
    let jar = match cookies {
        Some(path) => path.to_owned(),
        None => {
            let home = std::env::var_os("HOME").ok_or_else(|| Error::Cookies {
                path: "~/.pytr".into(),
                message: "HOME is not set".into(),
            })?;
            Cookies::find(Path::new(&home))?
        }
    };
    let known = db::load_account_map(pool).await?;
    let mut snapshots = Vec::new();
    for (pair, snapshot) in
        trade_republic::fetch_snapshots(endpoints, &Cookies::load(&jar)?).await?
    {
        match account_for(&known, &pair.cash_account_number, &pair.product_type) {
            Some(account) => snapshots.push((account, snapshot)),
            None => warn!(
                product = pair.product_type,
                "unknown account type, not reconciled"
            ),
        }
    }
    Ok(snapshots)
}

fn account_for(known: &AccountMap, cash_account: &str, product_type: &str) -> Option<Account> {
    if known.pea_cash_account.as_deref() == Some(cash_account) {
        return Some(Account::Pea);
    }
    if known.regular_cash_account.as_deref() == Some(cash_account) {
        return Some(Account::Regular);
    }
    match product_type {
        "DEFAULT" => Some(Account::Regular),
        "TAX_WRAPPER" => Some(Account::Pea),
        _ => None,
    }
}

pub async fn sync(pool: &SqlitePool, config: &SyncConfig) -> Result<(), Error> {
    let run_id = db::start_run(pool).await?;
    match run_steps(pool, config, run_id).await {
        Ok((complete, export_error)) => {
            let status = if export_error.is_some() {
                RunStatus::Partial
            } else {
                RunStatus::Ok
            };
            db::finish_run(pool, run_id, status, export_error.as_deref(), complete).await?;
            info!(run_id, complete, "sync finished");
            Ok(())
        }
        Err(err) => {
            error!(run_id, %err, "sync failed");
            db::finish_run(
                pool,
                run_id,
                RunStatus::Failed,
                Some(&err.to_string()),
                false,
            )
            .await?;
            Err(err)
        }
    }
}

async fn run_steps(
    pool: &SqlitePool,
    config: &SyncConfig,
    run_id: i64,
) -> Result<(bool, Option<String>), Error> {
    let corrections = corrections::load_optional(config.corrections.as_deref())?;

    db::set_run_step(pool, run_id, "ingest").await?;
    let events = config
        .pytr
        .export_events(&config.data_dir.join("pytr"), config.recent_days)
        .await?;

    db::set_run_step(pool, run_id, "import").await?;
    let stats = import_file(pool, &events).await?;
    info!(
        events = stats.events,
        postings = stats.postings,
        ignored = stats.ignored,
        "imported"
    );

    db::set_run_step(pool, run_id, "reconcile").await?;
    let date = today();
    let snapshots =
        fetch_account_snapshots(pool, &config.endpoints, config.cookies.as_deref()).await?;
    let ledger = load_ledger(pool, &corrections).await?;
    let mut issues = Vec::new();
    for (account, snapshot) in &snapshots {
        db::save_snapshot(pool, run_id, date, *account, snapshot).await?;
        issues.extend(reconcile(&ledger, *account, date, snapshot));
    }
    for account in Account::ALL {
        issues.extend(consistency(&ledger, account, date));
    }
    for issue in &issues {
        warn!(account = %issue.account, isin = ?issue.isin, derived = ?issue.derived, reported = ?issue.reported, issue = issue.issue, "reconciliation");
    }
    db::save_issues(pool, run_id, &issues).await?;

    db::set_run_step(pool, run_id, "prices").await?;
    let isins = ledger.held_isins(date);
    let source = PytrRates {
        pytr: config.pytr.clone(),
        dir: config.data_dir.join("prices"),
    };
    let price_stats = prices::collect(pool, &source, &isins, Utc::now()).await?;
    if !price_stats.missing.is_empty() {
        warn!(missing = ?price_stats.missing, "no price today");
    }
    info!(stored = price_stats.stored, "prices collected");

    let mut export_error = None;
    if let Some(instruments) = &config.instruments {
        db::set_run_step(pool, run_id, "export").await?;
        match export_tradingview(
            pool,
            instruments,
            &corrections,
            &config.data_dir.join("tradingview"),
        )
        .await
        {
            Ok(written) => info!(?written, "TradingView CSVs written"),
            Err(err) => {
                warn!(%err, "TradingView export failed");
                export_error = Some(err.to_string());
            }
        }
    }

    Ok((
        issues.is_empty() && price_stats.missing.is_empty(),
        export_error,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_accounts_by_cash_number_then_product_type() {
        let known = AccountMap {
            pea_cash_account: Some("P".into()),
            regular_cash_account: Some("R".into()),
        };
        assert_eq!(account_for(&known, "P", "DEFAULT"), Some(Account::Pea));
        assert_eq!(account_for(&known, "X", "TAX_WRAPPER"), Some(Account::Pea));
        assert_eq!(
            account_for(&AccountMap::default(), "X", "DEFAULT"),
            Some(Account::Regular)
        );
        assert_eq!(account_for(&AccountMap::default(), "X", "CRYPTO"), None);
    }
}
