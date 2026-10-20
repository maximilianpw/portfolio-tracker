use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use axum::extract::{Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::corrections;
use crate::db::{self, ReconciliationIssue};
use crate::error::Error;
use crate::ledger::{Account, Kind};
use crate::sync::{load_ledger, today};
use crate::valuation::{Ledger, Mover, PeriodChange, PositionValue, Scope};

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub token: Arc<str>,
    pub corrections: Option<PathBuf>,
}

pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/positions", get(positions))
        .route("/summary", get(summary))
        .route("/movers", get(movers))
        .route("/dividends", get(dividends))
        .route("/transactions", get(transactions))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_token));
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .merge(protected)
        .with_state(state)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn require_token(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match presented {
        Some(token) if constant_time_eq(token.as_bytes(), state.token.as_bytes()) => {
            next.run(request).await
        }
        _ => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "missing or invalid bearer token"})),
        )
            .into_response(),
    }
}

pub enum ApiError {
    BadRequest(String),
    Internal(Error),
}

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        ApiError::Internal(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            ApiError::Internal(e) => {
                tracing::error!(error = %e, "request failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_owned(),
                )
            }
        };
        (status, Json(serde_json::json!({"error": message}))).into_response()
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct Params {
    account: Option<String>,
    period: Option<String>,
    since: Option<NaiveDate>,
    kind: Option<String>,
}

impl Params {
    fn scope(&self) -> Result<Scope, ApiError> {
        match self.account.as_deref() {
            None | Some("all") => Ok(Scope::All),
            Some(a) => Account::from_str(a)
                .map(Scope::Account)
                .map_err(ApiError::BadRequest),
        }
    }

    fn period_days(&self) -> Result<u64, ApiError> {
        match self.period.as_deref().unwrap_or("week") {
            "day" => Ok(1),
            "week" => Ok(7),
            "month" => Ok(30),
            "year" => Ok(365),
            other => Err(ApiError::BadRequest(format!(
                "unknown period {other:?}; use day, week, month or year"
            ))),
        }
    }

    fn includes(&self, account: Account) -> Result<bool, ApiError> {
        Ok(match self.scope()? {
            Scope::All => true,
            Scope::Account(a) => a == account,
        })
    }
}

#[derive(Serialize)]
pub struct Envelope<T> {
    as_of: NaiveDate,
    last_sync: Option<String>,
    complete: bool,
    reconciliation: Vec<ReconciliationIssue>,
    data: T,
}

struct Context {
    ledger: Ledger,
    as_of: NaiveDate,
    last_sync: Option<String>,
    sync_complete: bool,
    reconciliation: Vec<ReconciliationIssue>,
}

impl Context {
    async fn load(state: &AppState) -> Result<Context, Error> {
        let pool = &state.pool;
        let corrections = corrections::load_optional(state.corrections.as_deref())?;
        let ledger = load_ledger(pool, &corrections).await?;
        let run = db::last_successful_run(pool).await?;
        let reconciliation = match &run {
            Some(r) => db::load_issues(pool, r.id).await?,
            None => Vec::new(),
        };
        Ok(Context {
            as_of: ledger.latest_price_date().unwrap_or_else(today),
            last_sync: run.as_ref().and_then(|r| r.finished_at.clone()),
            sync_complete: run.as_ref().is_some_and(|r| r.complete),
            reconciliation,
            ledger,
        })
    }

    fn wrap<T>(self, complete: bool, data: T) -> Json<Envelope<T>> {
        Json(Envelope {
            as_of: self.as_of,
            last_sync: self.last_sync,
            complete: complete && self.sync_complete,
            reconciliation: self.reconciliation,
            data,
        })
    }
}

#[derive(Serialize)]
struct PositionsData {
    positions: Vec<PositionValue>,
    securities: Decimal,
    cash: Decimal,
    total: Decimal,
}

async fn positions(
    State(state): State<AppState>,
    Query(params): Query<Params>,
) -> Result<impl IntoResponse, ApiError> {
    let scope = params.scope()?;
    let ctx = Context::load(&state).await?;
    let v = ctx.ledger.value_at(scope, ctx.as_of);
    let complete = v.complete;
    Ok(ctx.wrap(
        complete,
        PositionsData {
            positions: v.positions,
            securities: v.securities,
            cash: v.cash,
            total: v.total,
        },
    ))
}

