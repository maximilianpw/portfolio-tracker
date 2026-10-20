# portfolio-tracker

A self-hosted, read-only tracker for a [Trade Republic](https://traderepublic.com) account. Every day it pulls your transaction history and holdings, keeps them in a local SQLite database, values the portfolio, writes CSVs you can import into [TradingView Portfolios](https://www.tradingview.com/support/solutions/43000760937-tradingview-portfolios-track-your-assets-know-your-trades/), and serves a small JSON API, for example for a bot that sends you a weekly summary.

> **Unofficial.** Not affiliated with or endorsed by Trade Republic. It talks to Trade Republic's undocumented API through [pytr](https://github.com/pytr-org/pytr) and a small client of its own, so it can break whenever Trade Republic changes something, and automated access may conflict with their terms. It only ever reads: there is no code path that places orders, changes savings plans or moves money. Use at your own risk.

## What it does

- **Full history:** every trade, dividend, interest payment, deposit, fee and tax, with exact decimal amounts.
- **Both accounts:** the regular account and a PEA (Trade Republic's French tax wrapper), kept apart.
- **Reconciliation:** checks the history against what Trade Republic reports each day, so gaps don't go unnoticed.
- **Corrections:** a reviewed file for what the history doesn't show, such as shares transferred in from another bank.
- **Valuation:** daily value, change over a period (net of deposits and withdrawals), and top movers.
- **TradingView:** import CSVs, one per account.
- **API:** read-only, token-protected; every response says how fresh and complete its numbers are.

## Quick start

With Nix:

```sh
nix develop
pytr login --store_credentials --v2     # once; confirm in the Trade Republic app
cargo run -- sync                       # fetch, import, reconcile, collect prices
cargo run -- summary --period week      # value, change and movers as JSON
```

Without Nix: install Rust 1.94+ and pytr (`pip install pytr==0.4.10`), then use the same commands.

If `sync` reports reconciliation issues, see [Corrections](#corrections).

## Hosting

It needs a machine that's always on: pytr is a Python program, the session lives on disk, and the data is a SQLite file. A home server, a NAS or a small VPS all work.

### NixOS

```nix
# flake.nix
inputs.portfolio-tracker.url = "github:maximilianpw/portfolio-tracker";

# configuration.nix
imports = [ inputs.portfolio-tracker.nixosModules.default ];
services.portfolio-tracker = {
  enable = true;
  tokenFile = "/run/secrets/portfolio-tracker-token";      # API token, 16+ characters (sops-nix, agenix, …)
  credentialsFile = "/run/secrets/pytr-credentials";       # optional: phone number and PIN on two lines
  correctionsFile = ./corrections.toml;                     # optional
  instrumentsFile = ./instruments.toml;                     # optional, enables the TradingView CSVs
  onFailure = [ "notify-failure@%n.service" ];             # optional alerting unit
};
```

This gives you an API service on `127.0.0.1:8787` and a daily sync at 23:15 Berlin time. Both run as a hardened `portfolio-tracker` user, with state in `/var/lib/portfolio-tracker`. After the first deploy:

```sh
sudo portfolio-tracker-login               # pytr login as the service user; confirm in the app
sudo systemctl start portfolio-tracker-sync
```

### Docker or Podman

```sh
docker build -t portfolio-tracker .
docker volume create pt-data
docker run --rm -it -v pt-data:/data portfolio-tracker pytr login --store_credentials --v2
docker run --rm -v pt-data:/data portfolio-tracker sh -c 'head -c 32 /dev/urandom | base64 > /data/token'
docker run -d --restart unless-stopped --name portfolio-tracker \
  -v pt-data:/data -p 127.0.0.1:8787:8787 portfolio-tracker
```

Then schedule the daily sync, for example with cron:

```cron
15 23 * * * docker run --rm -v pt-data:/data portfolio-tracker portfolio-tracker sync
```

For corrections or TradingView exports, copy the files into the volume and add `-e PT_CORRECTIONS=/data/corrections.toml -e PT_INSTRUMENTS=/data/instruments.toml` to both commands.

### Plain binary

```sh
cargo install --git https://github.com/maximilianpw/portfolio-tracker
pip install pytr==0.4.10
pytr login --store_credentials --v2
portfolio-tracker --data-dir ~/.local/share/portfolio-tracker sync   # schedule this daily
```

### Cloudflare

A Cloudflare Worker can't run this: it has no Python, no local files and no long-running process. What works well is running it on any of the machines above and exposing only the API through a [Cloudflare Tunnel](https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/), with [Cloudflare Access](https://developers.cloudflare.com/cloudflare-one/policies/access/) in front:

```sh
cloudflared tunnel --url http://localhost:8787    # quick test; use a named tunnel for real
```

For a bot, create an Access service token and send its `CF-Access-Client-Id` and `CF-Access-Client-Secret` headers along with the API's own bearer token. Tailscale or a reverse proxy on your LAN work just as well.

## Configuration

| Variable | Flag | Meaning |
| --- | --- | --- |
| `PT_DATA_DIR` | `--data-dir` | Database, pytr exports, snapshots and CSVs. Default `./data`. |
| `PT_CORRECTIONS` | `--corrections` | Corrections file. |
| `PT_INSTRUMENTS` | `--instruments` | ISIN → TradingView symbol mapping. |
| `PT_PYTR` | `--pytr` | pytr executable. Default `pytr`. |
| `PT_PYTR_COOKIES` | `--cookies` | pytr cookie jar. Default: the single `~/.pytr/cookies.*.txt`. |
| `PT_LISTEN` | `--listen` | API address. Default `127.0.0.1:8787`. |
| `PT_TOKEN_FILE` | `--token-file` | File containing the API bearer token. |

### Corrections

Trade Republic's history leaves some things out, notably securities and cash transferred in from another bank. When that happens, `sync` reports reconciliation issues and figures are marked incomplete.

`portfolio-tracker corrections-template > corrections.toml` proposes entries that close the gaps:
- holdings that existed before the history starts become opening balances;
- anything else becomes an adjustment dated today.

Review the file, set real dates where you know them, and point `PT_CORRECTIONS` at it.

```toml
[[correction]]
account = "pea"
date = 2025-03-03
kind = "opening_balance"      # or "adjustment"
isin = "DE0005557508"         # securities: isin + quantity
quantity = "6"
price = "21.50"               # optional cost per share, only used for TradingView
note = "Transferred from my old bank"

[[correction]]
account = "pea"
date = 2025-03-03
kind = "opening_balance"
amount = "1000.00"            # cash
note = "Cash that came with the transfer"
```

Trade Republic often imports transferred positions without a buy price, so the template never fills in `price`. Values and period changes don't need it. The TradingView export does, and refuses to run until it's set; `price = "0"` reproduces what the Trade Republic app shows.

### TradingView

1. Run `portfolio-tracker instruments-template > instruments.toml`, then fill in each `tradingview` symbol (`EXCHANGE:TICKER`, e.g. `XETR:SAP`). See [`instruments.example.toml`](instruments.example.toml).
2. Set `PT_INSTRUMENTS`. Every sync then writes `tradingview/regular.csv` and `tradingview/pea.csv` in the data directory.
3. Import them in TradingView under Portfolios → Upload CSV. Re-uploading is safe: the files are byte-for-byte stable, and TradingView skips rows it already has.

## API

All endpoints except `/health` and `/openapi.json` need `Authorization: Bearer <token>`.

| Endpoint | Returns |
| --- | --- |
| `GET /positions?account=all\|regular\|pea` | Holdings with price, value and staleness, plus cash and total. |
| `GET /summary?period=day\|week\|month\|year&account=…` | Change over the period, net flows, dividends, interest, fees, taxes. |
| `GET /movers?period=…&account=…` | Per-holding change over the period, largest first. |
| `GET /dividends?since=YYYY-MM-DD&account=…` | Dividends and their total. |
| `GET /transactions?since=…&kind=…&account=…` | Ledger entries. |

Every response is `{as_of, last_sync, complete, reconciliation, data}`. When `complete` is false (a reconciliation issue, a missing or stale price, a negative balance), treat the numbers as indicative only.

### For AI agents

`GET /openapi.json` describes the API: an OpenAPI 3.1 spec with operation IDs (`get_positions`, `get_summary`, `get_movers`, `get_dividends`, `get_transactions`), parameter choices and a bearer security scheme. Tools that accept OpenAPI can turn it into callable tools directly. For example, in [Executor](https://github.com/UsefulSoftwareCo/executor), add an OpenAPI integration with the spec URL and bearer auth with your token; any MCP client connected to Executor can then use it, and the tracker itself doesn't need to be reachable from the internet.

## Commands

| Command | What it does |
| --- | --- |
| `sync` | The daily job: timeline, import, reconciliation, prices, TradingView CSVs. |
| `snapshot` | Holdings and cash per account, as Trade Republic reports them. |
| `corrections-template` | Proposed corrections from the last sync, keeping existing ones. |
| `import <all_events.json>` | Import a pytr event file without contacting Trade Republic. |
| `prices` | Record today's price for every holding. |
| `export --instruments <file>` | Write the TradingView CSVs. |
| `instruments-template` | An `instruments.toml` skeleton for every ISIN seen. |
| `summary --period week` | Value, change and movers as JSON. |
| `serve` | The API. |

## Limitations

- **Logins expire.** Every few weeks or months, `sync` fails with "session expired". Run `pytr login --store_credentials --v2` again as the same user.
- **Price history starts with your first sync.** Weekly figures become complete after a week.
- **EUR only.** Positions priced in other currencies are rejected.
- **No time-weighted returns.** Period change is value change net of deposits and withdrawals.
- **Untested:** the TradingView `reward` rows (gifted and perk shares).

## Development

```sh
nix develop                           # Rust toolchain, pytr, sqlx-cli, sqlite
cargo test
nix flake check                       # build, tests, clippy, rustfmt
cargo sqlx prepare -- --all-targets   # after changing a SQL query
```

[`docs/design.md`](docs/design.md) covers the data flow, ledger, valuation and export. [`docs/trade-republic-notes.md`](docs/trade-republic-notes.md) describes what Trade Republic's data and API look like.

## License

[MIT](LICENSE)
