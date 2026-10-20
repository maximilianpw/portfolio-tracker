use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use portfolio_tracker::api::{AppState, router};
use portfolio_tracker::corrections;
use portfolio_tracker::db;
use portfolio_tracker::instruments::Instruments;
use portfolio_tracker::ledger::Account;
use portfolio_tracker::prices::{self, PytrRates};
use portfolio_tracker::pytr::Pytr;
use portfolio_tracker::sync::{
    self, SyncConfig, export_tradingview, fetch_account_snapshots, import_file, load_ledger,
    load_postings, today,
};
use portfolio_tracker::trade_republic::Endpoints;
use portfolio_tracker::valuation::{Ledger, Scope};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about = "Read-only Trade Republic portfolio tracker")]
struct Cli {
    /// Directory for the database, pytr exports and TradingView CSVs.
    #[arg(long, env = "PT_DATA_DIR", default_value = "data", global = true)]
    data_dir: PathBuf,

    /// Reviewed corrections for what the timeline doesn't contain, e.g. a PEA transfer.
    #[arg(long, env = "PT_CORRECTIONS", global = true)]
    corrections: Option<PathBuf>,

    /// pytr's cookie jar; defaults to the single ~/.pytr/cookies.*.txt.
    #[arg(long, env = "PT_PYTR_COOKIES", global = true)]
    cookies: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Fetch events, import, reconcile, collect prices and write the TradingView CSVs.
    Sync {
        #[arg(long, env = "PT_PYTR", default_value = "pytr")]
        pytr: PathBuf,
        #[arg(long, env = "PT_INSTRUMENTS")]
        instruments: Option<PathBuf>,
        /// Days of history to refetch once the event database exists.
        #[arg(long, default_value_t = 14)]
        recent_days: u32,
    },
    /// Print both accounts' holdings and cash as Trade Republic reports them.
    Snapshot,
    /// Print corrections.toml with entries proposed from the last sync's snapshots, keeping
    /// the corrections already in the file.
    CorrectionsTemplate,
    /// Import a pytr all_events.json without contacting Trade Republic.
    Import { file: PathBuf },
    /// Record today's price for every holding.
    Prices {
        #[arg(long, env = "PT_PYTR", default_value = "pytr")]
        pytr: PathBuf,
    },
    /// Write the TradingView import CSVs, one per account.
    Export {
        #[arg(long, env = "PT_INSTRUMENTS")]
        instruments: PathBuf,
        #[arg(long)]
        out_dir: Option<PathBuf>,
    },
    /// Print an instruments.toml skeleton for every ISIN in the ledger, keeping filled-in entries.
    InstrumentsTemplate {
        #[arg(long, env = "PT_INSTRUMENTS")]
        instruments: Option<PathBuf>,
    },
    /// Print value, period change and movers as JSON.
    Summary {
        #[arg(long, default_value = "week")]
        period: String,
        #[arg(long, default_value = "all")]
        account: String,
    },
    /// Serve the read-only JSON API.
    Serve {
        #[arg(long, env = "PT_LISTEN", default_value = "127.0.0.1:8787")]
        listen: String,
        #[arg(long, env = "PT_TOKEN_FILE")]
        token_file: PathBuf,
    },
}

fn period_days(period: &str) -> anyhow::Result<u64> {
    Ok(match period {
        "day" => 1,
        "week" => 7,
        "month" => 30,
        "year" => 365,
        other => bail!("unknown period {other:?}; use day, week, month or year"),
    })
}

