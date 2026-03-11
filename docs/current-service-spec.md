# Current service spec

This document extracts the behavior implemented in [`base.ts`](/home/cdenis/Documents/gsheet/base.ts).

## Core Google Sheets API calls

### 1. Spreadsheet metadata lookup

Used only when the `:sheet` path segment is numeric and must be translated from a 1-based sheet number to a sheet title.

- Method: `GET`
- URL: `https://sheets.googleapis.com/v4/spreadsheets/{spreadsheet_id}?key={GOOGLE_API_KEY}`
- Purpose:
  - validate spreadsheet access
  - map sheet number `1..n` to `sheets[index].properties.title`
  - return a user-facing error when the sheet number is invalid

### 2. Sheet values lookup

Used for every successful data request after the sheet title is known.

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
- `GET /:id/:sheet`
  - returns sheet data as JSON array of objects
- all other paths
  - return a JSON error with status `404`

### Query parameters

- query parameters are not supported
- any query parameter returns `400`

### Sheet resolution behavior

- `:sheet` is URL-decoded
- `+` is treated as a space before decoding
- non-numeric values are used as sheet titles directly
- numeric values are treated as 1-based sheet indexes
- sheet `0` is rejected with `For this API, sheet numbers start at 1`
- missing numeric sheet indexes return `There is no sheet number {n}`

### Response shaping

- Google `values[0]` becomes the header row
- each subsequent row becomes an object keyed by the header names
- missing trailing cells are omitted
- response body is a JSON array

### Headers and caching

- all JSON responses include:
  - `Content-Type: application/json`
  - `Access-Control-Allow-Origin: *`
  - `Access-Control-Allow-Headers: Origin, X-Requested-With, Content-Type, Accept`
- successful sheet responses include randomized cache headers:
  - `Cache-Control: public, max-age={30-60}, s-maxage={30-60}`
- error responses include:
  - `Cache-Control: public, max-age=30, s-maxage=30`

### Error shape

All handled errors return:

```json
{
  "error": "message",
  "documentation": "https://github.com/benborgers/opensheet#readme"
}
```

## Non-functional behavior in the Bun version

- Redis cache keyed by full request URL
- separate Redis cache for spreadsheet metadata for 300 seconds
- in-memory pending-request dedupe keyed by full request URL
- 1% sampled analytics write to SQL by `(hour, sheet_id)`

## Rust migration plan

### Implemented in the first Rust slice

- lightweight HTTP server using `hyper`
- route surface: `/`, `/up`, `/:id/:sheet`, fallback `404`
- reject all query parameters
- numeric sheet resolution via spreadsheet metadata lookup
- sheet values lookup
- row-to-object JSON transformation
- CORS and cache headers
- JSON error format

### Selected Rust replacements

- metadata cache: `heed` instead of Redis
- values cache: `heed` instead of Redis
- request coalescing: keep an in-process pending-request map
- analytics: keep as a separate concern from caching

Reasoning:
- the Rust port caches Google-origin payloads as the source of truth
- final response shaping happens after cache retrieval
- `heed` gives a local embedded cache without introducing another service dependency
- LMDB-style storage does not replace the in-flight request dedupe logic, so that stays separate

### Deferred to follow-up slices

- `heed` spreadsheet metadata cache with 300 second TTL
- `heed` sheet values cache with 300 second TTL
- pending request coalescing
- sampled SQL analytics
- parity tests against the Bun implementation
