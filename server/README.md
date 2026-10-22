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
| `PORT` | unset | set by hosting platforms; when `FLOKI_BIND` is unset the server listens on `0.0.0.0:$PORT` |
| `GOOGLE_CLIENT_ID` / `GOOGLE_CLIENT_SECRET` | unset | Google sign-in is disabled unless both are set |

## Deploying

- `cargo build --release`, then run `target/release/floki-server` under systemd or similar.
- The server speaks plain HTTP: put it behind a TLS reverse proxy (Caddy, nginx) and set
  `FLOKI_BIND=0.0.0.0:8787` only when the proxy is the sole way in.
- Back up with `pg_dump "$DATABASE_URL" > backup.sql`.

### Docker

```sh
docker build -t floki-server .
docker run -e DATABASE_URL='postgres://…?sslmode=require' -p 8787:8787 floki-server
```

### Your own VPS (Hostinger, Hetzner, DigitalOcean…)

`deploy/` runs PostgreSQL, the server and Caddy (automatic HTTPS) with Docker Compose.

1. Point a domain at the VPS with an A record, e.g. `api.example.com`.
2. On the VPS (Ubuntu): install Docker (`curl -fsSL https://get.docker.com | sh`), open only SSH, 80 and 443
   (`ufw allow OpenSSH && ufw allow 80,443/tcp && ufw enable`).
3. `git clone https://github.com/Adityakumar-blip/floki && cd floki/server/deploy`
4. `cp .env.example .env`, set `FLOKI_DOMAIN` and a strong `POSTGRES_PASSWORD` (`openssl rand -base64 32`).
5. `docker compose up -d --build`. Check `https://<domain>/api/auth/google/config` answers (404 JSON until Google is set up).
6. Updates: `git pull && docker compose up -d --build`. Logs: `docker compose logs -f server`.
7. Backups (daily cron): `docker compose exec -T db pg_dump -U floki floki | gzip > ~/floki-$(date +%F).sql.gz`

### Without Docker (systemd + nginx)

Release builds attach `floki-server-linux-x64.tar.gz` (server binary, `floki-server.service`, `nginx-floki.conf`,
`floki.env.example`, `floki-update`). Install once:

```sh
install -m 755 floki-server /usr/local/bin/ && install -m 755 floki-update /usr/local/bin/
useradd --system --no-create-home --shell /usr/sbin/nologin floki
mkdir -p /etc/floki && cp floki.env.example /etc/floki/floki.env && chmod 600 /etc/floki/floki.env   # then edit it
cp floki-server.service /etc/systemd/system/ && systemctl daemon-reload && systemctl enable --now floki-server
```

**Updating:** publish a release (push a `v*` tag), then on the server run `floki-update`. It downloads the latest
server build, restarts the service, and rolls back to the previous binary if the new one doesn't answer. Database
migrations run automatically on start.

### Free hosting: Google Cloud Run + Neon

Cloud Run gives HTTPS, scales to zero and has an always-free monthly allowance; Neon gives a free serverless
Postgres (0.5 GB). Both need an account; Cloud Run also needs a billing account on file.

1. Neon: create a project and copy its connection string (it already includes `sslmode=require`).
2. Install the `gcloud` CLI, then from this folder:
   ```sh
   gcloud run deploy floki-server --source . --region us-central1 --allow-unauthenticated \
     --set-env-vars "DATABASE_URL=<neon url>"
   ```
   Add `GOOGLE_CLIENT_ID`, `GOOGLE_CLIENT_SECRET`, `RESEND_API_KEY` and `FLOKI_MAIL_FROM` the same way
   (better: store secrets in Secret Manager and pass `--set-secrets`).
3. Use the printed `https://…run.app` URL as the app's default server: set the repository variable
   `FLOKI_SERVER` (Settings → Secrets and variables → Actions → Variables) before tagging a release.

Free tiers have limits: each signed-in app checks for changes every 15 s and keeps the database awake while open,
so a small team fits, but watch Neon's monthly compute hours and Cloud Run's request count as usage grows.

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
| `RESEND_API_KEY` | API key from [Resend](https://resend.com) (`re_…`); unset = dev fallback |
| `FLOKI_MAIL_FROM` | required with the key; an address on a domain verified in Resend, e.g. `Floki <no-reply@example.com>` |

Emails go through Resend's HTTP API over HTTPS, so outbound SMTP ports don't need to be open. To set up:
add your domain in Resend → Domains, add the DNS records it shows (SPF, DKIM) at your DNS provider, wait until
it says Verified, then create an API key with "Sending access".

Without `RESEND_API_KEY` the code is printed to the server's stdout as
`Verification code for <email>: 123456`. The startup line says which mode is active.
