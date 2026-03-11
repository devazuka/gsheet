mod google;

use std::{convert::Infallible, env, net::SocketAddr};

use bytes::Bytes;
use google::GoogleSheetsClient;
use http_body_util::Full;
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
use hyper_util::rt::TokioIo;
use serde::Serialize;

const README_URL: &str = "https://github.com/benborgers/opensheet#readme";
const BASE_ALLOWED_HEADERS: &str = "Origin, X-Requested-With, Content-Type, Accept";
const SUCCESS_CACHE_CONTROL: &str = "public, max-age=60, s-maxage=60";
const ERROR_CACHE_CONTROL: &str = "public, max-age=30, s-maxage=30";

type ResBody = Full<Bytes>;

#[derive(Clone)]
struct AppState {
    google: GoogleSheetsClient,
}

#[tokio::main]
async fn main() {
    let api_key = env::var("GOOGLE_API_KEY").expect("GOOGLE_API_KEY is required");
    let cache_dir = env::var("CACHE_DIR").unwrap_or_else(|_| "./data/heed".to_string());
    let port = env::var("PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(3000);

    let state = AppState {
        google: GoogleSheetsClient::new(api_key, &cache_dir).expect("failed to initialize client"),
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

    if uri.path() == "/" {
        return redirect_response();
    }

    if uri.path() == "/up" {
        return plain_response(StatusCode::OK, "ok");
    }

    if uri.query().is_some() {
        return error_response(
            format!("Query parameters are not supported. Your request was: {uri}"),
            StatusCode::BAD_REQUEST,
        );
    }

    let path = uri.path().trim_start_matches('/');
    let Some((id, sheet)) = path.split_once('/') else {
        return not_found_response();
    };

    if id.is_empty() || sheet.is_empty() || sheet.contains('/') {
        return not_found_response();
    }

    match state.google.fetch_sheet_json(id, sheet).await {
        Ok(body) => json_bytes_response(StatusCode::OK, SUCCESS_CACHE_CONTROL, body),
        Err(error) => error_response(error.message, error.status),
    }
}

fn redirect_response() -> Response<ResBody> {
    let mut response = Response::new(Full::new(Bytes::new()));
    *response.status_mut() = StatusCode::FOUND;
    response
        .headers_mut()
        .insert(LOCATION, HeaderValue::from_static(README_URL));
    response
}

fn plain_response(status: StatusCode, body: &'static str) -> Response<ResBody> {
    let mut response = Response::new(Full::new(Bytes::from_static(body.as_bytes())));
    *response.status_mut() = status;
    response
}

fn not_found_response() -> Response<ResBody> {
    error_response(
        "URL format is /spreadsheet_id/sheet_name".to_owned(),
        StatusCode::NOT_FOUND,
    )
}

fn error_response(message: String, status: StatusCode) -> Response<ResBody> {
    eprintln!("{} {}", status.as_u16(), message);

    let body = serde_json::to_vec(&ErrorBody {
        error: &message,
        documentation: README_URL,
    })
    .expect("failed to serialize error response");

    json_bytes_response(status, ERROR_CACHE_CONTROL, body)
}

fn json_bytes_response(
    status: StatusCode,
    cache_control: &'static str,
    body: Vec<u8>,
) -> Response<ResBody> {
    let mut response = Response::new(Full::new(Bytes::from(body)));
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

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
    documentation: &'a str,
}

#[cfg(test)]
mod tests {
    use hyper::{Method, Uri};
    use tempfile::{TempDir, tempdir};

    use super::*;

    fn test_state() -> (AppState, TempDir) {
        let dir = tempdir().expect("tempdir");
        let state = AppState {
            google: GoogleSheetsClient::new("test-key".to_string(), dir.path())
                .expect("initialize client"),
        };
        (state, dir)
    }

    #[tokio::test]
    async fn rejects_any_query_string() {
        let (state, _cache_dir) = test_state();
        let uri: Uri = "/spreadsheet/sheet?v=1".parse().expect("uri");

        let response = route_request(&Method::GET, &uri, state).await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn rejects_extra_path_segments() {
        let (state, _cache_dir) = test_state();
        let uri: Uri = "/spreadsheet/sheet/extra".parse().expect("uri");

        let response = route_request(&Method::GET, &uri, state).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
