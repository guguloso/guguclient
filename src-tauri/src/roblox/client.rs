use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use parking_lot::RwLock;
use reqwest::header::{HeaderValue, CONTENT_TYPE, COOKIE};
use reqwest::{Method, Response, StatusCode};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::error::{AppError, AppResult};
use crate::roblox::endpoints;

const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) ",
    "Chrome/140.0.0.0 Safari/537.36 GuguClient/",
    env!("CARGO_PKG_VERSION")
);
const CSRF_HEADER: &str = "x-csrf-token";
const MAX_ATTEMPTS: u32 = 4;
const WEB_REFERER: &str = "https://www.roblox.com/";
const ERROR_BODY_LIMIT: usize = 300;

/// Which credentials a request carries.
#[derive(Clone, Copy)]
enum Auth<'a> {
    /// The active session cookie (rozod's synced cookie).
    Session,
    /// An explicit account cookie.
    Cookie(&'a str),
    /// Website context; falls back to the session cookie without a tracker.
    Web {
        cookie: Option<&'a str>,
        tracker: Option<&'a str>,
    },
}

const CHALLENGE_ID_HEADER: &str = "rblx-challenge-id";
const CHALLENGE_TYPE_HEADER: &str = "rblx-challenge-type";
const CHALLENGE_METADATA_HEADER: &str = "rblx-challenge-metadata";
const MAX_CHALLENGE_RETRIES: u32 = 3;

/// A Roblox generic challenge (`parseChallengeHeaders`).
#[derive(Clone, Debug)]
pub struct Challenge {
    pub id: String,
    pub kind: String,
    pub metadata: String,
}

/// The request a challenge interrupted (`createChallengeRequestContext`).
#[derive(Clone, Debug)]
pub struct ReplayRequest {
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    pub replay_supported: bool,
}

/// What the challenge handler decided.
pub enum ChallengeOutcome {
    /// Retry with these `rblx-challenge-*` values.
    Retry(Challenge),
    /// Use this response instead (the captcha replay popup's).
    Respond(Response),
}

pub type ChallengeHandler =
    Arc<dyn Fn(Challenge, ReplayRequest) -> futures::future::BoxFuture<'static, Option<ChallengeOutcome>> + Send + Sync>;

