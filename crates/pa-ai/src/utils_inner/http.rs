//! Minimal async HTTP client plumbing for providers.
//!
//! Providers in the TS reference go through SDK clients configured with
//! `maxRetries: 0`; here each provider issues one streaming HTTP request
//! through a shared `reqwest` client. Aborts are surfaced as
//! [`ProviderError::Aborted`]; HTTP failures as [`ProviderError::Http`].

use std::sync::OnceLock;

use tokio_util::sync::CancellationToken;

use crate::utils::stream_failure::{ProviderError, ProviderHttpError};

static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

fn client() -> &'static reqwest::Client {
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .build()
            .expect("reqwest client")
    })
}

/// An opened HTTP response: status, headers, and the byte stream.
pub struct HttpResponse {
    pub status: u16,
    pub headers: std::collections::HashMap<String, String>,
    body: reqwest::Response,
    signal: Option<CancellationToken>,
}

impl HttpResponse {
    /// Read the next text chunk from the body (None at end of stream).
    pub async fn next_text(&mut self) -> Result<Option<String>, ProviderError> {
        if self
            .signal
            .as_ref()
            .map(|signal| signal.is_cancelled())
            .unwrap_or(false)
        {
            return Err(ProviderError::Aborted);
        }
        let signal = self.signal.clone();
        let chunk = match signal {
            Some(signal) => {
                let next = self.body.chunk();
                tokio::select! {
                    _ = signal.cancelled() => return Err(ProviderError::Aborted),
                    result = next => result,
                }
            }
            None => self.body.chunk().await,
        };
        match chunk {
            Ok(Some(bytes)) => Ok(Some(String::from_utf8_lossy(&bytes).to_string())),
            Ok(None) => Ok(None),
            Err(error) => Err(ProviderError::Http(ProviderHttpError {
                message: format!("Failed to read provider response body: {error}"),
                status: Some(self.status),
                body: None,
                headers: self.headers.clone(),
                request_id: None,
            })),
        }
    }

    /// Read the entire body as text (for error responses and small payloads).
    pub async fn read_all_text(&mut self) -> Result<String, ProviderError> {
        let mut out = String::new();
        while let Some(chunk) = self.next_text().await? {
            out.push_str(&chunk);
        }
        Ok(out)
    }
}

pub struct RequestOptions {
    pub method: reqwest::Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub signal: Option<CancellationToken>,
    pub timeout_ms: Option<u64>,
}

/// Issue a request and return the response with a streaming body. No retries:
/// retry ownership lives with the caller (agent layer), matching the TS
/// `maxRetries: 0` client configuration.
pub async fn send(request: RequestOptions) -> Result<HttpResponse, ProviderError> {
    let signal = request.signal.clone();
    if signal
        .as_ref()
        .map(|signal| signal.is_cancelled())
        .unwrap_or(false)
    {
        return Err(ProviderError::Aborted);
    }

    let mut builder = client().request(request.method, &request.url);
    for (name, value) in &request.headers {
        builder = builder.header(name, value);
    }
    if let Some(body) = &request.body {
        builder = builder.body(body.clone());
    }

    let send_future = builder.send();
    let response = match (&signal, request.timeout_ms) {
        (Some(signal), _) => {
            tokio::select! {
                _ = signal.cancelled() => return Err(ProviderError::Aborted),
                result = send_future => result,
            }
        }
        (None, Some(timeout_ms)) => {
            match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), send_future)
                .await
            {
                Ok(result) => result,
                Err(_) => {
                    return Err(ProviderError::Http(ProviderHttpError {
                        message: format!("Request timed out after {timeout_ms}ms"),
                        status: None,
                        body: None,
                        headers: Default::default(),
                        request_id: None,
                    }))
                }
            }
        }
        (None, None) => send_future.await,
    };

    let response = response.map_err(|error| {
        if error.is_timeout() {
            ProviderError::Http(ProviderHttpError {
                message: format!("Request timed out: {error}"),
                status: None,
                body: None,
                headers: Default::default(),
                request_id: None,
            })
        } else if error.is_connect() {
            ProviderError::Http(ProviderHttpError {
                message: format!("Connection failed: {error}"),
                status: None,
                body: None,
                headers: Default::default(),
                request_id: None,
            })
        } else {
            ProviderError::Message(format!("Request failed: {error}"))
        }
    })?;

    let status = response.status().as_u16();
    let mut headers = std::collections::HashMap::new();
    for (name, value) in response.headers() {
        if let Ok(value) = value.to_str() {
            headers.insert(name.as_str().to_ascii_lowercase(), value.to_string());
        }
    }

    Ok(HttpResponse {
        status,
        headers,
        body: response,
        signal,
    })
}

/// JSON POST helper used by non-streaming calls (OAuth token refresh, catalogs).
#[allow(dead_code)] // token refresh/catalog fetches for upcoming providers
pub async fn post_json(
    url: &str,
    headers: Vec<(String, String)>,
    body: serde_json::Value,
    signal: Option<CancellationToken>,
) -> Result<(u16, serde_json::Value), ProviderError> {
    let mut response = send(RequestOptions {
        method: reqwest::Method::POST,
        url: url.to_string(),
        headers,
        body: Some(body.to_string()),
        signal,
        timeout_ms: Some(30_000),
    })
    .await?;
    let status = response.status;
    let text = response.read_all_text().await?;
    let parsed = if text.trim().is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(&text)
            .map_err(|error| ProviderError::Message(format!("Invalid JSON response: {error}")))?
    };
    Ok((status, parsed))
}