async fn summary(
    State(state): State<AppState>,
    Query(params): Query<Params>,
) -> Result<Json<Envelope<PeriodChange>>, ApiError> {
    let (scope, days) = (params.scope()?, params.period_days()?);
    let ctx = Context::load(&state).await?;
    let change = ctx
        .ledger
        .period_change(scope, Ledger::period_start(ctx.as_of, days), ctx.as_of);
    let complete = change.complete;
    Ok(ctx.wrap(complete, change))
}

async fn movers(
    State(state): State<AppState>,
    Query(params): Query<Params>,
) -> Result<Json<Envelope<Vec<Mover>>>, ApiError> {
    let (scope, days) = (params.scope()?, params.period_days()?);
    let ctx = Context::load(&state).await?;
    let movers = ctx
        .ledger
        .movers(scope, Ledger::period_start(ctx.as_of, days), ctx.as_of);
    let complete = movers.iter().all(|m| m.change.is_some());
    Ok(ctx.wrap(complete, movers))
}

#[derive(Serialize)]
struct TransactionView {
    event_id: String,
    leg: i64,
    account: Account,
    date: NaiveDate,
    ts: String,
    isin: Option<String>,
    name: Option<String>,
    kind: &'static str,
    quantity: Option<Decimal>,
    price: Option<Decimal>,
    amount: Decimal,
    fee: Decimal,
    tax: Decimal,
}

fn transaction_views(
    ctx: &Context,
    params: &Params,
    kind: Option<Kind>,
) -> Result<Vec<TransactionView>, ApiError> {
    let mut out = Vec::new();
    for p in ctx.ledger.postings() {
        if !params.includes(p.account)?
            || params.since.is_some_and(|s| p.date < s)
            || kind.is_some_and(|k| p.kind != k)
        {
            continue;
        }
        out.push(TransactionView {
            event_id: p.event_id.clone(),
            leg: p.leg,
            account: p.account,
            date: p.date,
            ts: p.ts.clone(),
            name: p.isin.as_deref().and_then(|i| ctx.ledger.name(i)),
            isin: p.isin.clone(),
            kind: p.kind.as_str(),
            quantity: p.quantity,
            price: p.price,
            amount: p.amount,
            fee: p.fee,
            tax: p.tax,
        });
    }
    Ok(out)
}

#[derive(Serialize)]
struct DividendsData {
    total: Decimal,
    dividends: Vec<TransactionView>,
}

async fn dividends(
    State(state): State<AppState>,
    Query(params): Query<Params>,
) -> Result<impl IntoResponse, ApiError> {
    let ctx = Context::load(&state).await?;
    let dividends = transaction_views(&ctx, &params, Some(Kind::Dividend))?;
    let total = dividends.iter().map(|d| d.amount).sum();
    Ok(ctx.wrap(true, DividendsData { total, dividends }))
}

async fn transactions(
    State(state): State<AppState>,
    Query(params): Query<Params>,
) -> Result<impl IntoResponse, ApiError> {
    let kind = params
        .kind
        .as_deref()
        .map(Kind::from_str)
        .transpose()
        .map_err(ApiError::BadRequest)?;
    let ctx = Context::load(&state).await?;
    let txs = transaction_views(&ctx, &params, kind)?;
    Ok(ctx.wrap(true, txs))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;
    use crate::db::connect;

    async fn app() -> (Router, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let pool = connect(&dir.path().join("db.sqlite")).await.unwrap();
        (
            router(AppState {
                pool,
                token: Arc::from("secret"),
                corrections: None,
            }),
            dir,
        )
    }

    async fn get(app: Router, uri: &str, token: Option<&str>) -> (StatusCode, serde_json::Value) {
        let mut req = Request::builder().uri(uri);
        if let Some(t) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let res = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let status = res.status();
        let body = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn rejects_missing_or_wrong_token() {
        let (app, _dir) = app().await;
        assert_eq!(
            get(app.clone(), "/summary", None).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            get(app.clone(), "/summary", Some("nope")).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(get(app, "/health", None).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn empty_database_is_reported_incomplete() {
        let (app, _dir) = app().await;
        let (status, body) = get(app, "/summary?period=week", Some("secret")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["complete"], false);
        assert_eq!(body["last_sync"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn rejects_bad_parameters() {
        let (app, _dir) = app().await;
        assert_eq!(
            get(app.clone(), "/summary?period=decade", Some("secret"))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            get(app, "/positions?account=cto", Some("secret")).await.0,
            StatusCode::BAD_REQUEST
        );
    }
}
