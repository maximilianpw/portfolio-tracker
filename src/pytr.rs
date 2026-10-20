use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::FromStr;
use std::time::Duration;

use rust_decimal::Decimal;
use tokio::process::Command;
use tracing::{info, warn};

use crate::error::Error;

const LOGIN_MARKERS: &[&str] = &[
    "Initiating web login",
    "Please enter your TradeRepublic",
    "Enter the code",
];
const RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(30), Duration::from_secs(120)];

/// The only pytr subcommands this program runs. pytr can also place orders and pay out cash;
/// none of that is reachable from here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Subcommand {
    ExportTransactions,
    Rates,
}

impl Subcommand {
    fn name(self) -> &'static str {
        match self {
            Subcommand::ExportTransactions => "export_transactions",
            Subcommand::Rates => "rates",
        }
    }

    fn timeout(self) -> Duration {
        match self {
            Subcommand::ExportTransactions => Duration::from_secs(600),
            Subcommand::Rates => Duration::from_secs(180),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Pytr {
    binary: PathBuf,
    retries: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Rate {
    pub isin: String,
    pub price: Decimal,
}

impl Pytr {
    pub fn new(binary: PathBuf) -> Pytr {
        Pytr {
            binary,
            retries: true,
        }
    }

    pub fn without_retries(mut self) -> Pytr {
        self.retries = false;
        self
    }

    async fn run(&self, sub: Subcommand, args: &[String]) -> Result<String, Error> {
        let mut attempt = 0;
        loop {
            match self.run_once(sub, args).await {
                Ok(out) => return Ok(out),
                Err(err @ (Error::SessionExpired | Error::PytrUnavailable { .. })) => {
                    return Err(err);
                }
                Err(err) if self.retries && attempt < RETRY_DELAYS.len() => {
                    warn!(command = sub.name(), %err, "pytr failed, retrying");
                    tokio::time::sleep(RETRY_DELAYS[attempt]).await;
                    attempt += 1;
                }
                Err(err) => return Err(err),
            }
        }
    }

    async fn run_once(&self, sub: Subcommand, args: &[String]) -> Result<String, Error> {
        info!(command = sub.name(), "running pytr");
        let child = Command::new(&self.binary)
            .arg(sub.name())
            .arg("--v2")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| Error::PytrUnavailable {
                path: self.binary.display().to_string(),
                message: e.to_string(),
            })?;

        let output = tokio::time::timeout(sub.timeout(), child.wait_with_output())
            .await
            .map_err(|_| Error::PytrTimeout {
                command: sub.name(),
                seconds: sub.timeout().as_secs(),
            })??;

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let combined = format!("{stdout}\n{stderr}");
        if LOGIN_MARKERS.iter().any(|m| combined.contains(m)) && !output.status.success() {
            return Err(Error::SessionExpired);
        }
        if !output.status.success() {
            let tail: Vec<&str> = stderr.lines().rev().take(5).collect();
            return Err(Error::Pytr {
                command: sub.name(),
                message: format!(
                    "{}: {}",
                    output.status,
                    tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
                ),
            });
        }
        Ok(combined)
    }

    /// Updates the cumulative `all_events.json` in `dir`: a full fetch the first time, the
    /// last `recent_days` afterwards.
    pub async fn export_events(&self, dir: &Path, recent_days: u32) -> Result<PathBuf, Error> {
        tokio::fs::create_dir_all(dir).await?;
        let events = dir.join("all_events.json");
        let days = if events.exists() { recent_days } else { 0 };
        self.run(
            Subcommand::ExportTransactions,
            &[
                "--last_days".into(),
                days.to_string(),
                "--store-event-database".into(),
                "--outputdir".into(),
                dir.display().to_string(),
                "--format".into(),
                "json".into(),
            ],
        )
        .await?;
        if !events.exists() {
            return Err(Error::PytrOutput(format!(
                "{} was not written",
                events.display()
            )));
        }
        Ok(events)
    }

    pub async fn rates(&self, dir: &Path, isins: &[String]) -> Result<Vec<Rate>, Error> {
        if isins.is_empty() {
            return Ok(Vec::new());
        }
        tokio::fs::create_dir_all(dir).await?;
        let csv_path = dir.join("rates.csv");
        let mut args: Vec<String> = isins.to_vec();
        args.extend(["-o".into(), csv_path.display().to_string()]);
        self.run(Subcommand::Rates, &args).await?;
        parse_rates(&tokio::fs::read_to_string(&csv_path).await?)
    }
}

fn semicolon_reader(text: &str) -> csv::Reader<&[u8]> {
    csv::ReaderBuilder::new()
        .delimiter(b';')
        .from_reader(text.as_bytes())
}

fn field(record: &csv::StringRecord, headers: &csv::StringRecord, name: &str) -> Option<String> {
    headers
        .iter()
        .position(|h| h == name)
        .and_then(|i| record.get(i))
        .map(str::to_owned)
}

pub fn parse_rates(csv: &str) -> Result<Vec<Rate>, Error> {
    let mut reader = semicolon_reader(csv);
    let headers = reader.headers()?.clone();
    let mut rates = Vec::new();
    for record in reader.records() {
        let record = record?;
        let (Some(isin), Some(price)) = (
            field(&record, &headers, "ISIN"),
            field(&record, &headers, "price"),
        ) else {
            continue;
        };
        if let Ok(price) = Decimal::from_str(&price) {
            rates.push(Rate { isin, price });
        }
    }
    Ok(rates)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::d;

    #[test]
    fn parses_rates() {
        let rates =
            parse_rates("Name;ISIN;price;ask\nApple;US0378331005;200.1;200.3\nBroken;XX;n/a;\n")
                .unwrap();
        assert_eq!(
            rates,
            vec![Rate {
                isin: "US0378331005".into(),
                price: d("200.1")
            }]
        );
    }

    #[tokio::test]
    async fn detects_session_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-pytr");
        std::fs::write(
            &script,
            "#!/bin/sh\necho 'Initiating web login...' >&2\nexit 1\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        let pytr = Pytr::new(script).without_retries();
        assert!(matches!(
            pytr.rates(dir.path(), &["US0378331005".to_owned()]).await,
            Err(Error::SessionExpired)
        ));
    }
}
