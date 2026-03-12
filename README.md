# GSheet

A more convenient way to use Google Sheets as your backend data.

Heavily inspired by [opensheet](https://github.com/benborgers/opensheet#readme) from [@benborgers](https://github.com/benborgers).

I used his service a bunch, it's very handy, but I wanted a bit more control and the possibility for something faster and more reliable to run myself.

I also plan to add a way to pass in your own API key and bypass the shared limitations.

This code was mostly vibe-coded, then loosely reviewed and cleaned up.

The main idea is to keep it super lightweight and as close as possible to "just run the binary and put it behind your usual setup".

So the flow is basically:

- get your Google API key
- make the `.env`
- run the binary
- expose it how you want, with your trusted reverse proxy or whatever

## Run with Docker Compose

Create a `.env` file:

```env
GOOGLE_API_KEY=your_google_api_key
PORT=8080
```

Then run:

```sh
docker compose up --build -d
```

The service listens on `http://localhost:8080` by default and stores its LMDB cache in the `gsheet-data` Docker volume.

## Run without Docker

```sh
GOOGLE_API_KEY=your_google_api_key cargo run --release
```

Or build the binary first:

```sh
cargo build --release
GOOGLE_API_KEY=your_google_api_key ./target/release/gsheet
```

By default the service listens on port `8080`.

## Config

Only `GOOGLE_API_KEY` is required.

- `PORT` default `8080`
- `CACHE_DIR` default `./data/heed` when running directly, `/data` in Docker
- `SHEET_MAX_AGE_SECS` default `300`
- `DOCUMENT_MAX_AGE_SECS` default `3600`
- `ERROR_MAX_AGE_SECS` default `30`
- `GOOGLE_CACHE_TTL_SECS` default `300`
- `GOOGLE_CACHE_TTL_MAX_SECS` default `1200`
- `GOOGLE_RATE_LIMIT` default `300`
- `GOOGLE_RATE_WINDOW_SECS` default `60`
- `GOOGLE_MAX_QUEUED_REQUESTS` default `64`

## Deploy

Minimal VPS flow:

```sh
git clone https://github.com/devazuka/gsheet.git
cd gsheet
printf 'GOOGLE_API_KEY=your_google_api_key\nPORT=8080\n' > .env
docker compose up --build -d
```

Update in place:

```sh
git pull
docker compose up --build -d
```

## Routes

- `GET /up`
- `GET /:id`
- `GET /:id/:sheet`
- `GET /raw/:id`
- `GET /raw/:id/:sheet`
