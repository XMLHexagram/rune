//! The HTTP client: authentication, retries and concurrency limits.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use tokio::sync::Semaphore;

use crate::error::{is_client_not_registered, parse_errors, Error, Result};
use crate::tokens::TokenSource;

/// The production API host.
pub const DEFAULT_BASE_URL: &str = "https://api.music.apple.com";

const USER_TOKEN_HEADER: &str = "Music-User-Token";

/// How failed requests are retried (429, 5xx and transport errors).
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Total number of attempts, including the first one.
    pub max_attempts: u32,
    /// Delay before the first retry; doubles each time.
    pub base_delay: Duration,
    /// Upper bound of the exponential delay.
    pub max_delay: Duration,
    /// Upper bound applied to a server supplied `Retry-After`.
    pub max_retry_after: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            max_retry_after: Duration::from_secs(120),
        }
    }
}

impl RetryPolicy {
    /// Exponential delay with jitter before retry number `retry` (1-based).
    fn backoff(&self, retry: u32) -> Duration {
        let exp = self.base_delay.saturating_mul(
            1u32.checked_shl(retry.saturating_sub(1))
                .unwrap_or(u32::MAX),
        );
        let capped = exp.min(self.max_delay);
        // Jitter in [0.5, 1.0] of the capped delay.
        capped.mul_f64(0.5 + rand::random::<f64>() * 0.5)
    }
}

/// Which tokens a request carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Auth {
    /// Developer token only (catalog).
    Developer,
    /// Developer token and Music User Token (`/v1/me/*`).
    User,
}

struct Inner {
    base_url: String,
    http: reqwest::Client,
    tokens: Arc<dyn TokenSource>,
    configured_storefront: Option<String>,
    storefront: Mutex<StorefrontCache>,
    permits: Semaphore,
    retry: RetryPolicy,
}

/// The storefront in use. `epoch` counts [`AppleMusicClient::forget_storefront`] calls, so
/// a lookup that started before one is not kept.
struct StorefrontCache {
    epoch: u64,
    id: Option<String>,
}

/// A client for the Apple Music API. Cloning is cheap and shares all state.
#[derive(Clone)]
pub struct AppleMusicClient {
    inner: Arc<Inner>,
}

/// Builds an [`AppleMusicClient`].
pub struct ClientBuilder {
    tokens: Arc<dyn TokenSource>,
    base_url: String,
    storefront: Option<String>,
    max_concurrent: usize,
    retry: RetryPolicy,
    http: Option<reqwest::Client>,
}

impl ClientBuilder {
    /// Overrides the API host, for example to point at a test server.
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Sets the storefront (lowercase country code) used by catalog calls.
    /// When unset it is looked up with `/v1/me/storefront` on first use.
    pub fn storefront(mut self, storefront: impl Into<String>) -> Self {
        self.storefront = Some(storefront.into());
        self
    }

    /// Caps the number of requests in flight. The default is 4.
    pub fn max_concurrent_requests(mut self, n: usize) -> Self {
        self.max_concurrent = n.max(1);
        self
    }

    /// Replaces the retry policy.
    pub fn retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.retry = policy;
        self
    }

    /// Uses an existing `reqwest` client (proxy settings, timeouts, ...).
    pub fn http_client(mut self, client: reqwest::Client) -> Self {
        self.http = Some(client);
        self
    }

    /// Builds the client.
    pub fn build(self) -> AppleMusicClient {
        let http = self.http.unwrap_or_else(|| {
            reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("an HTTP client with only a timeout set always builds")
        });
        AppleMusicClient {
            inner: Arc::new(Inner {
                base_url: self.base_url.trim_end_matches('/').to_owned(),
                http,
                tokens: self.tokens,
                configured_storefront: self.storefront.clone(),
                storefront: Mutex::new(StorefrontCache {
                    epoch: 0,
                    id: self.storefront,
                }),
                permits: Semaphore::new(self.max_concurrent),
                retry: self.retry,
            }),
        }
    }
}

impl AppleMusicClient {
    /// Creates a client for the production API with default settings.
    pub fn new(tokens: Arc<dyn TokenSource>) -> Self {
        Self::builder(tokens).build()
    }

    /// Starts building a client.
    pub fn builder(tokens: Arc<dyn TokenSource>) -> ClientBuilder {
        ClientBuilder {
            tokens,
            base_url: DEFAULT_BASE_URL.to_owned(),
            storefront: None,
            max_concurrent: 4,
            retry: RetryPolicy::default(),
            http: None,
        }
    }

    /// The storefront in use, if known, and the epoch to hand back to [`Self::set_storefront`].
    pub(crate) fn cached_storefront(&self) -> (u64, Option<String>) {
        let cache = self
            .inner
            .storefront
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        (cache.epoch, cache.id.clone())
    }

    /// Keeps a looked-up storefront, unless the storefront was forgotten since `epoch`.
    pub(crate) fn set_storefront(&self, epoch: u64, storefront: String) {
        let mut cache = self
            .inner
            .storefront
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if cache.epoch == epoch {
            cache.id = Some(storefront);
        }
    }

    /// Drops the storefront looked up for the last user, for example after another
    /// user signed in, including a lookup still in flight. A storefront set on the
    /// builder stays.
    pub fn forget_storefront(&self) {
        let mut cache = self
            .inner
            .storefront
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        cache.epoch += 1;
        cache.id.clone_from(&self.inner.configured_storefront);
    }

