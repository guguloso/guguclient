use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use parking_lot::RwLock;
use reqwest::header::{HeaderValue, CONTENT_TYPE, COOKIE};
use reqwest::{Method, Response, StatusCode};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::error::{AppError, AppResult};

const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) ",
    "Chrome/140.0.0.0 Safari/537.36 GuguClient/",
    env!("CARGO_PKG_VERSION")
);
const CSRF_HEADER: &str = "x-csrf-token";
const MAX_ATTEMPTS: u32 = 4;
const ERROR_BODY_LIMIT: usize = 300;

/// Shared, connection-pooled Roblox client. Cheap to share via `Arc`; the
/// cookie and CSRF token sit behind short-lived locks never held across `.await`.
pub struct RobloxClient {
    http: reqwest::Client,
    cookie: RwLock<Option<Arc<str>>>,
    csrf: RwLock<Option<HeaderValue>>,
}

impl RobloxClient {
    pub fn new() -> AppResult<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .gzip(true)
            .brotli(true)
            .pool_max_idle_per_host(16)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self { http, cookie: RwLock::new(None), csrf: RwLock::new(None) })
    }

    /// Raw client for non-API downloads (thumbnail images, etc.).
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    pub fn set_cookie(&self, cookie: Option<&str>) {
        *self.cookie.write() = cookie.map(Arc::from);
        *self.csrf.write() = None;
    }

    pub async fn get<T: DeserializeOwned>(&self, url: &str) -> AppResult<T> {
        let response = self.send(Method::GET, url, None, None).await?;
        Self::decode(url, response).await
    }

    pub async fn post<B, T>(&self, url: &str, body: &B) -> AppResult<T>
    where
        B: Serialize + ?Sized,
        T: DeserializeOwned,
    {
        let bytes = Bytes::from(serde_json::to_vec(body)?);
        let response = self.send(Method::POST, url, Some(bytes), None).await?;
        Self::decode(url, response).await
    }

    /// POST without a request body (follow, unfriend, accept, ...).
    pub async fn post_empty<T: DeserializeOwned>(&self, url: &str) -> AppResult<T> {
        let response = self.send(Method::POST, url, None, None).await?;
        Self::decode(url, response).await
    }

    pub async fn patch<B, T>(&self, url: &str, body: &B) -> AppResult<T>
    where
        B: Serialize + ?Sized,
        T: DeserializeOwned,
    {
        let bytes = Bytes::from(serde_json::to_vec(body)?);
        let response = self.send(Method::PATCH, url, Some(bytes), None).await?;
        Self::decode(url, response).await
    }

    pub async fn delete<T: DeserializeOwned>(&self, url: &str) -> AppResult<T> {
        let response = self.send(Method::DELETE, url, None, None).await?;
        Self::decode(url, response).await
    }

    /// Like [`get`](Self::get) but authenticates with an explicit cookie
    /// (account validation, multi-account operations).
    pub async fn get_as(&self, url: &str, cookie: &str) -> AppResult<Response> {
        self.send(Method::GET, url, None, Some(cookie)).await
    }

    pub async fn post_as<B: Serialize + ?Sized>(&self, url: &str, body: &B, cookie: &str) -> AppResult<Response> {
        let bytes = Bytes::from(serde_json::to_vec(body)?);
        self.send(Method::POST, url, Some(bytes), Some(cookie)).await
    }

    pub async fn decode<T: DeserializeOwned>(url: &str, response: Response) -> AppResult<T> {
        let status = response.status();
        if !status.is_success() {
            let mut message = response.text().await.unwrap_or_default();
            message.truncate(ERROR_BODY_LIMIT);
            return Err(AppError::Api { status: status.as_u16(), url: url.to_owned(), message });
        }
        // Some endpoints answer 200 with an empty body; treat it as `null`.
        let bytes = response.bytes().await?;
        if bytes.is_empty() {
            return Ok(serde_json::from_slice(b"null")?);
        }
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Sends with Roblox's protocol quirks handled:
    /// - 403 + `x-csrf-token` header: store the token and replay once
    /// - 429: exponential backoff (400ms, 800ms, 1.6s)
    async fn send(
        &self,
        method: Method,
        url: &str,
        body: Option<Bytes>,
        cookie_override: Option<&str>,
    ) -> AppResult<Response> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let cookie_header = match cookie_override {
                Some(cookie) => Some(format!(".ROBLOSECURITY={cookie}")),
                None => self.cookie.read().as_deref().map(|c| format!(".ROBLOSECURITY={c}")),
            };
            let csrf = if method == Method::GET { None } else { self.csrf.read().clone() };

            let mut request = self.http.request(method.clone(), url);
            if let Some(header) = cookie_header {
                request = request.header(COOKIE, header);
            }
            if let Some(token) = csrf {
                request = request.header(CSRF_HEADER, token);
            }
            if let Some(bytes) = &body {
                // `Bytes` clones are reference-counted: replays don't copy the body.
                request = request.header(CONTENT_TYPE, "application/json").body(bytes.clone());
            }

            let response = request.send().await?;
            match response.status() {
                StatusCode::FORBIDDEN if attempt < MAX_ATTEMPTS => {
                    let Some(token) = response.headers().get(CSRF_HEADER).cloned() else {
                        return Ok(response);
                    };
                    *self.csrf.write() = Some(token);
                }
                StatusCode::TOO_MANY_REQUESTS if attempt < MAX_ATTEMPTS => {
                    tokio::time::sleep(Duration::from_millis(400 << (attempt - 1))).await;
                }
                _ => return Ok(response),
            }
        }
    }
}
