use std::{
    collections::HashMap,
    fs,
    future::Future,
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use heed::{
    Database, Env, EnvOpenOptions,
    types::{SerdeBincode, Str},
};
use http_body_util::{BodyExt, Empty};
use hyper::{Method, Request, StatusCode};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::{Mutex, Notify};

const GOOGLE_CACHE_TTL_SECS: u64 = 300;
const METADATA_DB: &str = "metadata";
const VALUES_DB: &str = "values";

type CacheDb = Database<Str, SerdeBincode<CacheEntry>>;
type CacheResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
type HttpClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Empty<Bytes>>;

#[derive(Clone)]
pub struct GoogleSheetsClient {
    http: HttpClient,
    api_key: String,
    cache: HeedCache,
    inflight: InflightRequests,
}

impl GoogleSheetsClient {
    pub fn new(api_key: String, cache_dir: impl AsRef<Path>) -> CacheResult<Self> {
        Ok(Self {
            http: Client::builder(TokioExecutor::new()).build(
                HttpsConnectorBuilder::new()
                    .with_webpki_roots()
                    .https_only()
                    .enable_http1()
                    .build(),
            ),
            api_key,
            cache: HeedCache::open(cache_dir)?,
            inflight: InflightRequests::new(),
        })
    }

    pub async fn fetch_sheet_json(
        &self,
        spreadsheet_id: &str,
        sheet_param: &str,
    ) -> Result<Vec<u8>, GoogleError> {
        let sheet_name = self.resolve_sheet_name(spreadsheet_id, sheet_param).await?;
        let body = self.fetch_values_bytes(spreadsheet_id, &sheet_name).await?;
        let payload: SheetValuesResponse =
            serde_json::from_slice(&body).map_err(GoogleError::json_decode)?;

        if let Some(error) = payload.error {
            return Err(GoogleError::bad_request(error.message));
        }

        let rows = values_to_rows(payload.values.unwrap_or_default());
        serde_json::to_vec(&rows).map_err(GoogleError::json_encode)
    }

    async fn resolve_sheet_name(
        &self,
        spreadsheet_id: &str,
        sheet_param: &str,
    ) -> Result<String, GoogleError> {
        let decoded = decode_sheet_param(sheet_param)?;

        if let Ok(sheet_number) = decoded.parse::<usize>() {
            if sheet_number == 0 {
                return Err(GoogleError::bad_request(
                    "For this API, sheet numbers start at 1",
                ));
            }

            let metadata = self.fetch_spreadsheet_metadata(spreadsheet_id).await?;
            let index = sheet_number - 1;
            let sheet = metadata.sheets.get(index).ok_or_else(|| {
                GoogleError::bad_request(format!("There is no sheet number {decoded}"))
            })?;

            return Ok(sheet.properties.title.clone());
        }

        Ok(decoded)
    }

    async fn fetch_spreadsheet_metadata(
        &self,
        spreadsheet_id: &str,
    ) -> Result<SpreadsheetMetadataResponse, GoogleError> {
        let cache_key = format!("metadata:{spreadsheet_id}");

        if let Some(body) = self
            .cache
            .get_metadata(&cache_key)
            .map_err(GoogleError::cache)?
        {
            return parse_metadata(&body);
        }

        let url = format!(
            "https://sheets.googleapis.com/v4/spreadsheets/{spreadsheet_id}?key={}",
            self.api_key
        );

        let body = self
            .inflight
            .run(cache_key.clone(), || async {
                if let Some(body) = self
                    .cache
                    .get_metadata(&cache_key)
                    .map_err(GoogleError::cache)?
                {
                    return Ok(body);
                }

                let (status, body) = self.fetch_json(&url).await?;
                parse_metadata_with_status(status, &body)?;
                self.cache
                    .put_metadata(&cache_key, &body, GOOGLE_CACHE_TTL_SECS)
                    .map_err(GoogleError::cache)?;
                Ok(body)
            })
            .await?;

        parse_metadata(&body)
    }

    async fn fetch_values_bytes(
        &self,
        spreadsheet_id: &str,
        sheet_name: &str,
    ) -> Result<Bytes, GoogleError> {
        let cache_key = format!("values:{spreadsheet_id}:{sheet_name}");

        if let Some(body) = self
            .cache
            .get_values(&cache_key)
            .map_err(GoogleError::cache)?
        {
            return Ok(body);
        }

        let url = format!(
            "https://sheets.googleapis.com/v4/spreadsheets/{spreadsheet_id}/values/{}?key={}",
            urlencoding::encode(sheet_name),
            self.api_key
        );

        self.inflight
            .run(cache_key.clone(), || async {
                if let Some(body) = self
                    .cache
                    .get_values(&cache_key)
                    .map_err(GoogleError::cache)?
                {
                    return Ok(body);
                }

                let (status, body) = self.fetch_json(&url).await?;
                parse_values_with_status(status, &body)?;
                self.cache
                    .put_values(&cache_key, &body, GOOGLE_CACHE_TTL_SECS)
                    .map_err(GoogleError::cache)?;
                Ok(body)
            })
            .await
    }

    async fn fetch_json(&self, url: &str) -> Result<(StatusCode, Bytes), GoogleError> {
        let request = Request::builder()
            .method(Method::GET)
            .uri(url)
            .header("accept", "application/json")
            .body(Empty::new())
            .map_err(GoogleError::request_build)?;

        let response = self
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
}

#[derive(Clone)]
struct HeedCache {
    env: Env,
    metadata: CacheDb,
    values: CacheDb,
}

impl HeedCache {
    fn open(path: impl AsRef<Path>) -> CacheResult<Self> {
        fs::create_dir_all(path.as_ref())?;

        let env = unsafe { EnvOpenOptions::new().max_dbs(4).open(path.as_ref())? };
        let mut wtxn = env.write_txn()?;
        let metadata = env.create_database(&mut wtxn, Some(METADATA_DB))?;
        let values = env.create_database(&mut wtxn, Some(VALUES_DB))?;
        wtxn.commit()?;

        Ok(Self {
            env,
            metadata,
            values,
        })
    }

    fn get_metadata(&self, key: &str) -> CacheResult<Option<Bytes>> {
        self.get(&self.metadata, key)
    }

    fn put_metadata(&self, key: &str, body: &Bytes, ttl_seconds: u64) -> CacheResult<()> {
        self.put(&self.metadata, key, body, ttl_seconds)
    }

    fn get_values(&self, key: &str) -> CacheResult<Option<Bytes>> {
        self.get(&self.values, key)
    }

    fn put_values(&self, key: &str, body: &Bytes, ttl_seconds: u64) -> CacheResult<()> {
        self.put(&self.values, key, body, ttl_seconds)
    }

    fn get(&self, db: &CacheDb, key: &str) -> CacheResult<Option<Bytes>> {
        let now = now_unix_seconds();
        let rtxn = self.env.read_txn()?;
        let entry = db.get(&rtxn, key)?;

        match entry {
            Some(entry) if entry.expires_at_unix_seconds > now => Ok(Some(Bytes::from(entry.body))),
            Some(_) => {
                drop(rtxn);
                let mut wtxn = self.env.write_txn()?;
                db.delete(&mut wtxn, key)?;
                wtxn.commit()?;
                Ok(None)
            }
            None => Ok(None),
        }
    }

    fn put(&self, db: &CacheDb, key: &str, body: &Bytes, ttl_seconds: u64) -> CacheResult<()> {
        let entry = CacheEntry {
            body: body.to_vec(),
            expires_at_unix_seconds: now_unix_seconds() + ttl_seconds,
        };

        let mut wtxn = self.env.write_txn()?;
        db.put(&mut wtxn, key, &entry)?;
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

fn decode_sheet_param(sheet_param: &str) -> Result<String, GoogleError> {
    let normalized = if sheet_param.contains('+') {
        sheet_param.replace('+', " ")
    } else {
        sheet_param.to_owned()
    };

    urlencoding::decode(&normalized)
        .map(|decoded| decoded.into_owned())
        .map_err(|_| GoogleError::bad_request("Invalid sheet path encoding"))
}

fn parse_metadata(body: &[u8]) -> Result<SpreadsheetMetadataResponse, GoogleError> {
    let payload: SpreadsheetMetadataResponse =
        serde_json::from_slice(body).map_err(GoogleError::json_decode)?;

    if let Some(error) = payload.error.as_ref() {
        return Err(GoogleError::bad_request(error.message.clone()));
    }

    Ok(payload)
}

fn parse_metadata_with_status(status: StatusCode, body: &[u8]) -> Result<(), GoogleError> {
    let payload = parse_metadata(body)?;

    if status != StatusCode::OK {
        return Err(GoogleError::bad_request(
            "Google Sheets metadata request failed",
        ));
    }

    drop(payload);
    Ok(())
}

fn parse_values_with_status(status: StatusCode, body: &[u8]) -> Result<(), GoogleError> {
    let payload: SheetValuesResponse =
        serde_json::from_slice(body).map_err(GoogleError::json_decode)?;

    if let Some(error) = payload.error {
        return Err(GoogleError::bad_request(error.message));
    }

    if status != StatusCode::OK {
        return Err(GoogleError::bad_request("Google Sheets request failed"));
    }

    Ok(())
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

fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time drifted before UNIX_EPOCH")
        .as_secs()
}

#[derive(Clone, Debug)]
pub struct GoogleError {
    pub message: String,
    pub status: StatusCode,
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
        time::Duration,
    };

    use bytes::Bytes;
    use hyper::StatusCode;
    use serde_json::Value;
    use tempfile::{TempDir, tempdir};
    use tokio::time::sleep;

    use super::{GoogleError, GoogleSheetsClient, InflightRequests};

    fn live_client() -> Option<(GoogleSheetsClient, String, TempDir)> {
        let _ = dotenvy::dotenv();

        let api_key = env::var("GOOGLE_API_KEY").ok()?;
        let sheet_id = env::var("TEST_SHEET_ID").ok()?;
        let dir = tempdir().ok()?;
        let client = GoogleSheetsClient::new(api_key, dir.path()).ok()?;

        Some((client, sheet_id, dir))
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
    #[ignore = "requires GOOGLE_API_KEY and TEST_SHEET_ID"]
    async fn resolves_first_sheet_name_with_live_google_data() {
        let (client, sheet_id, _cache_dir) = live_client().expect("missing live test config");

        let sheet_name = client
            .resolve_sheet_name(&sheet_id, "1")
            .await
            .expect("resolve first sheet");

        assert!(!sheet_name.is_empty());
    }

    #[tokio::test]
    #[ignore = "requires GOOGLE_API_KEY and TEST_SHEET_ID"]
    async fn fetches_sheet_json_with_live_google_data() {
        let (client, sheet_id, _cache_dir) = live_client().expect("missing live test config");

        let json = client
            .fetch_sheet_json(&sheet_id, "1")
            .await
            .expect("fetch sheet JSON");

        let payload: Value = serde_json::from_slice(&json).expect("valid JSON response");
        assert!(payload.is_array());
    }
}
