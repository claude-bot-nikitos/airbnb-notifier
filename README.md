# airbnb-notifier

A small Telegram bot that watches Airbnb searches and messages you when a **new**
listing shows up. It ships as a single static Rust binary, built to run on a
Raspberry Pi.

```
you ──(Airbnb search link)──> bot ──every 15 min──> Airbnb
you <──(🏠 new listing: photo, price, rating, link)── bot
```

## How it works

1. Open Airbnb (web or app), search, and set all your filters: dates, guests,
   price, room type, amenities, map area, and so on.
2. Copy or share the link to the results page and send it to the bot.
3. The bot saves every listing that matches right now **without** notifying you.
   After that, each check sends you only listings it hasn't seen before.
4. Optionally give the search a name. The bot asks right after you create it.
   Use `/list` to **pause / resume / rename / delete** it. You can have as many
   searches as you want.

The bot accepts full search URLs (`airbnb.*/s/.../homes?...`) and app share or
short links. It follows short links until it reaches the search URL.

### Bot commands

| Command | |
|---|---|
| *(send a link)* | create a search |
| `/list` | your searches, each with ⏸ Pause / ✏️ Rename / 🗑 Delete buttons |
| `/check` | check all your searches now |
| `/rename <id> <name>` | rename a search |
| `/pause <id>`, `/resume <id>`, `/delete <id>` | same actions as the buttons |

Only users in `allowed_users` can use the bot. If anyone else writes to it, the
bot replies with their Telegram ID so you can add them.

## Install on a Raspberry Pi

Works on Raspberry Pi OS 64-bit (Pi 3/4/5). All commands run on the Pi.

