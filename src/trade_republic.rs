use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::info;

use crate::db::{PortfolioSnapshot, SnapshotPosition};
use crate::error::Error;

const SESSION_PATH: &str = "/api/v1/auth/web/session";
const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    pub http: String,
    pub ws: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints {
            http: "https://api.traderepublic.com".into(),
            ws: "wss://api.traderepublic.com".into(),
        }
    }
}
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const COOKIE_DOMAIN: &str = "traderepublic.com";

/// Session cookies saved by `pytr login --store_credentials`, in Netscape cookie-jar format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cookies(Vec<(String, String)>);

impl Cookies {
    /// pytr names the jar after the phone number, so the file is found by pattern.
    pub fn find(home: &Path) -> Result<PathBuf, Error> {
        let dir = home.join(".pytr");
        let mut jars: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map_err(|e| Error::Cookies {
                path: dir.display().to_string(),
                message: e.to_string(),
            })?
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("cookies.") && n.ends_with(".txt"))
            })
            .collect();
        jars.sort();
        match jars.as_slice() {
            [jar] => Ok(jar.clone()),
            [] => Err(Error::SessionExpired),
            _ => Err(Error::Cookies {
                path: dir.display().to_string(),
                message: "more than one cookies.*.txt; pass the one to use explicitly".into(),
            }),
        }
    }

    pub fn load(path: &Path) -> Result<Cookies, Error> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::Cookies {
            path: path.display().to_string(),
            message: e.to_string(),
        })?;
        let cookies = Cookies::parse(&text);
        if cookies.0.is_empty() {
            return Err(Error::SessionExpired);
        }
        Ok(cookies)
    }

    pub fn parse(text: &str) -> Cookies {
        let cookies = text
            .lines()
            .filter_map(|line| {
                let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
                if line.starts_with('#') {
                    return None;
                }
                let fields: Vec<&str> = line.split('\t').collect();
                let [domain, _, _, _, _, name, value] = fields.as_slice() else {
                    return None;
                };
                domain
                    .trim_start_matches('.')
                    .ends_with(COOKIE_DOMAIN)
                    .then(|| ((*name).to_owned(), (*value).to_owned()))
            })
            .collect();
        Cookies(cookies)
    }

    fn set(&mut self, name: &str, value: &str) {
        match self.0.iter_mut().find(|(n, _)| n == name) {
            Some(cookie) => cookie.1 = value.to_owned(),
            None => self.0.push((name.to_owned(), value.to_owned())),
        }
    }

    /// The saved `tr_session` expires within minutes. Like pytr on every run, this trades the
    /// long-lived `tr_refresh` cookie for a fresh one, in memory only: pytr's jar is never written.
    pub async fn refresh(&mut self, http_base: &str) -> Result<(), Error> {
        let fail = |e: reqwest::Error| Error::TradeRepublic(format!("session refresh failed: {e}"));
        let response = http_client()
            .map_err(fail)?
            .get(format!("{http_base}{SESSION_PATH}"))
            .header(reqwest::header::COOKIE, self.header())
            .send()
            .await
            .map_err(fail)?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(Error::SessionExpired);
        }
        if !status.is_success() {
            return Err(Error::TradeRepublic(format!(
                "session refresh answered {status}"
            )));
        }
        for header in response.headers().get_all(reqwest::header::SET_COOKIE) {
            let pair = header
                .to_str()
                .ok()
                .and_then(|h| h.split(';').next())
                .and_then(|p| p.split_once('='));
            if let Some((name, value)) = pair {
                self.set(name.trim(), value.trim());
            }
        }
        Ok(())
    }

    fn header(&self) -> String {
        self.0
            .iter()
            .map(|(n, v)| format!("{n}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// Uses the same bundled root certificates as the WebSocket, so nothing depends on a system
/// certificate store being present (systemd sandboxes, Nix builds).
fn http_client() -> Result<reqwest::Client, reqwest::Error> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("ring supports the default TLS versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .user_agent(USER_AGENT)
        .timeout(REQUEST_TIMEOUT)
        .build()
}

/// The only topics this client subscribes to, all of them reads. Trade Republic's order,
/// savings plan and payout topics have no variant here.
enum Topic<'a> {
    AccountPairs,
    Portfolio { securities_account: &'a str },
    Cash { cash_account: &'a str },
}

impl Topic<'_> {
    fn payload(&self) -> Value {
        match self {
            Topic::AccountPairs => json!({"type": "accountPairs"}),
            Topic::Portfolio { securities_account } => {
                json!({"type": "compactPortfolioByType", "secAccNo": securities_account})
            }
            Topic::Cash { cash_account } => json!({"type": "cash", "accountNumber": cash_account}),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountPair {
    pub securities_account_number: String,
    pub cash_account_number: String,
    pub product_type: String,
}

#[derive(Deserialize)]
struct AccountPairs {
    accounts: Vec<AccountPair>,
}

#[derive(Deserialize)]
struct CompactPortfolio {
    categories: Vec<Category>,
}

#[derive(Deserialize)]
struct Category {
    positions: Vec<Position>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Position {
    isin: String,
    net_size: Value,
    average_buy_in: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CashBalance {
    account_number: String,
    currency_id: String,
    amount: Value,
}

/// Trade Republic sends some numbers as JSON strings and others as floats.
fn decimal(value: &Value) -> Option<Decimal> {
    match value {
        Value::String(s) => Decimal::from_str(s).ok(),
        Value::Number(n) => Decimal::from_str(&n.to_string()).ok(),
        _ => None,
    }
}

fn parse_portfolio(response: Value) -> Result<Vec<SnapshotPosition>, Error> {
    let portfolio: CompactPortfolio = serde_json::from_value(response)?;
    portfolio
        .categories
        .into_iter()
        .flat_map(|c| c.positions)
        .map(|p| {
            let quantity = decimal(&p.net_size).ok_or_else(|| {
                Error::TradeRepublic(format!("unreadable netSize for {}", p.isin))
            })?;
            Ok(SnapshotPosition {
                avg_cost: p.average_buy_in.as_ref().and_then(decimal),
                isin: p.isin,
                quantity,
            })
        })
        .collect()
}

fn parse_cash(response: Value, cash_account: &str) -> Result<Option<Decimal>, Error> {
    let balances: Vec<CashBalance> = serde_json::from_value(response)?;
    let Some(balance) = balances
        .into_iter()
        .find(|b| b.account_number == cash_account)
    else {
        return Ok(None);
    };
    if balance.currency_id != "EUR" {
        return Err(Error::TradeRepublic(format!(
            "cash in {}, only EUR is supported",
            balance.currency_id
        )));
    }
    decimal(&balance.amount)
        .map(|d| Some(d.round_dp(2)))
        .ok_or_else(|| Error::TradeRepublic("unreadable cash amount".into()))
}

fn is_auth_error(payload: &str) -> bool {
    ["AUTHENTICATION", "UNAUTHORIZED", "SESSION"]
        .iter()
        .any(|m| payload.to_uppercase().contains(m))
}

pub struct Client {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
}

impl Client {
    pub async fn connect(endpoint: &str, cookies: &Cookies) -> Result<Client, Error> {
        let mut request = endpoint
            .into_client_request()
            .map_err(|e| Error::TradeRepublic(format!("invalid endpoint {endpoint}: {e}")))?;
        let cookie = HeaderValue::from_str(&cookies.header()).map_err(|_| Error::Cookies {
            path: "pytr cookie jar".into(),
            message: "cookie not valid in a header".into(),
        })?;
        request.headers_mut().insert("Cookie", cookie);

        let (ws, _) =
            tokio::time::timeout(REQUEST_TIMEOUT, tokio_tungstenite::connect_async(request))
                .await
                .map_err(|_| Error::TradeRepublic("connection timed out".into()))?
                .map_err(|e| Error::TradeRepublic(format!("connection failed: {e}")))?;
        let mut client = Client { ws, next_id: 1 };

        let hello = json!({
            "locale": "de",
            "platformId": "webtrading",
            "platformVersion": "chrome - 94.0.4606",
            "clientId": "app.traderepublic.com",
            "clientVersion": "5582",
        });
        client.send(format!("connect 31 {hello}")).await?;
        match client.next_text().await?.as_str() {
            "connected" => Ok(client),
            other if is_auth_error(other) => Err(Error::SessionExpired),
            other => Err(Error::TradeRepublic(format!("connection refused: {other}"))),
        }
    }

    async fn send(&mut self, text: String) -> Result<(), Error> {
        self.ws
            .send(Message::text(text))
            .await
            .map_err(|e| Error::TradeRepublic(format!("send failed: {e}")))
    }

    async fn next_text(&mut self) -> Result<String, Error> {
        loop {
            let message = tokio::time::timeout(REQUEST_TIMEOUT, self.ws.next())
                .await
                .map_err(|_| Error::TradeRepublic("no answer within 30s".into()))?
                .ok_or_else(|| Error::TradeRepublic("connection closed".into()))?
                .map_err(|e| Error::TradeRepublic(format!("receive failed: {e}")))?;
            match message {
                Message::Text(text) => return Ok(text.to_string()),
                Message::Close(frame) => {
                    return Err(Error::TradeRepublic(format!(
                        "connection closed: {frame:?}"
                    )));
                }
                _ => {}
            }
        }
    }

    /// Subscribes, takes the first full answer and unsubscribes again. Later deltas (`D`) are
    /// never needed because every topic used here is read once.
    async fn request(&mut self, topic: Topic<'_>) -> Result<Value, Error> {
        let id = self.next_id.to_string();
        self.next_id += 1;
        self.send(format!("sub {id} {}", topic.payload())).await?;
        let answer = loop {
            let text = self.next_text().await?;
            let mut parts = text.splitn(3, ' ');
            let (Some(sub_id), Some(code)) = (parts.next(), parts.next()) else {
                continue;
            };
            if sub_id != id {
                continue;
            }
            let payload = parts.next().unwrap_or("").trim();
            match code {
                "A" => break serde_json::from_str(payload)?,
                "E" if is_auth_error(payload) => return Err(Error::SessionExpired),
                "E" => {
                    return Err(Error::TradeRepublic(format!(
                        "{}: {payload}",
                        topic.payload()
                    )));
                }
                "C" => {
                    return Err(Error::TradeRepublic(format!(
                        "{} closed without an answer",
                        topic.payload()
                    )));
                }
                _ => continue,
            }
        };
        self.send(format!("unsub {id}")).await?;
        Ok(answer)
    }

    pub async fn account_pairs(&mut self) -> Result<Vec<AccountPair>, Error> {
        let pairs: AccountPairs = serde_json::from_value(self.request(Topic::AccountPairs).await?)?;
        Ok(pairs.accounts)
    }

    pub async fn snapshot(&mut self, pair: &AccountPair) -> Result<PortfolioSnapshot, Error> {
        let positions = parse_portfolio(
            self.request(Topic::Portfolio {
                securities_account: &pair.securities_account_number,
            })
            .await?,
        )?;
        let cash = parse_cash(
            self.request(Topic::Cash {
                cash_account: &pair.cash_account_number,
            })
            .await?,
            &pair.cash_account_number,
        )?;
        Ok(PortfolioSnapshot { positions, cash })
    }

    pub async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }
}

/// Holdings and cash for every account of the login.
pub async fn fetch_snapshots(
    endpoints: &Endpoints,
    cookies: &Cookies,
) -> Result<Vec<(AccountPair, PortfolioSnapshot)>, Error> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut cookies = cookies.clone();
    cookies.refresh(&endpoints.http).await?;
    let mut client = Client::connect(&endpoints.ws, &cookies).await?;
    let pairs = client.account_pairs().await?;
    let mut snapshots = Vec::with_capacity(pairs.len());
    for pair in pairs {
        info!(product = pair.product_type, "fetching account snapshot");
        let snapshot = client.snapshot(&pair).await?;
        snapshots.push((pair, snapshot));
    }
    client.close().await;
    Ok(snapshots)
}

#[cfg(test)]
mod tests {
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

    use super::*;
    use crate::testing::d;

    #[test]
    fn parses_netscape_cookie_jar() {
        let jar = "# Netscape HTTP Cookie File\n\n\
                   api.traderepublic.com\tFALSE\t/\tTRUE\t\ttr_session\tabc\n\
                   #HttpOnly_.traderepublic.com\tTRUE\t/\tTRUE\t0\ttr_claims\tdef\n\
                   example.com\tFALSE\t/\tTRUE\t\tother\tnope\n";
        assert_eq!(
            Cookies::parse(jar).header(),
            "tr_session=abc; tr_claims=def"
        );
    }

    #[test]
    fn parses_portfolio_and_cash() {
        let positions = parse_portfolio(
            json!({"categories": [{"categoryType": "stocksAndETFs", "positions": [
                {"isin": "FR0000131104", "netSize": "10", "averageBuyIn": "55.5"},
                {"isin": "US0378331005", "netSize": 0.5, "averageBuyIn": null},
            ]}]}),
        )
        .unwrap();
        assert_eq!(
            positions,
            vec![
                SnapshotPosition {
                    isin: "FR0000131104".into(),
                    quantity: d("10"),
                    avg_cost: Some(d("55.5"))
                },
                SnapshotPosition {
                    isin: "US0378331005".into(),
                    quantity: d("0.5"),
                    avg_cost: None
                },
            ]
        );
        let cash = json!([{"accountNumber": "A", "currencyId": "EUR", "amount": 12.3400001}]);
        assert_eq!(parse_cash(cash.clone(), "A").unwrap(), Some(d("12.34")));
        assert_eq!(parse_cash(cash, "B").unwrap(), None);
    }

    /// Answers like Trade Republic's websocket: `connected`, then one `A` per subscription.
    #[expect(
        clippy::result_large_err,
        reason = "tungstenite's handshake callback signature"
    )]
    async fn fake_server(auth_error: bool) -> (String, tokio::task::JoinHandle<Option<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut cookie = None;
            let mut ws =
                tokio_tungstenite::accept_hdr_async(stream, |req: &Request, res: Response| {
                    cookie = req
                        .headers()
                        .get("Cookie")
                        .map(|v| v.to_str().unwrap().to_owned());
                    Ok(res)
                })
                .await
                .unwrap();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let text = text.to_string();
                if text.starts_with("connect 31 ") {
                    ws.send(Message::text("connected")).await.unwrap();
                    continue;
                }
                let Some(rest) = text.strip_prefix("sub ") else {
                    continue;
                };
                let (id, payload) = rest.split_once(' ').unwrap();
                let payload: Value = serde_json::from_str(payload).unwrap();
                let answer = match payload["type"].as_str().unwrap() {
                    _ if auth_error => {
                        ws.send(Message::text(format!(
                            "{id} E {{\"errors\":[{{\"errorCode\":\"AUTHENTICATION_ERROR\"}}]}}"
                        )))
                        .await
                        .unwrap();
                        continue;
                    }
                    "accountPairs" => json!({"accounts": [
                        {"securitiesAccountNumber": "S1", "cashAccountNumber": "C1", "productType": "DEFAULT"},
                        {"securitiesAccountNumber": "S2", "cashAccountNumber": "C2", "productType": "TAX_WRAPPER"},
                    ]}),
                    "compactPortfolioByType" if payload["secAccNo"] == "S2" => {
                        json!({"categories": [{"positions": [{"isin": "FR0000131104", "netSize": "3"}]}]})
                    }
                    "compactPortfolioByType" => json!({"categories": [{"positions": []}]}),
                    "cash" => {
                        json!([{"accountNumber": payload["accountNumber"], "currencyId": "EUR", "amount": 7.5}])
                    }
                    other => panic!("unexpected topic {other}"),
                };
                ws.send(Message::text("99 A {}")).await.unwrap();
                ws.send(Message::text(format!("{id} A {answer}")))
                    .await
                    .unwrap();
            }
            cookie
        });
        (url, handle)
    }

    /// Answers the session refresh with `status`, handing out `tr_session=fresh` on 200.
    async fn fake_refresh(status: u16) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.ends_with(b"\r\n\r\n") {
                let n = stream.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
            }
            let response = if status == 200 {
                "HTTP/1.1 200 OK\r\nSet-Cookie: tr_session=fresh; Path=/; Secure; HttpOnly\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
            } else {
                format!("HTTP/1.1 {status} Nope\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            };
            stream.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        (url, handle)
    }

    fn saved_cookies() -> Cookies {
        Cookies(vec![
            ("tr_refresh".into(), "r".into()),
            ("tr_session".into(), "old".into()),
        ])
    }

    #[tokio::test]
    async fn refreshes_the_session_then_fetches_both_accounts() {
        let (http, refresh) = fake_refresh(200).await;
        let (ws, server) = fake_server(false).await;
        let snapshots = fetch_snapshots(&Endpoints { http, ws }, &saved_cookies())
            .await
            .unwrap();
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[1].0.product_type, "TAX_WRAPPER");
        assert_eq!(snapshots[1].1.positions[0].quantity, d("3"));
        assert_eq!(snapshots[1].1.cash, Some(d("7.5")));
        let request = refresh.await.unwrap();
        assert!(request.starts_with("GET /api/v1/auth/web/session "));
        assert!(
            request
                .to_lowercase()
                .contains("cookie: tr_refresh=r; tr_session=old")
        );
        assert_eq!(
            server.await.unwrap().as_deref(),
            Some("tr_refresh=r; tr_session=fresh")
        );
    }

    #[tokio::test]
    async fn rejected_refresh_means_session_expired() {
        let (http, _refresh) = fake_refresh(401).await;
        let endpoints = Endpoints {
            http,
            ws: "ws://127.0.0.1:9".into(),
        };
        assert!(matches!(
            fetch_snapshots(&endpoints, &saved_cookies()).await,
            Err(Error::SessionExpired)
        ));
    }

    #[tokio::test]
    async fn maps_authentication_errors_to_session_expired() {
        let (http, _refresh) = fake_refresh(200).await;
        let (ws, _server) = fake_server(true).await;
        assert!(matches!(
            fetch_snapshots(&Endpoints { http, ws }, &saved_cookies()).await,
            Err(Error::SessionExpired)
        ));
    }
}
