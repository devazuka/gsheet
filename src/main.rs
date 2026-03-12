use std::{
    collections::{HashMap, VecDeque},
    convert::Infallible,
    env, fs,
    future::Future,
    net::SocketAddr,
    path::Path,
    sync::{Arc, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use heed::{
    Database, Env, EnvOpenOptions,
    types::{Bytes as HeedBytes, SerdeBincode, Str, Unit},
};
use http_body_util::{BodyExt, Empty, Full};
use hyper::http::HeaderValue;
use hyper::{
    Method, Request, Response, StatusCode,
    body::Incoming,
    header::{
        ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_ORIGIN, CACHE_CONTROL, CONTENT_TYPE,
        LOCATION,
    },
    server::conn::http1,
    service::service_fn,
};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::{
    sync::{Mutex, Notify, Semaphore},
    time::{Instant, sleep},
};

const README_URL: &str = "https://github.com/kigiri/gsheet#readme";
const BASE_ALLOWED_HEADERS: &str = "Origin, X-Requested-With, Content-Type, Accept";
const SUCCESS_CACHE_CONTROL: &str = "public, max-age=60, s-maxage=60";
const ERROR_CACHE_CONTROL: &str = "public, max-age=30, s-maxage=30";
const ROUTE_FORMAT: &str =
    "URL format is /spreadsheet_id[/sheet_name] or /raw/spreadsheet_id[/sheet_name]";
const GOOGLE_CACHE_TTL_SECS: u64 = 300;
const GOOGLE_CACHE_TTL_MAX_SECS: u64 = 1200;
const GOOGLE_RATE_LIMIT: usize = 300;
const GOOGLE_RATE_WINDOW: Duration = Duration::from_secs(60);
const GOOGLE_MAX_QUEUED_REQUESTS: usize = 64;
const CACHE_DB: &str = "cache";
const EXPIRY_DB: &str = "expiry";

type ResBody = Full<Bytes>;
type CacheDb = Database<Str, SerdeBincode<CacheEntry>>;
type ExpiryDb = Database<HeedBytes, Unit>;
type CacheResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
type HttpClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Empty<Bytes>>;

static STATE: OnceLock<State> = OnceLock::new();

struct State {
    http: HttpClient,
    api_key: String,
    cache: HeedCache,
    inflight: InflightRequests,
    throttle: GoogleThrottle,
}

#[tokio::main]
async fn main() {
    let port = env::var("PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(3000);

    let http = Client::builder(TokioExecutor::new()).build(
        HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_only()
            .enable_http1()
            .build(),
    );
    let state = State {
        http,
        api_key: env::var("GOOGLE_API_KEY").expect("GOOGLE_API_KEY is required"),
        cache: HeedCache::open(env::var("CACHE_DIR").unwrap_or_else(|_| "./data/heed".to_string()))
            .expect("failed to initialize cache"),
        inflight: InflightRequests::new(),
        throttle: GoogleThrottle::new(
            GOOGLE_RATE_LIMIT,
            GOOGLE_RATE_WINDOW,
            GOOGLE_MAX_QUEUED_REQUESTS,
        ),
    };
    let _ = STATE.set(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind TCP listener");

    println!("Server running on http://localhost:{port}");

    loop {
        let (stream, _) = listener.accept().await.expect("failed to accept socket");
        let io = TokioIo::new(stream);

        tokio::spawn(async move {
            let service = service_fn(|request: Request<Incoming>| async move {
                Ok::<_, Infallible>(route_request(request.method(), request.uri()).await)
            });

            if let Err(error) = http1::Builder::new().serve_connection(io, service).await {
                eprintln!("server connection error: {error}");
            }
        });
    }
}

fn state() -> &'static State {
    STATE.get().expect("state is not initialized")
}

async fn route_request(method: &Method, uri: &hyper::Uri) -> Response<ResBody> {
    if method != Method::GET {
        return error_response(ROUTE_FORMAT.to_owned(), StatusCode::NOT_FOUND);
    }

    let path = uri.path().trim_matches('/');

    if path.is_empty() {
        let mut response = Response::new(Full::new(Bytes::new()));
        *response.status_mut() = StatusCode::FOUND;
        response
            .headers_mut()
            .insert(LOCATION, HeaderValue::from_static(README_URL));
        return response;
    }

    if path == "up" {
        let mut response = Response::new(Full::new(Bytes::from_static(b"ok")));
        *response.status_mut() = StatusCode::OK;
        return response;
    }

    if uri.query().is_some() {
        return error_response(
            format!("Query parameters are not supported. Your request was: {uri}"),
            StatusCode::BAD_REQUEST,
        );
    }

    let mut path = path;
    let raw = if let Some(raw_path) = path.strip_prefix("raw/") {
        path = raw_path;
        true
    } else {
        false
    };
    let mut parts = path.split('/');
    let Some(id) = parts.next() else {
        return error_response(ROUTE_FORMAT.to_owned(), StatusCode::NOT_FOUND);
    };
    let sheet = parts.next();

    if id.is_empty() || sheet.is_some_and(str::is_empty) || parts.next().is_some() {
        return error_response(ROUTE_FORMAT.to_owned(), StatusCode::NOT_FOUND);
    }

    let result = if let Some(sheet) = sheet {
        let sheet_name = match decode_sheet_name(sheet) {
            Ok(sheet_name) => sheet_name,
            Err(error) => return error_response(error.message, error.status),
        };

        if raw {
            fetch_values_bytes(id, &sheet_name).await
        } else {
            fetch_sheet_json(id, &sheet_name).await.map(Bytes::from)
        }
    } else if raw {
        fetch_document_raw_json(id).await
    } else {
        fetch_document_json(id).await.map(Bytes::from)
    };

    match result {
        Ok(body) => json_response(StatusCode::OK, SUCCESS_CACHE_CONTROL, body),
        Err(error) => error_response(error.message, error.status),
    }
}

async fn fetch_sheet_json(spreadsheet_id: &str, sheet_name: &str) -> Result<Vec<u8>, GoogleError> {
    let body = fetch_values_bytes(spreadsheet_id, sheet_name).await?;
    let payload: SheetValuesResponse =
        serde_json::from_slice(&body).map_err(GoogleError::json_decode)?;

    if let Some(error) = payload.error {
        return Err(GoogleError::bad_request(error.message));
    }

    let rows = values_to_rows(payload.values.unwrap_or_default());
    serde_json::to_vec(&rows).map_err(GoogleError::json_encode)
}

async fn fetch_document_json(spreadsheet_id: &str) -> Result<Vec<u8>, GoogleError> {
    let metadata = fetch_spreadsheet_metadata(spreadsheet_id).await?;
    let mut document = Map::with_capacity(metadata.sheets.len());

    for sheet in metadata.sheets {
        let body = fetch_values_bytes(spreadsheet_id, &sheet.properties.title).await?;
        let payload: SheetValuesResponse =
            serde_json::from_slice(&body).map_err(GoogleError::json_decode)?;

        if let Some(error) = payload.error {
            return Err(GoogleError::bad_request(error.message));
        }

        document.insert(
            sheet.properties.title,
            Value::Array(values_to_rows(payload.values.unwrap_or_default())),
        );
    }

    serde_json::to_vec(&document).map_err(GoogleError::json_encode)
}

async fn fetch_document_raw_json(spreadsheet_id: &str) -> Result<Bytes, GoogleError> {
    let metadata = fetch_spreadsheet_metadata(spreadsheet_id).await?;
    let mut document = Map::with_capacity(metadata.sheets.len());

    for sheet in metadata.sheets {
        let body = fetch_values_bytes(spreadsheet_id, &sheet.properties.title).await?;
        let payload: Value = serde_json::from_slice(&body).map_err(GoogleError::json_decode)?;
        document.insert(sheet.properties.title, payload);
    }

    serde_json::to_vec(&document)
        .map(Bytes::from)
        .map_err(GoogleError::json_encode)
}

fn decode_sheet_name(sheet_param: &str) -> Result<String, GoogleError> {
    let normalized = if sheet_param.contains('+') {
        sheet_param.replace('+', " ")
    } else {
        sheet_param.to_owned()
    };
    let decoded = urlencoding::decode(&normalized)
        .map(|decoded| decoded.into_owned())
        .map_err(|_| GoogleError::bad_request("Invalid sheet path encoding"))?;

    Ok(decoded)
}

async fn fetch_spreadsheet_metadata(
    spreadsheet_id: &str,
) -> Result<SpreadsheetMetadataResponse, GoogleError> {
    let cache_key = format!("metadata:{spreadsheet_id}");

    if let Some(body) = state()
        .cache
        .get_metadata(&cache_key)
        .map_err(GoogleError::cache)?
    {
        return parse_metadata(&body);
    }

    let url = format!(
        "https://sheets.googleapis.com/v4/spreadsheets/{spreadsheet_id}?key={}",
        state().api_key
    );

    let body = state()
        .inflight
        .run(cache_key.clone(), || async {
            if let Some(body) = state()
                .cache
                .get_metadata(&cache_key)
                .map_err(GoogleError::cache)?
            {
                return Ok(body);
            }

            let (status, body) = fetch_json(&url).await?;
            let payload = parse_metadata(&body)?;
            if status != StatusCode::OK {
                drop(payload);
                return Err(GoogleError::bad_request(
                    "Google Sheets metadata request failed",
                ));
            }
            let ttl_seconds = state().throttle.cache_ttl_secs();
            state()
                .cache
                .put_metadata(&cache_key, &body, ttl_seconds)
                .map_err(GoogleError::cache)?;
            Ok(body)
        })
        .await?;

    parse_metadata(&body)
}

async fn fetch_values_bytes(spreadsheet_id: &str, sheet_name: &str) -> Result<Bytes, GoogleError> {
    let cache_key = format!("values:{spreadsheet_id}:{sheet_name}");

    if let Some(body) = state()
        .cache
        .get_values(&cache_key)
        .map_err(GoogleError::cache)?
    {
        return Ok(body);
    }

    let url = format!(
        "https://sheets.googleapis.com/v4/spreadsheets/{spreadsheet_id}/values/{}?key={}",
        urlencoding::encode(sheet_name),
        state().api_key
    );

    state()
        .inflight
        .run(cache_key.clone(), || async {
            if let Some(body) = state()
                .cache
                .get_values(&cache_key)
                .map_err(GoogleError::cache)?
            {
                return Ok(body);
            }

            let (status, body) = fetch_json(&url).await?;
            let payload: SheetValuesResponse =
                serde_json::from_slice(&body).map_err(GoogleError::json_decode)?;
            if let Some(error) = payload.error {
                return Err(GoogleError::bad_request(error.message));
            }
            if status != StatusCode::OK {
                return Err(GoogleError::bad_request("Google Sheets request failed"));
            }
            let ttl_seconds = state().throttle.cache_ttl_secs();
            state()
                .cache
                .put_values(&cache_key, &body, ttl_seconds)
                .map_err(GoogleError::cache)?;
            Ok(body)
        })
        .await
}

async fn fetch_json(url: &str) -> Result<(StatusCode, Bytes), GoogleError> {
    state().throttle.acquire().await?;

    let request = Request::builder()
        .method(Method::GET)
        .uri(url)
        .header("accept", "application/json")
        .body(Empty::new())
        .map_err(GoogleError::request_build)?;

    let response = state()
        .http
        .request(request)
        .await
        .map_err(GoogleError::upstream)?;
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .map_err(GoogleError::body_read)?
        .to_bytes();

    Ok((status, body))
}

struct GoogleThrottle {
    limit: usize,
    window: Duration,
    queue: Semaphore,
    recent: Mutex<VecDeque<Instant>>,
}

impl GoogleThrottle {
    fn new(limit: usize, window: Duration, max_queued: usize) -> Self {
        Self {
            limit,
            window,
            queue: Semaphore::new(max_queued),
            recent: Mutex::new(VecDeque::with_capacity(limit)),
        }
    }

    async fn acquire(&self) -> Result<(), GoogleError> {
        let queue_permit = self
            .queue
            .try_acquire()
            .map_err(|_| GoogleError::too_many_requests("Too many Google requests queued"))?;

        loop {
            let wait_for = {
                let mut recent = self.recent.lock().await;
                let now = Instant::now();
                while recent
                    .front()
                    .is_some_and(|instant| now.duration_since(*instant) >= self.window)
                {
                    recent.pop_front();
                }

                if recent.len() < self.limit {
                    recent.push_back(now);
                    None
                } else {
                    Some(self.window - now.duration_since(*recent.front().expect("recent entry")))
                }
            };

            if let Some(wait_for) = wait_for {
                sleep(wait_for).await;
            } else {
                drop(queue_permit);
                return Ok(());
            }
        }
    }

    fn cache_ttl_secs(&self) -> u64 {
        let queued = GOOGLE_MAX_QUEUED_REQUESTS.saturating_sub(self.queue.available_permits());
        let multiplier = if queued >= 48 {
            4
        } else if queued >= 24 {
            3
        } else if queued >= 8 {
            2
        } else {
            1
        };

        (GOOGLE_CACHE_TTL_SECS * multiplier).min(GOOGLE_CACHE_TTL_MAX_SECS)
    }
}

#[derive(Clone)]
struct HeedCache {
    env: Env,
    entries: CacheDb,
    expiry: ExpiryDb,
}

impl HeedCache {
    fn open(path: impl AsRef<Path>) -> CacheResult<Self> {
        fs::create_dir_all(path.as_ref())?;

        let env = unsafe { EnvOpenOptions::new().max_dbs(2).open(path.as_ref())? };
        let mut wtxn = env.write_txn()?;
        let entries = env.create_database(&mut wtxn, Some(CACHE_DB))?;
        let expiry = env.create_database(&mut wtxn, Some(EXPIRY_DB))?;
        wtxn.commit()?;

        Ok(Self {
            env,
            entries,
            expiry,
        })
    }

    fn get_metadata(&self, key: &str) -> CacheResult<Option<Bytes>> {
        self.get(&cache_key_prefix("m", key))
    }

    fn put_metadata(&self, key: &str, body: &Bytes, ttl_seconds: u64) -> CacheResult<()> {
        self.put(&cache_key_prefix("m", key), body, ttl_seconds)
    }

    fn get_values(&self, key: &str) -> CacheResult<Option<Bytes>> {
        self.get(&cache_key_prefix("v", key))
    }

    fn put_values(&self, key: &str, body: &Bytes, ttl_seconds: u64) -> CacheResult<()> {
        self.put(&cache_key_prefix("v", key), body, ttl_seconds)
    }

    fn get(&self, key: &str) -> CacheResult<Option<Bytes>> {
        self.purge_expired()?;
        let rtxn = self.env.read_txn()?;
        Ok(self
            .entries
            .get(&rtxn, key)?
            .map(|entry| Bytes::from(entry.body)))
    }

    fn put(&self, key: &str, body: &Bytes, ttl_seconds: u64) -> CacheResult<()> {
        self.purge_expired()?;

        let expires_at_unix_seconds = now_unix_seconds() + ttl_seconds;
        let entry = CacheEntry {
            body: body.to_vec(),
            expires_at_unix_seconds,
        };

        let mut wtxn = self.env.write_txn()?;
        if let Some(previous) = self.entries.get(&wtxn, key)? {
            let previous_expiry_key = expiry_key(previous.expires_at_unix_seconds, key);
            self.expiry
                .delete(&mut wtxn, previous_expiry_key.as_slice())?;
        }
        self.entries.put(&mut wtxn, key, &entry)?;
        let expiry_key = expiry_key(expires_at_unix_seconds, key);
        self.expiry.put(&mut wtxn, expiry_key.as_slice(), &())?;
        wtxn.commit()?;
        Ok(())
    }

    fn purge_expired(&self) -> CacheResult<()> {
        let now = now_unix_seconds();
        let mut upper_bound = Vec::with_capacity(9);
        upper_bound.extend_from_slice(&now.to_be_bytes());
        upper_bound.push(0xff);

        let expired_entries = {
            let rtxn = self.env.read_txn()?;
            let mut expired_entries = Vec::new();
            for result in self.expiry.iter(&rtxn)? {
                let (key, _) = result?;
                if key > upper_bound.as_slice() {
                    break;
                }
                expired_entries.push(key.to_vec());
            }
            expired_entries
        };

        if expired_entries.is_empty() {
            return Ok(());
        }

        let mut wtxn = self.env.write_txn()?;
        for expiry_key in expired_entries {
            let cache_key = std::str::from_utf8(expiry_key.get(8..).ok_or("invalid expiry key")?)?;
            self.entries.delete(&mut wtxn, cache_key)?;
            self.expiry.delete(&mut wtxn, expiry_key.as_slice())?;
        }
        wtxn.commit()?;
        Ok(())
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct CacheEntry {
    body: Vec<u8>,
    expires_at_unix_seconds: u64,
}

#[derive(Clone, Default)]
struct InflightRequests {
    entries: Arc<Mutex<HashMap<String, Arc<InflightEntry>>>>,
}

impl InflightRequests {
    fn new() -> Self {
        Self::default()
    }

    async fn run<F, Fut>(&self, key: String, operation: F) -> Result<Bytes, GoogleError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Bytes, GoogleError>>,
    {
        let (entry, leader) = {
            let mut entries = self.entries.lock().await;
            if let Some(entry) = entries.get(&key) {
                (entry.clone(), false)
            } else {
                let entry = Arc::new(InflightEntry::default());
                entries.insert(key.clone(), entry.clone());
                (entry, true)
            }
        };

        if !leader {
            return entry.wait().await;
        }

        let result = operation().await;
        entry.finish(result.clone()).await;

        let mut entries = self.entries.lock().await;
        entries.remove(&key);

        result
    }
}

#[derive(Default)]
struct InflightEntry {
    notify: Notify,
    result: Mutex<Option<Result<Bytes, GoogleError>>>,
}

impl InflightEntry {
    async fn wait(&self) -> Result<Bytes, GoogleError> {
        loop {
            if let Some(result) = self.result.lock().await.clone() {
                return result;
            }
            self.notify.notified().await;
        }
    }

    async fn finish(&self, result: Result<Bytes, GoogleError>) {
        *self.result.lock().await = Some(result);
        self.notify.notify_waiters();
    }
}

fn error_response(message: String, status: StatusCode) -> Response<ResBody> {
    eprintln!("{} {}", status.as_u16(), message);

    let body = serde_json::to_vec(&ErrorBody {
        error: &message,
        documentation: README_URL,
    })
    .expect("failed to serialize error response");

    json_response(status, ERROR_CACHE_CONTROL, body)
}

fn json_response(
    status: StatusCode,
    cache_control: &'static str,
    body: impl Into<Bytes>,
) -> Response<ResBody> {
    let mut response = Response::new(Full::new(body.into()));
    *response.status_mut() = status;

    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    headers.insert(
        ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static(BASE_ALLOWED_HEADERS),
    );
    headers.insert(CACHE_CONTROL, HeaderValue::from_static(cache_control));

    response
}

fn parse_metadata(body: &[u8]) -> Result<SpreadsheetMetadataResponse, GoogleError> {
    let payload: SpreadsheetMetadataResponse =
        serde_json::from_slice(body).map_err(GoogleError::json_decode)?;

    if let Some(error) = payload.error.as_ref() {
        return Err(GoogleError::bad_request(error.message.clone()));
    }

    Ok(payload)
}
fn values_to_rows(values: Vec<Vec<Value>>) -> Vec<Value> {
    let mut rows = values.into_iter();
    let Some(headers) = rows.next() else {
        return Vec::new();
    };

    let mut shaped = Vec::with_capacity(rows.len());
    for row in rows {
        let mut object = Map::with_capacity(row.len());
        for (index, item) in row.into_iter().enumerate() {
            let key = headers
                .get(index)
                .and_then(Value::as_str)
                .unwrap_or("undefined")
                .to_owned();
            object.insert(key, item);
        }
        shaped.push(Value::Object(object));
    }

    shaped
}

fn expiry_key(expires_at_unix_seconds: u64, cache_key: &str) -> Vec<u8> {
    let key_bytes = cache_key.as_bytes();
    let mut encoded = Vec::with_capacity(8 + key_bytes.len());
    encoded.extend_from_slice(&expires_at_unix_seconds.to_be_bytes());
    encoded.extend_from_slice(key_bytes);
    encoded
}

fn cache_key_prefix(kind: &str, key: &str) -> String {
    let mut prefixed = String::with_capacity(kind.len() + 1 + key.len());
    prefixed.push_str(kind);
    prefixed.push(':');
    prefixed.push_str(key);
    prefixed
}

fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time drifted before UNIX_EPOCH")
        .as_secs()
}

#[derive(Clone, Debug)]
struct GoogleError {
    message: String,
    status: StatusCode,
}

impl GoogleError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status: StatusCode::BAD_REQUEST,
        }
    }

    fn upstream(error: hyper_util::client::legacy::Error) -> Self {
        Self {
            message: error.to_string(),
            status: StatusCode::BAD_GATEWAY,
        }
    }

    fn request_build(error: hyper::http::Error) -> Self {
        Self {
            message: error.to_string(),
            status: StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn body_read(error: hyper::Error) -> Self {
        Self {
            message: error.to_string(),
            status: StatusCode::BAD_GATEWAY,
        }
    }

    fn json_decode(error: serde_json::Error) -> Self {
        Self {
            message: error.to_string(),
            status: StatusCode::BAD_GATEWAY,
        }
    }

    fn json_encode(error: serde_json::Error) -> Self {
        Self {
            message: error.to_string(),
            status: StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn cache(error: impl std::fmt::Display) -> Self {
        Self {
            message: format!("Cache error: {error}"),
            status: StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn too_many_requests(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status: StatusCode::TOO_MANY_REQUESTS,
        }
    }
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
    documentation: &'a str,
}

#[derive(Deserialize)]
struct SpreadsheetMetadataResponse {
    #[serde(default)]
    sheets: Vec<SpreadsheetSheet>,
    error: Option<GoogleApiError>,
}

#[derive(Deserialize)]
struct SpreadsheetSheet {
    properties: SpreadsheetSheetProperties,
}

#[derive(Deserialize)]
struct SpreadsheetSheetProperties {
    title: String,
}

#[derive(Deserialize)]
struct SheetValuesResponse {
    values: Option<Vec<Vec<Value>>>,
    error: Option<GoogleApiError>,
}

#[derive(Clone, Deserialize)]
struct GoogleApiError {
    message: String,
}

#[cfg(test)]
mod tests {
    use std::{
        env,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
        time::Duration,
    };

    use serde_json::Value;
    use tempfile::{TempDir, tempdir};
    use tokio::time::sleep;

    use super::*;

    fn test_state() -> TempDir {
        let dir = tempdir().expect("tempdir");
        let _ = STATE.set(State {
            http: Client::builder(TokioExecutor::new()).build(
                HttpsConnectorBuilder::new()
                    .with_webpki_roots()
                    .https_only()
                    .enable_http1()
                    .build(),
            ),
            api_key: "test-key".to_string(),
            cache: HeedCache::open(dir.path()).expect("open cache"),
            inflight: InflightRequests::new(),
            throttle: GoogleThrottle::new(
                GOOGLE_RATE_LIMIT,
                GOOGLE_RATE_WINDOW,
                GOOGLE_MAX_QUEUED_REQUESTS,
            ),
        });
        dir
    }

    fn live_ready() -> Option<TempDir> {
        let _ = dotenvy::dotenv();
        let api_key = env::var("GOOGLE_API_KEY").ok()?;
        let dir = tempdir().ok()?;
        let _ = STATE.set(State {
            http: Client::builder(TokioExecutor::new()).build(
                HttpsConnectorBuilder::new()
                    .with_webpki_roots()
                    .https_only()
                    .enable_http1()
                    .build(),
            ),
            api_key,
            cache: HeedCache::open(dir.path()).ok()?,
            inflight: InflightRequests::new(),
            throttle: GoogleThrottle::new(
                GOOGLE_RATE_LIMIT,
                GOOGLE_RATE_WINDOW,
                GOOGLE_MAX_QUEUED_REQUESTS,
            ),
        });
        Some(dir)
    }

    #[tokio::test]
    async fn rejects_any_query_string() {
        let _cache_dir = test_state();
        let uri: hyper::Uri = "/spreadsheet/sheet?v=1".parse().expect("uri");

        let response = route_request(&Method::GET, &uri).await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn rejects_extra_path_segments() {
        let _cache_dir = test_state();
        let uri: hyper::Uri = "/spreadsheet/sheet/extra".parse().expect("uri");

        let response = route_request(&Method::GET, &uri).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn returns_whole_document_when_path_has_no_sheet_name() {
        let _cache_dir = test_state();
        let spreadsheet_id = "spreadsheet-document";
        state()
            .cache
            .put_metadata(
                &format!("metadata:{spreadsheet_id}"),
                &Bytes::from_static(
                    br#"{"sheets":[{"properties":{"title":"Sheet One"}},{"properties":{"title":"Sheet Two"}}]}"#,
                ),
                60,
            )
            .expect("put metadata");
        state()
            .cache
            .put_values(
                &format!("values:{spreadsheet_id}:Sheet One"),
                &Bytes::from_static(br#"{"values":[["name"],["alice"]]}"#),
                60,
            )
            .expect("put values");
        state()
            .cache
            .put_values(
                &format!("values:{spreadsheet_id}:Sheet Two"),
                &Bytes::from_static(br#"{"values":[["name"],["bob"]]}"#),
                60,
            )
            .expect("put values");

        let uri: hyper::Uri = format!("/{spreadsheet_id}").parse().expect("uri");
        let response = route_request(&Method::GET, &uri).await;
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("collect body")
            .to_bytes();

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            Bytes::from_static(br#"{"Sheet One":[{"name":"alice"}],"Sheet Two":[{"name":"bob"}]}"#)
        );
    }

    #[tokio::test]
    async fn serves_cached_google_values_from_raw_route() {
        let _cache_dir = test_state();
        let spreadsheet_id = "spreadsheet-raw-single";
        let raw_body = Bytes::from_static(br#"{"values":[["name"],["alice"]]}"#);
        state()
            .cache
            .put_metadata(
                &format!("metadata:{spreadsheet_id}"),
                &Bytes::from_static(br#"{"sheets":[{"properties":{"title":"Sheet One"}}]}"#),
                60,
            )
            .expect("put metadata");
        state()
            .cache
            .put_values(&format!("values:{spreadsheet_id}:Sheet One"), &raw_body, 60)
            .expect("put values");

        let uri: hyper::Uri = format!("/raw/{spreadsheet_id}/Sheet%20One")
            .parse()
            .expect("uri");
        let response = route_request(&Method::GET, &uri).await;
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("collect body")
            .to_bytes();

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, raw_body);
    }

    #[tokio::test]
    async fn serves_cached_google_values_for_whole_document_raw_route() {
        let _cache_dir = test_state();
        let spreadsheet_id = "spreadsheet-raw-document";
        state()
            .cache
            .put_metadata(
                &format!("metadata:{spreadsheet_id}"),
                &Bytes::from_static(
                    br#"{"sheets":[{"properties":{"title":"Sheet One"}},{"properties":{"title":"Sheet Two"}}]}"#,
                ),
                60,
            )
            .expect("put metadata");
        state()
            .cache
            .put_values(
                &format!("values:{spreadsheet_id}:Sheet One"),
                &Bytes::from_static(br#"{"values":[["name"],["alice"]]}"#),
                60,
            )
            .expect("put values");
        state()
            .cache
            .put_values(
                &format!("values:{spreadsheet_id}:Sheet Two"),
                &Bytes::from_static(br#"{"values":[["name"],["bob"]]}"#),
                60,
            )
            .expect("put values");

        let uri: hyper::Uri = format!("/raw/{spreadsheet_id}").parse().expect("uri");
        let response = route_request(&Method::GET, &uri).await;
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("collect body")
            .to_bytes();

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            Bytes::from_static(
                br#"{"Sheet One":{"values":[["name"],["alice"]]},"Sheet Two":{"values":[["name"],["bob"]]}}"#,
            )
        );
    }

    #[tokio::test]
    async fn ignores_trailing_slashes() {
        let _cache_dir = test_state();
        let spreadsheet_id = "spreadsheet-trailing";
        state()
            .cache
            .put_values(
                &format!("values:{spreadsheet_id}:Sheet One"),
                &Bytes::from_static(br#"{"values":[["name"],["alice"]]}"#),
                60,
            )
            .expect("put values");

        let uri: hyper::Uri = format!("/{spreadsheet_id}/Sheet%20One/")
            .parse()
            .expect("uri");
        let response = route_request(&Method::GET, &uri).await;
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("collect body")
            .to_bytes();

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, Bytes::from_static(br#"[{"name":"alice"}]"#));
    }

    #[tokio::test]
    async fn coalesces_duplicate_inflight_requests() {
        let inflight = InflightRequests::new();
        let calls = Arc::new(AtomicUsize::new(0));

        let first_calls = calls.clone();
        let first = {
            let inflight = inflight.clone();
            tokio::spawn(async move {
                inflight
                    .run("sheet-key".to_string(), move || async move {
                        first_calls.fetch_add(1, Ordering::SeqCst);
                        sleep(Duration::from_millis(50)).await;
                        Ok(Bytes::from_static(br#"[{"name":"alice"}]"#))
                    })
                    .await
            })
        };

        let second = {
            let inflight = inflight.clone();
            tokio::spawn(async move {
                inflight
                    .run("sheet-key".to_string(), move || async move {
                        Err(GoogleError {
                            message: "should not execute".to_string(),
                            status: StatusCode::INTERNAL_SERVER_ERROR,
                        })
                    })
                    .await
            })
        };

        let first_result = first.await.expect("first task panicked");
        let second_result = second.await.expect("second task panicked");

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            first_result.unwrap(),
            Bytes::from_static(br#"[{"name":"alice"}]"#)
        );
        assert_eq!(
            second_result.unwrap(),
            Bytes::from_static(br#"[{"name":"alice"}]"#)
        );
    }

    #[tokio::test]
    async fn propagates_errors_to_duplicate_waiters() {
        let inflight = InflightRequests::new();

        let first = {
            let inflight = inflight.clone();
            tokio::spawn(async move {
                inflight
                    .run("sheet-key".to_string(), move || async move {
                        sleep(Duration::from_millis(25)).await;
                        Err(GoogleError {
                            message: "upstream failed".to_string(),
                            status: StatusCode::BAD_GATEWAY,
                        })
                    })
                    .await
            })
        };

        let second = {
            let inflight = inflight.clone();
            tokio::spawn(async move {
                inflight
                    .run("sheet-key".to_string(), move || async move {
                        Ok(Bytes::from_static(b"should not execute"))
                    })
                    .await
            })
        };

        let first_result = first.await.expect("first task panicked");
        let second_result = second.await.expect("second task panicked");

        assert_eq!(first_result.unwrap_err().message, "upstream failed");
        assert_eq!(second_result.unwrap_err().message, "upstream failed");
    }

    #[tokio::test]
    async fn rejects_google_requests_when_throttle_queue_is_full() {
        let throttle = Arc::new(GoogleThrottle::new(1, Duration::from_millis(200), 1));

        throttle.acquire().await.expect("first slot");

        let waiting = {
            let throttle = throttle.clone();
            tokio::spawn(async move { throttle.acquire().await })
        };

        sleep(Duration::from_millis(10)).await;

        let error = throttle.acquire().await.expect_err("queue should be full");
        assert_eq!(error.status, StatusCode::TOO_MANY_REQUESTS);

        waiting
            .await
            .expect("waiting task panicked")
            .expect("waiting slot");
    }

    #[test]
    fn extends_cache_ttl_under_queue_pressure() {
        let throttle = GoogleThrottle::new(1, Duration::from_secs(60), GOOGLE_MAX_QUEUED_REQUESTS);

        assert_eq!(throttle.cache_ttl_secs(), GOOGLE_CACHE_TTL_SECS);

        let permits: Vec<_> = (0..8)
            .map(|_| throttle.queue.try_acquire().expect("queue permit"))
            .collect();
        assert_eq!(throttle.cache_ttl_secs(), GOOGLE_CACHE_TTL_SECS * 2);
        drop(permits);
    }

    #[tokio::test]
    #[ignore = "requires GOOGLE_API_KEY and TEST_SHEET_ID"]
    async fn fetches_sheet_json_with_live_google_data() {
        let _cache_dir = live_ready().expect("missing GOOGLE_API_KEY");
        let sheet_id = env::var("TEST_SHEET_ID").expect("missing TEST_SHEET_ID");

        let json = fetch_document_json(&sheet_id)
            .await
            .expect("fetch sheet JSON");

        let payload: Value = serde_json::from_slice(&json).expect("valid JSON response");
        assert!(payload.is_object());
    }

    #[test]
    fn expired_entry_is_removed_on_direct_read() {
        let dir = tempdir().expect("tempdir");
        let cache = HeedCache::open(dir.path()).expect("open cache");
        let payload = Bytes::from_static(b"payload");

        cache
            .put_metadata("metadata:test", &payload, 0)
            .expect("put metadata");

        assert_eq!(
            cache.get_metadata("metadata:test").expect("get metadata"),
            None
        );

        let rtxn = cache.env.read_txn().expect("read txn");
        assert!(
            cache
                .entries
                .get(&rtxn, cache_key_prefix("m", "metadata:test").as_str())
                .expect("raw metadata get")
                .is_none()
        );
    }

    #[test]
    fn unrelated_read_sweeps_cold_expired_entries() {
        let dir = tempdir().expect("tempdir");
        let cache = HeedCache::open(dir.path()).expect("open cache");
        let payload = Bytes::from_static(b"payload");

        cache
            .put_values("values:expired", &payload, 1)
            .expect("put expired value");

        let expired_index_key = {
            let rtxn = cache.env.read_txn().expect("read txn");
            let prefixed_key = cache_key_prefix("v", "values:expired");
            let entry = cache
                .entries
                .get(&rtxn, prefixed_key.as_str())
                .expect("raw expired value get")
                .expect("expired value entry");
            expiry_key(entry.expires_at_unix_seconds, prefixed_key.as_str())
        };

        thread::sleep(Duration::from_secs(1));

        cache
            .put_values("values:fresh", &payload, 60)
            .expect("put fresh value");

        assert_eq!(
            cache.get_values("values:fresh").expect("get fresh value"),
            Some(payload.clone())
        );

        let rtxn = cache.env.read_txn().expect("read txn");
        assert!(
            cache
                .entries
                .get(&rtxn, cache_key_prefix("v", "values:expired").as_str())
                .expect("raw expired value get")
                .is_none()
        );
        assert_eq!(
            cache
                .expiry
                .get(&rtxn, expired_index_key.as_slice())
                .expect("raw expiry get"),
            None
        );
    }
}
