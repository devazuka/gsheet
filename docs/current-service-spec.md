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
- successful sheet responses include:
  - `Cache-Control: public, max-age=60, s-maxage=60`
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

## Current Rust implementation

- lightweight HTTP server using `hyper`
- route surface: `/`, `/up`, `/:id/:sheet`, fallback `404`
- reject all query parameters
- numeric sheet resolution via spreadsheet metadata lookup
- sheet values lookup
- row-to-object JSON transformation
- CORS and fixed cache headers
- JSON error format
- `heed` cache for raw Google metadata payloads
- `heed` cache for raw Google values payloads
- in-process dedupe for in-flight upstream Google fetches

Notes:
- the Rust service caches raw Google responses and shapes them after cache retrieval
- expired `heed` entries are deleted on read instead of accumulating indefinitely
- analytics is still not implemented
