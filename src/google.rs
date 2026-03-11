use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::{Method, Request, StatusCode};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::cache::HeedCache;

#[derive(Clone)]
pub struct GoogleSheetsClient {
    http: Client<hyper_rustls::HttpsConnector<HttpConnector>, Empty<Bytes>>,
    api_key: String,
    cache: HeedCache,
}

impl GoogleSheetsClient {
    pub fn new(api_key: String, cache: HeedCache) -> Self {
        Self {
            http: Client::builder(TokioExecutor::new()).build(
                HttpsConnectorBuilder::new()
                    .with_webpki_roots()
                    .https_only()
                    .enable_http1()
                    .build(),
            ),
            api_key,
            cache,
        }
    }

    pub async fn resolve_sheet_name(
        &self,
        spreadsheet_id: &str,
        sheet_param: &str,
    ) -> Result<String, GoogleError> {
        let normalized = sheet_param.replace('+', " ");
        let decoded = urlencoding::decode(&normalized)
            .map_err(|_| GoogleError::bad_request("Invalid sheet path encoding"))?
            .into_owned();

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

    pub async fn fetch_rows(
        &self,
        spreadsheet_id: &str,
        sheet_name: &str,
    ) -> Result<Vec<Value>, GoogleError> {
        let url = format!(
            "https://sheets.googleapis.com/v4/spreadsheets/{spreadsheet_id}/values/{}?key={}",
            urlencoding::encode(sheet_name),
            self.api_key
        );

        let cache_key = format!("values:{spreadsheet_id}:{sheet_name}");
        let payload = if let Some(cached) = self
            .cache
            .get_values(&cache_key)
            .map_err(GoogleError::cache)?
        {
            serde_json::from_str::<SheetValuesResponse>(&cached)
                .map_err(GoogleError::json_decode)?
        } else {
            let (status, body) = self.fetch_json(&url).await?;
            let payload: SheetValuesResponse =
                serde_json::from_slice(&body).map_err(GoogleError::json_decode)?;

            if let Some(error) = payload.error.as_ref() {
                return Err(GoogleError::bad_request(error.message.clone()));
            }

            if status != StatusCode::OK {
                return Err(GoogleError::bad_request("Google Sheets request failed"));
            }

            let serialized = serde_json::to_string(&payload).map_err(GoogleError::cache)?;
            self.cache
                .put_values(&cache_key, &serialized, 300)
                .map_err(GoogleError::cache)?;
            payload
        };

        if let Some(error) = payload.error {
            return Err(GoogleError::bad_request(error.message));
        }

        Ok(values_to_rows(payload.values.unwrap_or_default()))
    }

    async fn fetch_spreadsheet_metadata(
        &self,
        spreadsheet_id: &str,
    ) -> Result<SpreadsheetMetadataResponse, GoogleError> {
        let cache_key = format!("metadata:{spreadsheet_id}");
        if let Some(cached) = self
            .cache
            .get_metadata(&cache_key)
            .map_err(GoogleError::cache)?
        {
            let payload = serde_json::from_str::<SpreadsheetMetadataResponse>(&cached)
                .map_err(GoogleError::cache)?;

            if let Some(error) = payload.error.as_ref() {
                return Err(GoogleError::bad_request(error.message.clone()));
            }

            return Ok(payload);
        }

        let url = format!(
            "https://sheets.googleapis.com/v4/spreadsheets/{spreadsheet_id}?key={}",
            self.api_key
        );

        let (status, body) = self.fetch_json(&url).await?;
        let payload: SpreadsheetMetadataResponse =
            serde_json::from_slice(&body).map_err(GoogleError::json_decode)?;

        if let Some(error) = payload.error.as_ref() {
            return Err(GoogleError::bad_request(error.message.clone()));
        }

        if status != StatusCode::OK {
            return Err(GoogleError::bad_request(
                "Google Sheets metadata request failed",
            ));
        }

        let serialized = serde_json::to_string(&payload).map_err(GoogleError::cache)?;
        self.cache
            .put_metadata(&cache_key, &serialized, 300)
            .map_err(GoogleError::cache)?;

        Ok(payload)
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

fn values_to_rows(values: Vec<Vec<Value>>) -> Vec<Value> {
    let mut iter = values.into_iter();
    let headers = match iter.next() {
        Some(row) => row,
        None => return Vec::new(),
    };

    iter.map(|row| {
        let mut object = Map::new();

        for (index, item) in row.into_iter().enumerate() {
            let key = headers
                .get(index)
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| "undefined".to_string());

            object.insert(key, normalize_cell(item));
        }

        Value::Object(object)
    })
    .collect()
}

fn normalize_cell(value: Value) -> Value {
    match value {
        Value::Number(number) => Value::Number(number),
        Value::Bool(boolean) => Value::Bool(boolean),
        Value::String(string) => Value::String(string),
        Value::Null => Value::Null,
        other => Value::String(other.to_string()),
    }
}

#[derive(Clone, Debug)]
pub struct GoogleError {
    pub message: String,
    pub status: u16,
}

impl GoogleError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status: 400,
        }
    }

    fn upstream(error: hyper_util::client::legacy::Error) -> Self {
        Self {
            message: error.to_string(),
            status: 502,
        }
    }

    fn request_build(error: hyper::http::Error) -> Self {
        Self {
            message: error.to_string(),
            status: 500,
        }
    }

    fn body_read(error: hyper::Error) -> Self {
        Self {
            message: error.to_string(),
            status: 502,
        }
    }

    fn json_decode(error: serde_json::Error) -> Self {
        Self {
            message: error.to_string(),
            status: 502,
        }
    }

    fn cache(error: impl std::fmt::Display) -> Self {
        Self {
            message: format!("Cache error: {error}"),
            status: 500,
        }
    }
}

