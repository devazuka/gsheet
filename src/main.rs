mod cache;
mod google;
mod inflight;

use std::{collections::HashMap, convert::Infallible, env, net::SocketAddr, sync::Arc};

use bytes::Bytes;
use cache::HeedCache;
use google::{GoogleError, GoogleSheetsClient};
use http_body_util::Full;
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
use hyper_util::rt::TokioIo;
use inflight::InflightRequests;
use rand::Rng;
use serde::Serialize;
use url::form_urlencoded;

const README_URL: &str = "https://github.com/benborgers/opensheet#readme";
const BASE_ALLOWED_HEADERS: &str = "Origin, X-Requested-With, Content-Type, Accept";

type ResBody = Full<Bytes>;

#[derive(Clone)]
struct AppState {
    google: Arc<GoogleSheetsClient>,
    inflight: InflightRequests,
}

#[tokio::main]
async fn main() {
    let api_key = env::var("GOOGLE_API_KEY").expect("GOOGLE_API_KEY is required");
    let cache_dir = env::var("CACHE_DIR").unwrap_or_else(|_| "./data/heed".to_string());
    let port = env::var("PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(3000);

    let cache = HeedCache::open(&cache_dir).expect("failed to open heed cache");
    let state = AppState {
        google: Arc::new(GoogleSheetsClient::new(api_key, cache)),
        inflight: InflightRequests::new(),
    };

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind TCP listener");

    println!("Server running on http://localhost:{port}");

    loop {
        let (stream, _) = listener.accept().await.expect("failed to accept socket");
        let io = TokioIo::new(stream);
        let state = state.clone();

        tokio::spawn(async move {
            let service = service_fn(move |request| handle_request(request, state.clone()));

            if let Err(error) = http1::Builder::new().serve_connection(io, service).await {
                eprintln!("server connection error: {error}");
            }
        });
    }
}

async fn handle_request(
    request: Request<Incoming>,
    state: AppState,
) -> Result<Response<ResBody>, Infallible> {
    let response = route_request(request.method(), request.uri(), state).await;
    Ok(response)
}

async fn route_request(method: &Method, uri: &hyper::Uri, state: AppState) -> Response<ResBody> {
    if method != Method::GET {
        return not_found_response();
    }

    let uri_string = uri.to_string();
    let path = uri.path().trim_matches('/');

    if uri.path() == "/" {
        return redirect_response();
    }

    if uri.path() == "/up" {
        return plain_response(StatusCode::OK, "ok");
    }

    let segments = path.split('/').collect::<Vec<_>>();
    if segments.len() != 2 {
        return not_found_response();
    }

    let id = segments[0];
    let sheet = segments[1];
    let query = parse_query(uri.query());

    if let Err(error) = validate_query(&query, &uri_string) {
        return error_response(error.message, error.status);
    }

    let headers = success_cache_headers();

    let google = state.google.clone();
    let result = state
        .inflight
        .run(uri_string.clone(), move || async move {
            let sheet_name = google.resolve_sheet_name(id, sheet).await?;
            let rows = google.fetch_rows(id, &sheet_name).await?;
            serde_json::to_string(&rows).map_err(|error| GoogleError {
                message: error.to_string(),
                status: 500,
            })
        })
        .await;

    match result {
        Ok(serialized) => json_string_response(StatusCode::OK, &headers, serialized),
        Err(error) => error_response(error.message, error.status),
    }
}

fn parse_query(query: Option<&str>) -> HashMap<String, String> {
    form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .into_owned()
        .collect()
}

fn validate_query(query: &HashMap<String, String>, uri: &str) -> Result<bool, GoogleError> {
    if !query.is_empty() {
        return Err(GoogleError {
            message: format!("Query parameters are not supported. Your request was: {uri}"),
            status: 400,
        });
    }

    Ok(false)
}

fn success_cache_headers() -> Vec<(&'static str, String)> {
    let cache_duration = rand::rng().random_range(30..=60);
    vec![(
        CACHE_CONTROL.as_str(),
        format!("public, max-age={cache_duration}, s-maxage={cache_duration}"),
    )]
}

fn redirect_response() -> Response<ResBody> {
    let mut response = Response::new(Full::new(Bytes::new()));
    *response.status_mut() = StatusCode::FOUND;
    response
        .headers_mut()
        .insert(LOCATION, README_URL.parse().expect("invalid README URL"));
    response
}

fn plain_response(status: StatusCode, body: &str) -> Response<ResBody> {
    let mut response = Response::new(Full::new(Bytes::from(body.to_string())));
    *response.status_mut() = status;
    response
}

fn not_found_response() -> Response<ResBody> {
    error_response("URL format is /spreadsheet_id/sheet_name".to_string(), 404)
}

fn error_response(message: String, status: u16) -> Response<ResBody> {
    println!("{status} {message}");

    let body = ErrorBody {
        error: message,
        documentation: README_URL,
    };

    json_response(
        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
        &[(
            CACHE_CONTROL.as_str(),
            "public, max-age=30, s-maxage=30".to_string(),
        )],
        body,
    )
}

fn json_response<T: Serialize>(
    status: StatusCode,
    extra_headers: &[(&'static str, String)],
    body: T,
) -> Response<ResBody> {
    let serialized = serde_json::to_string(&body).expect("failed to serialize JSON response");
    json_string_response(status, extra_headers, serialized)
}

fn json_string_response(
    status: StatusCode,
    extra_headers: &[(&'static str, String)],
    body: String,
) -> Response<ResBody> {
    let mut response = Response::new(Full::new(Bytes::from(body)));
    *response.status_mut() = status;

    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, "application/json".parse().unwrap());
    headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, "*".parse().unwrap());
    headers.insert(
        ACCESS_CONTROL_ALLOW_HEADERS,
        BASE_ALLOWED_HEADERS.parse().unwrap(),
    );

    for (name, value) in extra_headers {
        headers.insert(
            hyper::header::HeaderName::from_static(name),
            value.parse().expect("invalid header value"),
        );
    }

    response
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: String,
    documentation: &'a str,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hyper::{Method, Uri};
    use tempfile::tempdir;

    use super::*;

    fn test_state() -> AppState {
        let dir = tempdir().expect("tempdir");
        let cache = HeedCache::open(dir.path()).expect("open cache");

        AppState {
            google: Arc::new(GoogleSheetsClient::new("test-key".to_string(), cache)),
            inflight: InflightRequests::new(),
        }
    }

    #[tokio::test]
    async fn rejects_unknown_query_parameters() {
        let state = test_state();
        let uri: Uri = "/spreadsheet/sheet?v=1".parse().expect("uri");

        let response = route_request(&Method::GET, &uri, state).await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn accepts_empty_query_string() {
        let query = parse_query(None);
        let result = validate_query(&query, "/sheet/tab");

        assert!(!result.expect("expected valid query"));
    }
}