1. **Create the bot.** In Telegram, message [@BotFather](https://t.me/BotFather),
   send `/newbot`, and copy the token it gives you (`123456:ABC...`).

2. **Install the build tools** (one time, about 2 minutes):
   ```bash
   sudo apt update && sudo apt install -y git build-essential curl
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
   source "$HOME/.cargo/env"
   ```

3. **Get the code and build it** (about 3–5 minutes on a Pi 5). The repository
   is private, so clone it with your GitHub login: SSH key, `gh auth login`, or
   a personal access token as the password.
   ```bash
   git clone https://github.com/claude-bot-nikitos/airbnb-notifier.git
   cd airbnb-notifier
   cargo build --release
   ```

4. **Install it as a service.** This copies the binary to `/opt/airbnb-notifier`,
   adds the `airbnb-notifier` command, and sets up systemd:
   ```bash
   sudo BINARY=target/release/airbnb-notifier sh deploy/install.sh
   sudo airbnb-notifier init --token 123456:ABC...
   sudo systemctl enable --now airbnb-notifier
   ```

5. **Allow yourself.** Send `/start` to your bot. It replies with your Telegram
   ID. Then:
   ```bash
   sudo airbnb-notifier user add 123456789
   ```
   You don't need to restart; the bot re-reads the config on every message.

6. **Check that Airbnb is reachable from your network** (recommended):
   ```bash
   sudo airbnb-notifier test 'https://www.airbnb.com/s/Lisbon--Portugal/homes?adults=2' --pages 2
   ```
   It should print about 36 listings with names and prices.

7. **Use it.** On Airbnb, set up a search with your filters, copy the link of
   the results page, and send it to the bot.

Useful commands:

```bash
journalctl -u airbnb-notifier -f          # live logs
sudo systemctl restart airbnb-notifier    # restart
sudo airbnb-notifier user list            # who can use the bot
```

**Updating:**
```bash
cd ~/airbnb-notifier && git pull && cargo build --release
sudo BINARY=target/release/airbnb-notifier sh deploy/install.sh   # restarts the service
```

Everything the bot keeps lives in `/opt/airbnb-notifier`: the binary,
`config.toml` (mode `0600`, holds the token), and `searches.json`.

### Prebuilt binaries

`.github/workflows/release.yml` builds static binaries for `aarch64` (64-bit
Pi OS), `armv7` (32-bit Pi OS), and `x86_64` when you push a `v*` tag. If the
repository is public, `install.sh` without `BINARY` downloads the latest release:

```bash
curl -fsSL https://raw.githubusercontent.com/claude-bot-nikitos/airbnb-notifier/master/deploy/install.sh | sudo sh
```

## CLI

```
airbnb-notifier init [--token T]     create/update config.toml
airbnb-notifier run                  run the bot
airbnb-notifier user add <id>        allow a Telegram user
airbnb-notifier user remove <id>     revoke access (their searches pause)
airbnb-notifier user list
airbnb-notifier test <url> [--pages N]   fetch a search once and print the listings
```

The config path defaults to `./config.toml`. Change it with `--config` or
`AIRBNB_NOTIFIER_CONFIG`. The installed `/usr/local/bin/airbnb-notifier`
wrapper already points to `/opt/airbnb-notifier/config.toml`.

## Configuration (`config.toml`)

```toml
bot_token = "123456:ABC..."        # or env TELEGRAM_BOT_TOKEN
allowed_users = [123456789]
check_interval_minutes = 15         # per search
max_pages = 10                      # ~18 listings per page; Airbnb caps at 15 pages
data_file = "searches.json"         # relative to the config file
proxies = []                        # see below
```

If you break the file while the bot runs (a typo while editing), the bot logs
an error and keeps using the last version that loaded.

Advanced, rarely needed:

```toml
page_delay_ms = 1500                          # pause between Airbnb requests (+0–2s random)
telegram_api_url = "https://api.telegram.org" # e.g. a self-hosted Bot API server
airbnb_origin = "http://127.0.0.1:8080"       # testing only: fetch searches from a fake server
```

## Avoiding blocks

The bot reads the same search page your browser loads, at a low rate: one page
request every couple of seconds while checking, and each search only once per
interval. A Raspberry Pi on your home connection already has a **residential
IP**, which is what Airbnb trusts most. For a handful of searches you most
likely need nothing else.

If Airbnb starts blocking you, the bot says so after 3 failed checks in a row.
`/list` shows the last error. In that case:

- Increase `check_interval_minutes` and/or lower `max_pages`.
- Add proxies. Requests go through the first proxy, and the bot moves to the
  next one on any failure (HTTP error, captcha page, timeout):
  ```toml
  proxies = [
    "http://user:pass@gateway.example-provider.com:8000",
    "socks5://10.0.0.2:1080",
  ]
  ```
  Use a residential proxy provider you pay for, or a proxy/VPN on a machine you
  control. Free "public proxy" lists are not a good fit. They are mostly
  datacenter IPs that Airbnb already blocks, they die within hours, and the free
  "residential" ones often run on hijacked devices.

Test the setup with `airbnb-notifier test '<search url>'`. It uses the same
proxies and prints what it found.

## Development

```bash
cargo test                     # unit, integration and end-to-end tests
cargo llvm-cov --all-targets --ignore-filename-regex '/tests/'   # coverage (~99% of lines)
```

The tests don't need network access:

- **`tests/real_payload.rs`** parses a genuine Airbnb search payload captured in
  September 2026 (`tests/fixtures`). When Airbnb changes its format, replace the
  fixture with a fresh capture and these tests show what broke.
- **`tests/fetcher.rs`** runs the Airbnb client against a local fake Airbnb:
  pagination, blocks, captcha pages, short-link resolution, and proxy rotation
  and authentication.
- **`tests/e2e.rs`** starts the real binary against a fake Telegram Bot API and
  a fake Airbnb, then plays a whole chat session: unknown user, sharing a link,
  the silent baseline, naming, new-listing alerts, the buttons, blocking and
  recovery, revoking access, and restart without duplicate alerts.
- **`tests/cli.rs`** covers `init`, `user` and `test`.

CI also cross-compiles the Raspberry Pi binary (`aarch64-unknown-linux-musl`)
and runs the whole suite on it under qemu. To do that locally:

```bash
rustup target add aarch64-unknown-linux-musl   # plus: apt install clang qemu-user-static
export CC_aarch64_unknown_linux_musl=clang \
  CFLAGS_aarch64_unknown_linux_musl=--target=aarch64-unknown-linux-musl \
  CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
  CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUNNER=qemu-aarch64-static \
  TEST_BIN_RUNNER=qemu-aarch64-static
cargo test --target aarch64-unknown-linux-musl
```

## Limitations

- Airbnb has no public API. The bot reads the data embedded in the search results
  page, so it looks for listing objects anywhere in the embedded JSON rather than
  relying on exact paths. A big Airbnb redesign can still break it. If
  `airbnb-notifier test` finds 0 listings for a search that shows results in
  your browser, the page format changed.
- Only the first `max_pages` pages are scanned. For broad searches with hundreds
  of results, a listing that was always ranked beyond that limit and later moves
  up counts as "new". Narrow searches (map area, price, dates) avoid this.
- A listing that disappears and comes back later isn't reported again.
