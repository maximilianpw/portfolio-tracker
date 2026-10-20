use std::path::PathBuf;
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
use utoipa::openapi::security::{Http, HttpAuthScheme, SecurityScheme};
use utoipa::{IntoParams, Modify, OpenApi, ToSchema};

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

#[derive(OpenApi)]
#[openapi(
    info(
        title = "portfolio-tracker",
        description = "Read-only view of a Trade Republic portfolio: holdings, value changes, dividends and transactions. Amounts are EUR, as decimal strings. When a response has complete=false, its figures are indicative only."
    ),
    paths(positions, summary, movers, dividends, transactions),
    components(schemas(AccountFilter, Period, Kind)),
    modifiers(&BearerAuth),
    security(("bearer" = []))
)]
pub struct ApiDoc;

struct BearerAuth;

impl Modify for BearerAuth {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        openapi
            .components
            .get_or_insert_with(Default::default)
            .add_security_scheme(
                "bearer",
                SecurityScheme::Http(Http::new(HttpAuthScheme::Bearer)),
            );
    }
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
        .route("/openapi.json", get(|| async { Json(ApiDoc::openapi()) }))
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
            Json(ErrorBody {
                error: "missing or invalid bearer token".into(),
            }),
        )
            .into_response(),
    }
}

#[derive(Serialize, ToSchema)]
pub struct ErrorBody {
    error: String,
}

pub struct ApiError(Error);

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        ApiError(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        tracing::error!(error = %self.0, "request failed");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorBody {
                error: "internal error".into(),
            }),
        )
            .into_response()
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum AccountFilter {
    #[default]
    All,
    Regular,
    Pea,
}

impl From<AccountFilter> for Scope {
    fn from(filter: AccountFilter) -> Scope {
        match filter {
            AccountFilter::All => Scope::All,
            AccountFilter::Regular => Scope::Account(Account::Regular),
            AccountFilter::Pea => Scope::Account(Account::Pea),
        }
    }
}

impl AccountFilter {
    fn includes(self, account: Account) -> bool {
        match Scope::from(self) {
            Scope::All => true,
            Scope::Account(a) => a == account,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Period {
    Day,
    #[default]
    Week,
    Month,
    Year,
}

impl Period {
    fn days(self) -> u64 {
        match self {
            Period::Day => 1,
            Period::Week => 7,
            Period::Month => 30,
            Period::Year => 365,
        }
    }
}

#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AccountParams {
    /// Which account to include: `regular`, `pea` (French tax wrapper), or `all` (default).
    #[serde(default)]
    account: AccountFilter,
}

#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PeriodParams {
    /// Which account to include: `regular`, `pea`, or `all` (default).
    #[serde(default)]
    account: AccountFilter,
    /// Length of the period ending at the latest price date. Default `week`.
    #[serde(default)]
    period: Period,
}

#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct HistoryParams {
    /// Which account to include: `regular`, `pea`, or `all` (default).
    #[serde(default)]
    account: AccountFilter,
    /// Only entries on or after this date (YYYY-MM-DD).
    since: Option<NaiveDate>,
}

#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TransactionParams {
    /// Which account to include: `regular`, `pea`, or `all` (default).
    #[serde(default)]
    account: AccountFilter,
    /// Only entries on or after this date (YYYY-MM-DD).
    since: Option<NaiveDate>,
    /// Only entries of this kind.
    kind: Option<Kind>,
}

