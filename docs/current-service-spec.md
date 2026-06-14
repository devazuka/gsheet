# Current service spec

## Core Google Sheets API calls

### 1. Spreadsheet metadata lookup

Used when the request targets the whole document and the service must enumerate sheet titles.

- Method: `GET`
- URL: `https://sheets.googleapis.com/v4/spreadsheets/{spreadsheet_id}?key={GOOGLE_API_KEY}`
- Purpose:
  - validate spreadsheet access
  - list sheet titles
  - drive whole-document responses

### 2. Sheet values lookup

Used for every successful sheet data request once the sheet title is known.

- Method: `GET`
- URL: `https://sheets.googleapis.com/v4/spreadsheets/{spreadsheet_id}/values/{sheet_title_encoded}?key={GOOGLE_API_KEY}`
- Purpose:
  - read the tab contents
  - treat the first row as column headers
  - convert subsequent rows into JSON objects

## Observable HTTP features

### Routes

- `GET /`
  - redirects to the GitHub README
- `GET /up`
  - returns plain text `ok`
- `GET /:id`
  - returns all sheets keyed by sheet title
- `GET /:id/:sheet`
  - returns sheet data as JSON array of objects
- `GET /raw/:id`
  - returns all raw Google values payloads keyed by sheet title
- `GET /raw/:id/:sheet`
  - returns one raw Google values payload
- `GET /refresh/:id/:sheet`
  - deletes the cached upstream Google values payload for one sheet page so the next read fetches fresh data
  - returns `{"refreshed":true}` when an entry was removed, or `{"refreshed":false}` when no entry existed
- trailing `/` characters are ignored on all routes
- all other paths
  - return a JSON error with status `404`

### Query parameters

- query parameters are not supported
- any query parameter returns `400`

### Sheet resolution behavior

- `:sheet` is URL-decoded
- `+` is treated as a space before decoding
- the decoded value is used directly as the sheet title
- numeric sheet indexes are not supported

### Response shaping

- Google `values[0]` becomes the header row
- each subsequent row becomes an object keyed by the header names
- missing trailing cells are omitted
- `GET /:id/:sheet` returns a JSON array
- `GET /:id` returns a JSON object keyed by sheet title

### Headers and caching

- all JSON responses include:
  - `Content-Type: application/json`
  - `Access-Control-Allow-Origin: *`
  - `Access-Control-Allow-Headers: Origin, X-Requested-With, Content-Type, Accept`
- successful single-sheet responses include:
  - `Cache-Control: public, max-age=300, s-maxage=300` by default
- successful whole-document responses include:
  - `Cache-Control: public, max-age=3600, s-maxage=3600` by default
- error responses include:
  - `Cache-Control: public, max-age=30, s-maxage=30` by default
- upstream Google cache entries start at `300` seconds by default
- upstream Google cache entries can extend up to `1200` seconds by default when the Google request queue is under pressure

### Upstream throttling

- Google API calls are limited to `300` requests per `60` seconds by default
- the limiter is applied only to cache misses that actually call Google
- at most `64` Google requests may wait in the local throttle queue by default
- if that queue is full, the request fails with `429`

### Runtime configuration

All of these are optional and keep the current defaults when unset:

- `SHEET_MAX_AGE_SECS`
- `DOCUMENT_MAX_AGE_SECS`
- `ERROR_MAX_AGE_SECS`
- `GOOGLE_CACHE_TTL_SECS`
- `GOOGLE_CACHE_TTL_MAX_SECS`
- `GOOGLE_RATE_LIMIT`
- `GOOGLE_RATE_WINDOW_SECS`
- `GOOGLE_MAX_QUEUED_REQUESTS`

### Error shape

All handled errors return:

```json
{
  "error": "message",
  "documentation": "https://github.com/devazuka/gsheet#readme"
}
```

## Non-functional behavior in the Bun version

- Redis cache keyed by full request URL
- separate Redis cache for spreadsheet metadata for 300 seconds
- in-memory pending-request dedupe keyed by full request URL
- 1% sampled analytics write to SQL by `(hour, sheet_id)`

## Current Rust implementation

- lightweight HTTP server using `hyper`
- route surface: `/`, `/up`, `/:id`, `/:id/:sheet`, `/raw/:id`, `/raw/:id/:sheet`, fallback `404`
- reject all query parameters
- whole-document metadata lookup
- sheet values lookup
- row-to-object JSON transformation
- CORS and fixed cache headers
- JSON error format
- `lmdb` cache for raw Google metadata payloads
- `lmdb` cache for raw Google values payloads
- in-process dedupe for in-flight upstream Google fetches
- upstream Google throttle with bounded queue and `429` overflow

Notes:
- the Rust service caches raw Google responses and shapes them after cache retrieval
- expired `lmdb` entries are deleted on read instead of accumulating indefinitely
- trailing `/` characters are normalized away during routing
- analytics is still not implemented
