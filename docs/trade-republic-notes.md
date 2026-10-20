# Trade Republic data: what pytr and the API return

Notes from building the importer and client against a real account with pytr 0.4.10. Trade Republic has no public API; everything here is observed behaviour and can change without notice.

## Login and session

- `pytr login --store_credentials --v2` confirms the login in the Trade Republic app and needs no browser. Without `--v2`, pytr fetches an AWS WAF token with Playwright and Chromium.
- pytr stores the session as a Netscape cookie jar in `~/.pytr/cookies.<phone>.txt`, and the phone number and PIN in `~/.pytr/credentials` (mode `0600`).
- The saved `tr_session` cookie expires within minutes. pytr trades the long-lived `tr_refresh` cookie for a fresh one (`GET https://api.traderepublic.com/api/v1/auth/web/session`) on every run and keeps it in memory only, so anything reusing the jar must do the same.
- The refresh token itself lasts a long time but not forever. Expect to repeat `pytr login` occasionally.

## Accounts

One login can have two securities accounts, each with its own cash account:

| `productType` | What it is |
| --- | --- |
| `DEFAULT` | The regular brokerage account. |
| `TAX_WRAPPER` | A tax-advantaged account, e.g. a French PEA. |

Timeline events name their cash account in `cashAccountNumber`; older events and notices may have none. A purchase in the PEA is funded by a `PEA_DEPOSIT_DEBIT` from the regular account, which names the asset being bought and is often for the exact trade amount (sometimes only the shortfall).

## WebSocket API

`wss://api.traderepublic.com`, authenticated by the session cookies. After `connect 31 {…}` the server answers `connected`; requests are `sub <id> <json>`, answers `<id> A <json>` (full), `<id> D <delta>`, `<id> C` (closed) or `<id> E <json>` (error).

Read-only topics used here:

| Topic | Returns |
| --- | --- |
| `{"type": "accountPairs"}` | Both accounts: `securitiesAccountNumber`, `cashAccountNumber`, `productType`, `currency`. |
| `{"type": "compactPortfolioByType", "secAccNo": …}` | Holdings of one securities account: `isin`, `netSize`, `averageBuyIn`, … |
| `{"type": "cash", "accountNumber": …}` | Cash of one cash account. Without `accountNumber` it returns the regular account only; `availableCash` and `availableCashForPayout` do the same. |

`pytr portfolio` only asks for the default account. Filtering the timeline by account (`secAccNo`, `cashAccountNumber`, `productType`) has no effect; the timeline is one combined list.

## Timeline events

Use `export_transactions --store-event-database`, which writes `all_events.json` with every event and its details. The simplified CSV/JSON export drops event IDs and skips event types it doesn't know, including `PEA_DEPOSIT_DEBIT`, share gifts and securities transfers.

Event types that move cash or securities:

| `eventType` | Meaning |
| --- | --- |
| `TRADE_INVOICE` | Trade, older format (separate `Anteile` and `Aktienkurs` rows). |
| `TRADING_TRADE_EXECUTED` | Trade, current format (`Transaktion` row like `0,25 × 120,00 €`). |
| `PEA_DEPOSIT_DEBIT` | Cash moved from the regular account into the PEA. |
| `SSP_CORPORATE_ACTION_CASH` | Cash dividend: shares, dividend per share, tax, total. `status: CANCELED` and "korrigiert" subtitles occur. |
| `INTEREST_PAYOUT` | Interest, with tax withheld. |
| `BANK_TRANSACTION_INCOMING`, `PAYMENT_INBOUND`, `PAYMENT_INBOUND_APPLE_PAY` | Deposits. Card deposits show the gross amount and a separate fee. |
| `PAYMENT_OUTBOUND` | Withdrawal. |
| `SSP_SECURITIES_TRANSFER_INCOMING` | Shares transferred in (can be 0 shares). |
| `GIFTING_RECIPIENT_ACTIVITY`, `ACQUISITION_TRADE_PERK`, `STOCK_PERK_REFUNDED` | Gifted and perk shares. |
| `SSP_CORPORATE_ACTION_ACTIVITY`, `SSP_CORPORATE_ACTION_NO_CASH` | Spin-offs and bonus shares when they have an `Aktien hinzugefügt` row; otherwise meeting invitations and ISIN switches ("Wechsel") with no details. |

`SSP_CORPORATE_ACTION_UPCOMING` with status `Executed` duplicates the matching cash dividend event.

### Event shape

- Top level: `id`, `timestamp` (`2026-01-15T10:00:00.000+0000`), `title`, `subtitle`, `eventType`, `status`, `cashAccountNumber`, `icon`, and for money events `amount: {value, currency, fractionDigits}`. `amount` is the reliable signed cash figure, sent as a float.
- `details.sections[]` are built for display (`header`, `table`, `horizontalTable`, `documents`, `steps`). Table rows have a localized `title` and a `detail.text` such as `1.234,56 €`.
- Labels are mostly German with some English (`Übersicht`/`Overview`, `Gebühr`/`Fee`), and numbers use both `,` and `.` as the decimal separator.
- The trade side has to come from the order label (`Kauforder`, `Verkaufsorder`, `Stop-Sell-Order`…): a tiny sell can cost more in fees than it brings in, so its amount is negative.
- The ISIN: the header's `instrumentDetail` action, then a row's `…contextParams.instrumentId`, then the `icon` (`logos/<ISIN>/v2`). For derivatives the icon shows the underlying. Dividends only have it in the icon. A spin-off's new security appears only by name.

## Gaps the timeline doesn't show

- **Securities and cash transferred in from another bank** (e.g. a PEA transfer) have no events. Trade Republic may also import transferred positions without their buy price, so its average buy-in and gain/loss for them start from 0.
- **Fractional bonus shares** in a PEA, which can't hold fractions, disappear without an event.
- Small cash movements can be missing.

This is why the tracker reconciles against the account snapshot and lets you record [corrections](../README.md#corrections).

## Prices

`pytr rates <ISIN>…` returns `Name;ISIN;price;ask`: the latest quote from the first venue that answers, with no timestamp, venue or currency. It works for ISINs you don't hold, and an ISIN with no quote is left out.

## What pytr can do

The pytr library also contains order placement, savings plan changes, price alarms and cash payouts. Of its CLI commands, only `set_price_alarms` writes anything. This project runs only `export_transactions` and `rates`.