#[derive(Serialize, ToSchema)]
pub struct Envelope<T> {
    /// Date the figures refer to: the latest day with prices.
    as_of: NaiveDate,
    /// When the last successful sync with Trade Republic finished.
    last_sync: Option<String>,
    /// False when data is missing or doesn't reconcile; treat figures as indicative then.
    complete: bool,
    /// Differences between the local history and Trade Republic found by the last sync.
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

#[derive(Serialize, ToSchema)]
pub struct PositionsData {
    positions: Vec<PositionValue>,
    securities: Decimal,
    cash: Decimal,
    total: Decimal,
}

/// Current holdings and value
///
/// Every position with quantity, latest price and value, plus cash and the total portfolio value.
#[utoipa::path(
    get,
    path = "/positions",
    operation_id = "get_positions",
    params(AccountParams),
    responses(
        (status = 200, description = "Figures with freshness and completeness", body = Envelope<PositionsData>),
        (status = 400, description = "Invalid parameter"),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody)
    )
)]
async fn positions(
    State(state): State<AppState>,
    Query(params): Query<AccountParams>,
) -> Result<Json<Envelope<PositionsData>>, ApiError> {
    let ctx = Context::load(&state).await?;
    let v = ctx.ledger.value_at(params.account.into(), ctx.as_of);
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

/// Portfolio performance over a period
///
/// Change in value over the period, net of deposits and withdrawals, with dividends, interest,
/// fees and taxes received or paid in it.
#[utoipa::path(
    get,
    path = "/summary",
    operation_id = "get_summary",
    params(PeriodParams),
    responses(
        (status = 200, description = "Figures with freshness and completeness", body = Envelope<PeriodChange>),
        (status = 400, description = "Invalid parameter"),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody)
    )
)]
async fn summary(
    State(state): State<AppState>,
    Query(params): Query<PeriodParams>,
) -> Result<Json<Envelope<PeriodChange>>, ApiError> {
    let ctx = Context::load(&state).await?;
    let change = ctx.ledger.period_change(
        params.account.into(),
        Ledger::period_start(ctx.as_of, params.period.days()),
        ctx.as_of,
    );
    let complete = change.complete;
    Ok(ctx.wrap(complete, change))
}

/// Biggest movers over a period
///
/// Change in value per holding over the period, largest gain first. Shares bought or sold in the
/// period count at their execution price, so buying more isn't a gain.
#[utoipa::path(
    get,
    path = "/movers",
    operation_id = "get_movers",
    params(PeriodParams),
    responses(
        (status = 200, description = "Figures with freshness and completeness", body = Envelope<Vec<Mover>>),
        (status = 400, description = "Invalid parameter"),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody)
    )
)]
async fn movers(
    State(state): State<AppState>,
    Query(params): Query<PeriodParams>,
) -> Result<Json<Envelope<Vec<Mover>>>, ApiError> {
    let ctx = Context::load(&state).await?;
    let movers = ctx.ledger.movers(
        params.account.into(),
        Ledger::period_start(ctx.as_of, params.period.days()),
        ctx.as_of,
    );
    let complete = movers.iter().all(|m| m.change.is_some());
    Ok(ctx.wrap(complete, movers))
}

#[derive(Serialize, ToSchema)]
pub struct TransactionView {
    event_id: String,
    leg: i64,
    account: Account,
    date: NaiveDate,
    ts: String,
    isin: Option<String>,
    name: Option<String>,
    kind: Kind,
    /// Signed change in shares held.
    quantity: Option<Decimal>,
    price: Option<Decimal>,
    /// Signed effect on cash, fees and taxes included.
    amount: Decimal,
    fee: Decimal,
    tax: Decimal,
}

fn transaction_views(
    ctx: &Context,
    account: AccountFilter,
    since: Option<NaiveDate>,
    kind: Option<Kind>,
) -> Vec<TransactionView> {
    ctx.ledger
        .postings()
        .iter()
        .filter(|p| {
            account.includes(p.account)
                && since.is_none_or(|s| p.date >= s)
                && kind.is_none_or(|k| p.kind == k)
        })
        .map(|p| TransactionView {
            event_id: p.event_id.clone(),
            leg: p.leg,
            account: p.account,
            date: p.date,
            ts: p.ts.clone(),
            name: p.isin.as_deref().and_then(|i| ctx.ledger.name(i)),
            isin: p.isin.clone(),
            kind: p.kind,
            quantity: p.quantity,
            price: p.price,
            amount: p.amount,
            fee: p.fee,
            tax: p.tax,
        })
        .collect()
}