    /// Sends a request and returns the success body.
    ///
    /// `target` is the path with its query string, appended to the base URL.
    pub(crate) async fn execute(
        &self,
        method: Method,
        target: &str,
        body: Option<&serde_json::Value>,
        auth: Auth,
    ) -> Result<Vec<u8>> {
        let inner = &*self.inner;
        let url = format!("{}{}", inner.base_url, target);
        let policy = &inner.retry;

        let mut failures = 0u32;
        let mut refresh_user = false;
        let mut refresh_developer = false;
        let mut user_refresh_spent = false;
        let mut developer_refresh_spent = false;

        loop {
            let developer = inner.tokens.developer_token(refresh_developer).await?;
            let user = if auth == Auth::User {
                Some(
                    inner
                        .tokens
                        .user_token(refresh_user)
                        .await?
                        .ok_or(Error::MissingUserToken)?,
                )
            } else {
                None
            };
            refresh_developer = false;
            refresh_user = false;

            let mut request = inner.http.request(method.clone(), &url).header(
                AUTHORIZATION,
                secret_header(&format!("Bearer {developer}"))?,
            );
            if let Some(user) = &user {
                request = request.header(USER_TOKEN_HEADER, secret_header(user)?);
            }
            if let Some(body) = body {
                request = request
                    .header(CONTENT_TYPE, "application/json")
                    .body(serde_json::to_vec(body)?);
            }

            let outcome = {
                let _permit = inner.permits.acquire().await.ok();
                match request.send().await {
                    Ok(response) => {
                        let status = response.status();
                        let headers = response.headers().clone();
                        response
                            .bytes()
                            .await
                            .map(|bytes| (status, headers, bytes.to_vec()))
                    }
                    Err(e) => Err(e),
                }
            };

            let (status, headers, bytes) = match outcome {
                Ok(parts) => parts,
                Err(e) => {
                    failures += 1;
                    // A POST may have been applied even though the answer was lost.
                    if method == Method::POST || failures >= policy.max_attempts {
                        return Err(Error::Http(e));
                    }
                    tokio::time::sleep(policy.backoff(failures)).await;
                    continue;
                }
            };

            if status.is_success() {
                return Ok(bytes);
            }

            match status {
                StatusCode::UNAUTHORIZED => {
                    if auth == Auth::User && !user_refresh_spent {
                        user_refresh_spent = true;
                        refresh_user = true;
                    } else if !developer_refresh_spent {
                        developer_refresh_spent = true;
                        refresh_developer = true;
                    } else {
                        return Err(Error::Unauthorized);
                    }
                }
                StatusCode::FORBIDDEN => {
                    // Apple answers 403 for an expired or invalid user token.
                    if auth == Auth::User && !user_refresh_spent {
                        user_refresh_spent = true;
                        refresh_user = true;
                    } else {
                        return Err(Error::Forbidden);
                    }
                }
                StatusCode::NOT_FOUND => {
                    let errors = parse_errors(&bytes);
                    if is_client_not_registered(&errors) {
                        return Err(Error::ClientNotRegistered);
                    }
                    let message = errors
                        .first()
                        .and_then(|e| e.detail.clone().or_else(|| e.title.clone()))
                        .unwrap_or_else(|| target.to_owned());
                    return Err(Error::NotFound(message));
                }
                s if s == StatusCode::TOO_MANY_REQUESTS || s.is_server_error() => {
                    let retry_after = parse_retry_after(&headers);
                    failures += 1;
                    // Only a 429 means a POST was not processed; a 5xx may have been applied.
                    let post_not_retryable =
                        method == Method::POST && s != StatusCode::TOO_MANY_REQUESTS;
                    if post_not_retryable || failures >= policy.max_attempts {
                        return Err(if s == StatusCode::TOO_MANY_REQUESTS {
                            Error::RateLimited { retry_after }
                        } else {
                            Error::Server {
                                status: s.as_u16(),
                                errors: parse_errors(&bytes),
                            }
                        });
                    }
                    let delay = match retry_after {
                        Some(d) => d.min(policy.max_retry_after),
                        None => policy.backoff(failures),
                    };
                    tokio::time::sleep(delay).await;
                }
                s => {
                    return Err(Error::Api {
                        status: s.as_u16(),
                        errors: parse_errors(&bytes),
                    });
                }
            }
        }
    }

    /// Sends a request and decodes the JSON body.
    pub(crate) async fn execute_json<T: DeserializeOwned>(
        &self,
        method: Method,
        target: &str,
        body: Option<&serde_json::Value>,
        auth: Auth,
    ) -> Result<T> {
        let bytes = self.execute(method, target, body, auth).await?;
        Ok(serde_json::from_slice(&bytes)?)
    }
}

/// A header value that HTTP tooling must not print.
fn secret_header(value: &str) -> Result<HeaderValue> {
    let mut header = HeaderValue::from_str(value)
        .map_err(|_| Error::Token("a token is not a valid header value".into()))?;
    header.set_sensitive(true);
    Ok(header)
}

fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let seconds: u64 = headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_headers_are_marked_sensitive() {
        assert!(secret_header("Bearer token").unwrap().is_sensitive());
        assert!(matches!(secret_header("bad\nvalue"), Err(Error::Token(_))));
    }
}