#[derive(Deserialize, serde::Serialize)]
struct SpreadsheetMetadataResponse {
    #[serde(default)]
    sheets: Vec<SpreadsheetSheet>,
    error: Option<GoogleApiError>,
}

#[derive(Deserialize, serde::Serialize)]
struct SpreadsheetSheet {
    properties: SpreadsheetSheetProperties,
}

#[derive(Deserialize, serde::Serialize)]
struct SpreadsheetSheetProperties {
    title: String,
}

#[derive(Deserialize, serde::Serialize)]
struct SheetValuesResponse {
    values: Option<Vec<Vec<Value>>>,
    error: Option<GoogleApiError>,
}

#[derive(Clone, Deserialize, serde::Serialize)]
struct GoogleApiError {
    message: String,
}

#[cfg(test)]
mod tests {
    use std::env;

    use tempfile::{TempDir, tempdir};

    use super::GoogleSheetsClient;
    use crate::cache::HeedCache;

    fn live_client() -> Option<(GoogleSheetsClient, String, TempDir)> {
        let _ = dotenvy::dotenv();

        let api_key = env::var("GOOGLE_API_KEY").ok()?;
        let sheet_id = env::var("TEST_SHEET_ID").ok()?;
        let dir = tempdir().ok()?;
        let cache = HeedCache::open(dir.path()).ok()?;

        Some((GoogleSheetsClient::new(api_key, cache), sheet_id, dir))
    }

    #[tokio::test]
    async fn resolves_first_sheet_name_with_live_google_data() {
        let Some((client, sheet_id, _cache_dir)) = live_client() else {
            return;
        };

        let sheet_name = client
            .resolve_sheet_name(&sheet_id, "1")
            .await
            .expect("resolve first sheet");

        assert!(!sheet_name.is_empty());
    }

    #[tokio::test]
    async fn fetches_rows_with_live_google_data() {
        let Some((client, sheet_id, _cache_dir)) = live_client() else {
            return;
        };

        let sheet_name = client
            .resolve_sheet_name(&sheet_id, "1")
            .await
            .expect("resolve first sheet");

        let rows = client
            .fetch_rows(&sheet_id, &sheet_name)
            .await
            .expect("fetch rows");

        for row in rows {
            assert!(row.is_object());
        }
    }
}