#[derive(Serialize, ToSchema)]
pub struct DividendsData {
    total: Decimal,
    dividends: Vec<TransactionView>,
}

/// Dividends received
///
/// Every dividend payment, net of withholding tax, and their total.
#[utoipa::path(
    get,
    path = "/dividends",
    operation_id = "get_dividends",
    params(HistoryParams),
    responses(
        (status = 200, description = "Figures with freshness and completeness", body = Envelope<DividendsData>),
        (status = 400, description = "Invalid parameter"),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody)
    )
)]
async fn dividends(
    State(state): State<AppState>,
    Query(params): Query<HistoryParams>,
) -> Result<Json<Envelope<DividendsData>>, ApiError> {
    let ctx = Context::load(&state).await?;
    let dividends = transaction_views(&ctx, params.account, params.since, Some(Kind::Dividend));
    let total = dividends.iter().map(|d| d.amount).sum();
    Ok(ctx.wrap(true, DividendsData { total, dividends }))
}

/// Transaction history
///
/// Ledger entries: trades, dividends, interest, deposits, withdrawals, fees, taxes and transfers.
#[utoipa::path(
    get,
    path = "/transactions",
    operation_id = "get_transactions",
    params(TransactionParams),
    responses(
        (status = 200, description = "Figures with freshness and completeness", body = Envelope<Vec<TransactionView>>),
        (status = 400, description = "Invalid parameter"),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody)
    )
)]
async fn transactions(
    State(state): State<AppState>,
    Query(params): Query<TransactionParams>,
) -> Result<Json<Envelope<Vec<TransactionView>>>, ApiError> {
    let ctx = Context::load(&state).await?;
    let txs = transaction_views(&ctx, params.account, params.since, params.kind);
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
    async fn serves_the_openapi_spec_without_a_token() {
        let (app, _dir) = app().await;
        let (status, spec) = get(app, "/openapi.json", None).await;
        assert_eq!(status, StatusCode::OK);
        let operations: Vec<&str> = [
            "/positions",
            "/summary",
            "/movers",
            "/dividends",
            "/transactions",
        ]
        .iter()
        .map(|p| spec["paths"][p]["get"]["operationId"].as_str().unwrap())
        .collect();
        assert_eq!(
            operations,
            [
                "get_positions",
                "get_summary",
                "get_movers",
                "get_dividends",
                "get_transactions"
            ]
        );
        assert_eq!(
            spec["components"]["securitySchemes"]["bearer"]["scheme"],
            "bearer"
        );
        assert_eq!(spec["security"][0]["bearer"], serde_json::json!([]));
        assert_eq!(
            spec["components"]["schemas"]["Period"]["enum"],
            serde_json::json!(["day", "week", "month", "year"])
        );

        fn refs(value: &serde_json::Value, out: &mut Vec<String>) {
            match value {
                serde_json::Value::Object(map) => {
                    if let Some(serde_json::Value::String(r)) = map.get("$ref") {
                        out.push(r.clone());
                    }
                    map.values().for_each(|v| refs(v, out));
                }
                serde_json::Value::Array(items) => items.iter().for_each(|v| refs(v, out)),
                _ => {}
            }
        }
        for (path, item) in spec["paths"].as_object().unwrap() {
            for (status, response) in item["get"]["responses"].as_object().unwrap() {
                assert!(
                    response["description"].is_string(),
                    "{path} {status} has no description"
                );
            }
        }

        let mut all = Vec::new();
        refs(&spec, &mut all);
        let dangling: Vec<_> = all
            .iter()
            .filter(|r| {
                let name = r.trim_start_matches("#/components/schemas/");
                spec["components"]["schemas"][name].is_null()
            })
            .collect();
        assert!(dangling.is_empty(), "unresolved references: {dangling:?}");
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