/// Shared, connection-pooled Roblox client. Cheap to share via `Arc`; the
/// cookie and CSRF token sit behind short-lived locks never held across `.await`.
pub struct RobloxClient {
    http: reqwest::Client,
    cookie: RwLock<Option<Arc<str>>>,
    csrf: RwLock<Option<HeaderValue>>,
    accept_language: RwLock<HeaderValue>,
    challenge_handler: RwLock<Option<ChallengeHandler>>,
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
        Ok(Self {
            http,
            cookie: RwLock::new(None),
            csrf: RwLock::new(None),
            accept_language: RwLock::new(HeaderValue::from_static("en-US,en;q=0.9")),
            challenge_handler: RwLock::new(None),
        })
    }

    /// Sets the `Accept-Language` header sent on Roblox API requests.
    pub fn set_language(&self, lang: &str) {
        let header = match lang.to_ascii_lowercase().as_str() {
            l if l.starts_with("pt") => HeaderValue::from_static("pt-BR,pt;q=0.9,en-US;q=0.8,en;q=0.7"),
            l if l.starts_with("es") => HeaderValue::from_static("es-419,es;q=0.9,en-US;q=0.8,en;q=0.7"),
            _ => HeaderValue::from_static("en-US,en;q=0.9"),
        };
        *self.accept_language.write() = header;
    }

    /// Raw client for non-API downloads (thumbnail images, etc.).
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// `rozod.setHandleGenericChallenge`.
    pub fn set_challenge_handler(&self, handler: ChallengeHandler) {
        *self.challenge_handler.write() = Some(handler);
    }

    /// The active session cookie (`getAuthCookie`).
    pub fn session_cookie(&self) -> Option<String> {
        self.cookie.read().as_deref().map(str::to_owned)
    }

    pub fn set_cookie(&self, cookie: Option<&str>) {
        *self.cookie.write() = cookie.map(Arc::from);
        *self.csrf.write() = None;
    }

    pub async fn get<T: DeserializeOwned>(&self, url: &str) -> AppResult<T> {
        let response = self.send(Method::GET, url, None, Auth::Session).await?;
        Self::decode(url, response).await
    }

    pub async fn post<B, T>(&self, url: &str, body: &B) -> AppResult<T>
    where
        B: Serialize + ?Sized,
        T: DeserializeOwned,
    {
        let bytes = Bytes::from(serde_json::to_vec(body)?);
        let response = self
            .send(Method::POST, url, Some(bytes), Auth::Session)
            .await?;
        Self::decode(url, response).await
    }

    /// POST without a request body (follow, unfriend, accept, ...).
    pub async fn post_empty<T: DeserializeOwned>(&self, url: &str) -> AppResult<T> {
        let response = self.send(Method::POST, url, None, Auth::Session).await?;
        Self::decode(url, response).await
    }

    pub async fn patch<B, T>(&self, url: &str, body: &B) -> AppResult<T>
    where
        B: Serialize + ?Sized,
        T: DeserializeOwned,
    {
        let bytes = Bytes::from(serde_json::to_vec(body)?);
        let response = self
            .send(Method::PATCH, url, Some(bytes), Auth::Session)
            .await?;
        Self::decode(url, response).await
    }

    pub async fn delete<T: DeserializeOwned>(&self, url: &str) -> AppResult<T> {
        let response = self.send(Method::DELETE, url, None, Auth::Session).await?;
        Self::decode(url, response).await
    }

    /// Any method with the session cookie, returning the raw response.
    pub async fn request(&self, method: Method, url: &str, body: Option<&serde_json::Value>) -> AppResult<Response> {
        let bytes = body.map(serde_json::to_vec).transpose()?.map(Bytes::from);
        self.send(method, url, bytes, Auth::Session).await
    }

    /// Any method as an explicit account (`None` = the active session).
    pub async fn request_for(&self, method: Method, url: &str, body: Option<&serde_json::Value>, cookie: Option<&str>) -> AppResult<Response> {
        let bytes = body.map(serde_json::to_vec).transpose()?.map(Bytes::from);
        let auth = cookie.map_or(Auth::Session, Auth::Cookie);
        self.send(method, url, bytes, auth).await
    }

    /// POST with the session cookie; returns only the HTTP status.
    pub async fn post_raw<B: Serialize + ?Sized>(&self, url: &str, body: &B) -> AppResult<u16> {
        let bytes = Bytes::from(serde_json::to_vec(body)?);
        Ok(self
            .send(Method::POST, url, Some(bytes), Auth::Session)
            .await?
            .status()
            .as_u16())
    }

    /// GET with the session cookie, returning the raw response (status-aware callers).
    pub async fn get_raw(&self, url: &str) -> AppResult<Response> {
        self.send(Method::GET, url, None, Auth::Session).await
    }

    /// Like [`get`](Self::get) but authenticates with an explicit cookie
    /// (account validation, multi-account operations).
    pub async fn get_as(&self, url: &str, cookie: &str) -> AppResult<Response> {
        self.send(Method::GET, url, None, Auth::Cookie(cookie))
            .await
    }

    pub async fn post_as<B: Serialize + ?Sized>(
        &self,
        url: &str,
        body: &B,
        cookie: &str,
    ) -> AppResult<Response> {
        let bytes = Bytes::from(serde_json::to_vec(body)?);
        self.send(Method::POST, url, Some(bytes), Auth::Cookie(cookie))
            .await
    }

    pub async fn decode<T: DeserializeOwned>(url: &str, response: Response) -> AppResult<T> {
        let status = response.status();
        if !status.is_success() {
            let mut message = response.text().await.unwrap_or_default();
            message.truncate(ERROR_BODY_LIMIT);
            return Err(AppError::Api {
                status: status.as_u16(),
                url: url.to_owned(),
                message,
            });
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
    /// `fetchApiWithWebContext`: a call that carries the Roblox website
    /// context (Origin/Referer and, when known, the browser tracker cookie).
    pub async fn request_web<T: DeserializeOwned>(
        &self,
        method: Method,
        url: &str,
        body: Option<&serde_json::Value>,
        context: Option<(&str, &str)>,
    ) -> AppResult<T> {
        let bytes = body.map(serde_json::to_vec).transpose()?.map(Bytes::from);
        let auth = match context {
            Some((cookie, tracker)) => Auth::Web {
                cookie: Some(cookie),
                tracker: Some(tracker),
            },
            None => Auth::Web {
                cookie: None,
                tracker: None,
            },
        };
        let response = self.send(method, url, bytes, auth).await?;
        Self::decode(url, response).await
    }

    /// POST without a body as a specific account from the website origin
    /// (authentication tickets). Returns the raw response for its headers.
    pub async fn post_web_as(&self, url: &str, cookie: &str) -> AppResult<Response> {
        self.send(
            Method::POST,
            url,
            Some(Bytes::new()),
            Auth::Web {
                cookie: Some(cookie),
                tracker: None,
            },
        )
        .await
    }

    async fn send(
        &self,
        method: Method,
        url: &str,
        body: Option<Bytes>,
        auth: Auth<'_>,
    ) -> AppResult<Response> {
        let mut attempt = 0;
        let mut challenge_retries = 0;
        let mut challenge_data: Option<Challenge> = None;
        loop {
            attempt += 1;
            let session = || {
                self.cookie
                    .read()
                    .as_deref()
                    .map(|c| format!(".ROBLOSECURITY={c}"))
            };
            let cookie_header = match auth {
                Auth::Session => session(),
                Auth::Cookie(cookie) => Some(format!(".ROBLOSECURITY={cookie}")),
                Auth::Web {
                    cookie: Some(cookie),
                    tracker: Some(tracker),
                } => Some(format!(
                    ".ROBLOSECURITY={cookie}; RBXEventTrackerV2=browserid={}",
                    urlencoding::encode(tracker)
                )),
                Auth::Web {
                    cookie: Some(cookie),
                    tracker: None,
                } => Some(format!(".ROBLOSECURITY={cookie}")),
                Auth::Web { cookie: None, .. } => session(),
            };
            let csrf = if method == Method::GET {
                None
            } else {
                self.csrf.read().clone()
            };

            let mut request = self
                .http
                .request(method.clone(), url)
                .header("Accept-Language", self.accept_language.read().clone());
            if let Auth::Web { tracker, .. } = auth {
                request = request
                    .header("Origin", endpoints::WEB)
                    .header("Referer", WEB_REFERER);
                if let (Some(tracker), true) = (
                    tracker,
                    matches!(
                        auth,
                        Auth::Web {
                            cookie: Some(_),
                            ..
                        }
                    ),
                ) {
                    request = request.header("Roblox-Browser-Tracker", tracker);
                }
            }
            if let Some(header) = cookie_header {
                request = request.header(COOKIE, header);
            }
            if let Some(token) = csrf {
                request = request.header(CSRF_HEADER, token);
            }
            if let Some(c) = &challenge_data {
                request = request
                    .header(CHALLENGE_TYPE_HEADER, c.kind.as_str())
                    .header(CHALLENGE_ID_HEADER, c.id.as_str())
                    .header(CHALLENGE_METADATA_HEADER, c.metadata.as_str());
            }
            let sent_headers: Vec<(String, String)> = request
                .try_clone()
                .and_then(|r| r.build().ok())
                .map(|r| r.headers().iter().filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned()))).collect())
                .unwrap_or_default();
            if let Some(bytes) = &body {
                // `Bytes` clones are reference-counted: replays don't copy the body.
                request = request
                    .header(CONTENT_TYPE, "application/json")
                    .body(bytes.clone());
            }

            let response = request.send().await?;
            if response.headers().get(CSRF_HEADER).is_none() {
                if let Some(challenge) = parse_challenge(&response) {
                    let handler = self.challenge_handler.read().clone();
                    if let (Some(handler), true) = (handler, challenge_retries < MAX_CHALLENGE_RETRIES) {
                        let replay = ReplayRequest {
                            url: url.to_owned(),
                            method: method.as_str().to_owned(),
                            headers: sent_headers,
                            body: body.as_ref().map(|b| String::from_utf8_lossy(b).into_owned()),
                            replay_supported: body.as_ref().is_none_or(|b| std::str::from_utf8(b).is_ok()),
                        };
                        match handler(challenge, replay).await {
                            Some(ChallengeOutcome::Retry(data)) => {
                                challenge_retries += 1;
                                challenge_data = Some(data);
                                attempt -= 1;
                                continue;
                            }
                            Some(ChallengeOutcome::Respond(replayed)) => return Ok(replayed),
                            None => return Ok(response),
                        }
                    }
                }
            }
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

/// `parseChallengeHeaders`: all three headers or none.
fn parse_challenge(response: &Response) -> Option<Challenge> {
    let get = |name: &str| response.headers().get(name).and_then(|v| v.to_str().ok()).filter(|v| !v.is_empty()).map(str::to_owned);
    Some(Challenge { kind: get(CHALLENGE_TYPE_HEADER)?, id: get(CHALLENGE_ID_HEADER)?, metadata: get(CHALLENGE_METADATA_HEADER)? })
}