fn scope(account: &str) -> anyhow::Result<Scope> {
    Ok(match account {
        "all" => Scope::All,
        other => Scope::Account(other.parse::<Account>().map_err(anyhow::Error::msg)?),
    })
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    tokio::select! {
        _ = ctrl_c => {}
        _ = term.recv() => {}
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    tokio::fs::create_dir_all(&cli.data_dir)
        .await
        .with_context(|| format!("creating {}", cli.data_dir.display()))?;
    let pool = db::connect(&cli.data_dir.join("portfolio.sqlite")).await?;
    let existing_corrections = corrections::load_optional(cli.corrections.as_deref())?;

    match cli.command {
        Command::Sync {
            pytr,
            instruments,
            recent_days,
        } => {
            let config = SyncConfig {
                data_dir: cli.data_dir.clone(),
                pytr: Pytr::new(pytr),
                endpoints: Endpoints::default(),
                cookies: cli.cookies.clone(),
                instruments,
                corrections: cli.corrections.clone(),
                recent_days,
            };
            sync::sync(&pool, &config).await?;
        }
        Command::Snapshot => {
            let snapshots =
                fetch_account_snapshots(&pool, &Endpoints::default(), cli.cookies.as_deref())
                    .await?;
            let out: Vec<_> = snapshots
                .iter()
                .map(|(account, s)| {
                    serde_json::json!({
                        "account": account,
                        "cash": s.cash,
                        "positions": s.positions.iter().map(|p| serde_json::json!({
                            "isin": p.isin, "quantity": p.quantity, "avg_cost": p.avg_cost,
                        })).collect::<Vec<_>>(),
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        Command::CorrectionsTemplate => {
            let snapshots = db::load_latest_snapshots(&pool).await?;
            if snapshots.is_empty() {
                bail!("no snapshots yet; run `sync` first");
            }
            let postings = load_postings(&pool, &existing_corrections).await?;
            let mut all = existing_corrections.clone();
            all.extend(corrections::propose(&postings, &snapshots, today()));
            print!("{}", corrections::render(&all));
        }
        Command::Import { file } => {
            let stats = import_file(&pool, &file).await?;
            println!(
                "imported {} events, {} postings, {} without postings",
                stats.events, stats.postings, stats.ignored
            );
        }
        Command::Prices { pytr } => {
            let ledger = load_ledger(&pool, &existing_corrections).await?;
            let isins = ledger.held_isins(today());
            let source = PytrRates {
                pytr: Pytr::new(pytr),
                dir: cli.data_dir.join("prices"),
            };
            let stats = prices::collect(&pool, &source, &isins, chrono::Utc::now()).await?;
            println!(
                "stored {} prices; missing: {:?}",
                stats.stored, stats.missing
            );
        }
        Command::Export {
            instruments,
            out_dir,
        } => {
            let out_dir = out_dir.unwrap_or_else(|| cli.data_dir.join("tradingview"));
            for (account, rows) in
                export_tradingview(&pool, &instruments, &existing_corrections, &out_dir).await?
            {
                println!(
                    "{}: {rows} rows",
                    out_dir.join(format!("{account}.csv")).display()
                );
            }
        }
        Command::InstrumentsTemplate { instruments } => {
            let existing = match instruments {
                Some(path) if path.exists() => Instruments::load(&path)?,
                _ => Instruments::default(),
            };
            print!("{}", existing.template(&db::instrument_names(&pool).await?));
        }
        Command::Summary { period, account } => {
            let (scope, days) = (scope(&account)?, period_days(&period)?);
            let ledger = load_ledger(&pool, &existing_corrections).await?;
            let end = ledger.latest_price_date().unwrap_or_else(today);
            let start = Ledger::period_start(end, days);
            let out = serde_json::json!({
                "value": ledger.value_at(scope, end),
                "change": ledger.period_change(scope, start, end),
                "movers": ledger.movers(scope, start, end),
            });
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        Command::Serve { listen, token_file } => {
            let token = tokio::fs::read_to_string(&token_file)
                .await
                .with_context(|| format!("reading {}", token_file.display()))?;
            let token = token.trim();
            if token.len() < 16 {
                bail!(
                    "API token in {} must be at least 16 characters",
                    token_file.display()
                );
            }
            let app = router(AppState {
                pool,
                token: Arc::from(token),
                corrections: cli.corrections.clone(),
            });
            let listener = tokio::net::TcpListener::bind(&listen)
                .await
                .with_context(|| format!("binding {listen}"))?;
            tracing::info!(%listen, "serving");
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown_signal())
                .await?;
        }
    }
    Ok(())
}
