# floki-server

Accounts, teams and shared workspaces for the Floki desktop client. PostgreSQL, single binary.

## PostgreSQL setup

    createuser --pwprompt floki
    createdb --owner floki floki

`DATABASE_URL` format: `postgres://floki:PASSWORD@localhost:5432/floki`. Migrations (`migrations/`)
run automatically on every start. For managed Postgres append `?sslmode=require` (TLS is built in via rustls).

## Run

    DATABASE_URL=postgres://floki:PASSWORD@localhost/floki cargo run --release

| Env var | Default | |
|---|---|---|
| `DATABASE_URL` | required | Postgres connection URL |
| `FLOKI_DB_MAX_CONNECTIONS` | `10` | pool size |
| `FLOKI_BIND` | `127.0.0.1:8787` | listen address |
| `GOOGLE_CLIENT_ID` / `GOOGLE_CLIENT_SECRET` | unset | Google sign-in is disabled unless both are set |

## Deploying

- `cargo build --release`, then run `target/release/floki-server` under systemd or similar.
- The server speaks plain HTTP: put it behind a TLS reverse proxy (Caddy, nginx) and set
  `FLOKI_BIND=0.0.0.0:8787` only when the proxy is the sole way in.
- Back up with `pg_dump "$DATABASE_URL" > backup.sql`.

## Google sign-in

1. Google Cloud Console -> APIs & Services -> Credentials -> Create credentials -> OAuth client ID.
2. Application type: **Desktop app**. (Configure the OAuth consent screen first if prompted.)
3. Export the generated ID and secret as `GOOGLE_CLIENT_ID` and `GOOGLE_CLIENT_SECRET`.

The desktop client uses a loopback redirect (`http://127.0.0.1:<port>`) with PKCE, then posts the
`code`, `code_verifier` and `redirect_uri` to `POST /api/auth/google`.

## Test

    TEST_DATABASE_URL=postgres://user:pass@127.0.0.1:5432/scratch cargo test

Each test builds and drops its own schema. Without `TEST_DATABASE_URL` the tests are skipped.

## Email verification

Signup sends a 6-digit code (valid 30 minutes, 5 attempts). Unverified accounts work for personal
workspaces but don't receive team/workspace invites and can't be added directly to teams.

| Env var | |
|---|---|
| `FLOKI_SMTP_URL` | e.g. `smtps://user:pass@smtp.example.com:465`; unset = dev fallback |
| `FLOKI_MAIL_FROM` | required with the SMTP URL, e.g. `Floki <noreply@example.com>` |

Without `FLOKI_SMTP_URL` the code is printed to the server's stdout as
`Verification code for <email>: 123456`.
