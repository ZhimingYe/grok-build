//! Embedding provider abstraction for memory vector search.
//!
//! The sqlite-vec `chunks_vec` virtual table is the embedding cache; there is no separate cache.

use async_trait::async_trait;

/// Maximum retry attempts for transient API errors (429, 5xx).
const MAX_RETRIES: usize = 3;
/// Initial backoff delay in milliseconds (doubles on each retry: 1s, 2s, 4s).
const INITIAL_BACKOFF_MS: u64 = 1000;
// ZMY SELF-ADDED CODE START
const EMBEDDING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
// ZMY SELF-ADDED CODE END

// ZMY SELF-ADDED CODE START
const EMBEDDING_FAILURE_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(60);
const MAX_EMBEDDING_CIRCUITS: usize = 256;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct EmbeddingCircuitKey(u64);

struct EmbeddingCircuitEntry {
    in_flight: bool,
    failure_until: Option<std::time::Instant>,
    last_touched: std::time::Instant,
}

static EMBEDDING_CIRCUITS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<EmbeddingCircuitKey, EmbeddingCircuitEntry>>,
> = std::sync::OnceLock::new();

fn embedding_circuit_key(api_base: &str, model: &str, dimensions: usize) -> EmbeddingCircuitKey {
    use std::hash::{Hash, Hasher};

    let endpoint = reqwest::Url::parse(api_base.trim())
        .map(|url| url.to_string().trim_end_matches('/').to_owned())
        .unwrap_or_else(|_| api_base.trim().trim_end_matches('/').to_owned());
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    endpoint.hash(&mut hasher);
    model.hash(&mut hasher);
    dimensions.hash(&mut hasher);
    EmbeddingCircuitKey(hasher.finish())
}

struct EmbeddingRequestGuard {
    key: EmbeddingCircuitKey,
    cooldown: std::time::Duration,
    completed: bool,
}

impl EmbeddingRequestGuard {
    fn begin(
        key: EmbeddingCircuitKey,
        cooldown: std::time::Duration,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let now = std::time::Instant::now();
        let circuits = EMBEDDING_CIRCUITS.get_or_init(Default::default);
        let mut circuits = circuits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        circuits.retain(|_, entry| {
            entry.in_flight || entry.failure_until.is_some_and(|until| until > now)
        });
        if let Some(entry) = circuits.get_mut(&key) {
            if entry.in_flight {
                return Err("embedding request already in flight".into());
            }
            if entry.failure_until.is_some_and(|until| until > now) {
                return Err("embedding temporarily unavailable after a recent failure".into());
            }
            entry.in_flight = true;
            entry.failure_until = None;
            entry.last_touched = now;
            return Ok(Self {
                key,
                cooldown,
                completed: false,
            });
        }
        if circuits.len() >= MAX_EMBEDDING_CIRCUITS {
            return Err("embedding circuit registry is busy".into());
        }
        circuits.insert(
            key,
            EmbeddingCircuitEntry {
                in_flight: true,
                failure_until: None,
                last_touched: now,
            },
        );
        Ok(Self {
            key,
            cooldown,
            completed: false,
        })
    }

    fn succeed(mut self) {
        self.finish(None);
        self.completed = true;
    }

    fn finish(&self, failure_until: Option<std::time::Instant>) {
        let circuits = EMBEDDING_CIRCUITS.get_or_init(Default::default);
        let mut circuits = circuits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = circuits.get_mut(&self.key) {
            entry.in_flight = false;
            entry.failure_until = failure_until;
            entry.last_touched = std::time::Instant::now();
        }
    }
}

impl Drop for EmbeddingRequestGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.finish(Some(std::time::Instant::now() + self.cooldown));
        }
    }
}
// ZMY SELF-ADDED CODE END

/// Generates text embeddings.
/// The `Send + Sync` bound lets implementations run inside `Send` futures such as `tokio::spawn`.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Embed a batch of texts, returning one vector per input text.
    async fn embed_batch(
        &self,
        texts: &[&str],
    ) -> Result<Vec<Vec<f32>>, Box<dyn std::error::Error>>;

    fn model_name(&self) -> &str;

    fn dimensions(&self) -> usize;
}

/// API-based embedding provider using an OpenAI-compatible embeddings endpoint.
pub struct ApiEmbeddingProvider {
    api_base: String,
    model: String,
    dimensions: usize,
    client: reqwest_middleware::ClientWithMiddleware,
    max_batch_size: usize,
}

impl ApiEmbeddingProvider {
    // ZMY SELF-ADDED CODE START
    /// Whether configuration permits embedding requests before resolving credentials.
    pub fn is_configured(
        config: &xai_grok_config_types::MemoryEmbeddingConfig,
        api_base: &str,
    ) -> bool {
        config.provider != "none"
            && config
                .model
                .as_deref()
                .is_some_and(|model| !model.trim().is_empty())
            && config.dimensions > 0
            && !xai_grok_env::is_official_service_url(api_base)
    }
    // ZMY SELF-ADDED CODE END
    pub fn new(
        api_base: String,
        model: String,
        dimensions: usize,
        client: reqwest_middleware::ClientWithMiddleware,
    ) -> Self {
        Self {
            api_base,
            model,
            dimensions,
            client,
            max_batch_size: 32,
        }
    }

    pub fn from_config(
        config: &xai_grok_config_types::MemoryEmbeddingConfig,
        api_base: String,
        client: reqwest_middleware::ClientWithMiddleware,
    ) -> Option<Self> {
        // ZMY SELF-ADDED CODE START
        if !Self::is_configured(config, &api_base) {
            return None;
        }
        // ZMY SELF-ADDED CODE END
        let model = config.model.clone().filter(|m| !m.is_empty())?;
        Some(Self::new(api_base, model, config.dimensions, client))
    }

    pub fn from_session(
        config: &xai_grok_config_types::MemoryEmbeddingConfig,
        proxy_base_url: String,
        auth_key: String,
    ) -> Option<Self> {
        let client = build_static_middleware_client(Some(auth_key));
        Self::from_config(config, proxy_base_url, client)
    }
}

pub(super) fn build_middleware_client(
    credentials: std::sync::Arc<dyn xai_grok_auth::AuthCredentialProvider>,
) -> reqwest_middleware::ClientWithMiddleware {
    xai_grok_http::with_auth_retry(xai_grok_http::shared_client(), credentials)
}

fn build_static_middleware_client(
    api_key: Option<String>,
) -> reqwest_middleware::ClientWithMiddleware {
    let provider: std::sync::Arc<dyn xai_grok_auth::AuthCredentialProvider> = std::sync::Arc::new(
        xai_grok_auth::StaticAuthCredentialProvider::new(Box::new(NoopHttpAuth), api_key),
    );
    build_middleware_client(provider)
}

struct NoopHttpAuth;

impl xai_grok_auth::HttpAuth for NoopHttpAuth {
    fn apply(&self, builder: reqwest::RequestBuilder, _base_url: &str) -> reqwest::RequestBuilder {
        builder
    }
}

#[async_trait]
impl EmbeddingProvider for ApiEmbeddingProvider {
    // ZMY SELF-ADDED CODE START
    #[tracing::instrument(name = "memory.embed_batch", skip_all, fields(batch_size = texts.len()))]
    async fn embed_batch(
        &self,
        texts: &[&str],
    ) -> Result<Vec<Vec<f32>>, Box<dyn std::error::Error>> {
        self.embed_batch_with_policy(texts, EMBEDDING_TIMEOUT, EMBEDDING_FAILURE_COOLDOWN)
            .await
    }
    fn model_name(&self) -> &str {
        &self.model
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }
}
// ZMY SELF-ADDED CODE END

// ZMY SELF-ADDED CODE START
impl ApiEmbeddingProvider {
    async fn embed_batch_with_policy(
        &self,
        texts: &[&str],
        timeout: std::time::Duration,
        cooldown: std::time::Duration,
    ) -> Result<Vec<Vec<f32>>, Box<dyn std::error::Error>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        if xai_grok_env::is_official_service_url(&self.api_base) {
            return Err(xai_grok_env::OFFICIAL_SERVICES_DISABLED.into());
        }

        tokio::time::timeout(timeout, async {
            let key = embedding_circuit_key(&self.api_base, &self.model, self.dimensions);
            let guard = EmbeddingRequestGuard::begin(key, cooldown)?;
            let mut all_embeddings = Vec::with_capacity(texts.len());

            for batch in texts.chunks(self.max_batch_size) {
                let body_json = serde_json::json!({
                    "model": self.model,
                    "input": batch,
                    "dimensions": self.dimensions,
                });
                let mut last_err = String::new();
                let mut batch_embeddings = None;
                for attempt in 0..MAX_RETRIES {
                    if attempt > 0 {
                        let delay = INITIAL_BACKOFF_MS * 2u64.pow(attempt as u32 - 1);
                        tracing::warn!(
                            attempt,
                            delay_ms = delay,
                            "retrying embedding API call after transient error"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    }
                    let request = xai_grok_http::shared_client()
                        .post(format!("{}/embeddings", self.api_base))
                        .json(&body_json)
                        .header("X-XAI-Token-Auth", "xai-grok-cli")
                        .header("x-grok-client-version", xai_grok_version::VERSION);
                    let req = request
                        .build()
                        .map_err(|error| format!("failed to build embedding request: {error}"))?;
                    let response = match self.client.execute(req).await {
                        Ok(response) => response,
                        Err(error) => {
                            last_err = format!("request failed: {error}");
                            continue;
                        }
                    };
                    let status = response.status();
                    if status.is_success() {
                        let body: serde_json::Value = response.json().await?;
                        let data = body
                            .get("data")
                            .and_then(serde_json::Value::as_array)
                            .ok_or("embedding response missing 'data' array")?;
                        if data.len() != batch.len() {
                            return Err(format!(
                                "embedding response count mismatch: expected {}, got {}",
                                batch.len(),
                                data.len()
                            )
                            .into());
                        }
                        let mut embeddings = Vec::with_capacity(data.len());
                        for item in data {
                            let values = item
                                .get("embedding")
                                .and_then(serde_json::Value::as_array)
                                .ok_or("embedding item missing 'embedding' array")?;
                            let embedding = values
                                .iter()
                                .map(|value| {
                                    value
                                        .as_f64()
                                        .map(|value| value as f32)
                                        .ok_or("embedding item contains a non-numeric value")
                                })
                                .collect::<Result<Vec<_>, _>>()?;
                            if embedding.len() != self.dimensions {
                                return Err(format!(
                                    "embedding dimension mismatch: expected {}, got {}",
                                    self.dimensions,
                                    embedding.len()
                                )
                                .into());
                            }
                            embeddings.push(embedding);
                        }
                        batch_embeddings = Some(embeddings);
                        break;
                    }
                    if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
                    {
                        last_err = format!(
                            "HTTP {status}: {}",
                            response.text().await.unwrap_or_default()
                        );
                        continue;
                    }
                    let body = response.text().await.unwrap_or_default();
                    return Err(format!("embedding API error {status}: {body}").into());
                }
                let Some(embeddings) = batch_embeddings else {
                    return Err(format!(
                        "embedding API failed after {MAX_RETRIES} attempts: {last_err}"
                    )
                    .into());
                };
                all_embeddings.extend(embeddings);
            }
            if all_embeddings.len() != texts.len() {
                return Err("embedding response count did not match input count".into());
            }
            guard.succeed();
            Ok(all_embeddings)
        })
        .await
        .map_err(|_| Box::<dyn std::error::Error>::from("embedding request timed out"))?
    }
}
// ZMY SELF-ADDED CODE END

/// A mock provider for tests: each vector is derived from the blake3 hash of the text, so results are deterministic.
#[cfg(any(test, feature = "test-support"))]
pub struct MockEmbeddingProvider {
    pub dimensions: usize,
}

#[cfg(any(test, feature = "test-support"))]
#[async_trait]
impl EmbeddingProvider for MockEmbeddingProvider {
    async fn embed_batch(
        &self,
        texts: &[&str],
    ) -> Result<Vec<Vec<f32>>, Box<dyn std::error::Error>> {
        Ok(texts
            .iter()
            .map(|text| {
                let hash = blake3::hash(text.as_bytes());
                let bytes = hash.as_bytes();
                bytes
                    .iter()
                    .cycle()
                    .take(self.dimensions)
                    .map(|&b| b as f32 / 255.0)
                    .collect()
            })
            .collect())
    }

    fn model_name(&self) -> &str {
        "mock-embedding"
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_embedding_deterministic() {
        let provider = MockEmbeddingProvider { dimensions: 4 };
        let r1 = provider.embed_batch(&["hello"]).await.unwrap();
        let r2 = provider.embed_batch(&["hello"]).await.unwrap();
        assert_eq!(r1, r2);
    }

    #[tokio::test]
    async fn test_mock_embedding_different_texts() {
        let provider = MockEmbeddingProvider { dimensions: 4 };
        let results = provider.embed_batch(&["hello", "world"]).await.unwrap();
        let [a, b] = results.as_slice() else {
            panic!("expected two embeddings: {results:?}");
        };
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn test_mock_embedding_empty_input() {
        let provider = MockEmbeddingProvider { dimensions: 4 };
        let results = provider.embed_batch(&[]).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_mock_embedding_correct_dimensions() {
        let provider = MockEmbeddingProvider { dimensions: 128 };
        let results = provider.embed_batch(&["test"]).await.unwrap();
        assert_eq!(results.first().map(Vec::len), Some(128));
    }

    // ZMY SELF-ADDED CODE START
    #[test]
    fn fork_official_embedding_config_is_unavailable() {
        let config = xai_grok_config_types::MemoryEmbeddingConfig {
            model: Some("text-embedding".into()),
            ..Default::default()
        };
        let client = reqwest_middleware::ClientBuilder::new(reqwest::Client::new()).build();
        assert!(
            ApiEmbeddingProvider::from_config(&config, "https://api.x.ai/v1".into(), client)
                .is_none()
        );
        assert!(
            ApiEmbeddingProvider::from_session(
                &config,
                "https://api.x.ai/v1".into(),
                "session-token".into()
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn fork_direct_official_embedding_provider_refuses_before_request() {
        let client = reqwest_middleware::ClientBuilder::new(reqwest::Client::new()).build();
        let provider = ApiEmbeddingProvider::new(
            "https://api.x.ai/v1".into(),
            "text-embedding".into(),
            2,
            client,
        );
        let error = provider
            .embed_batch(&["no official request"])
            .await
            .expect_err("official embedding must be refused locally");
        assert_eq!(error.to_string(), xai_grok_env::OFFICIAL_SERVICES_DISABLED);
    }

    #[tokio::test]
    async fn fork_custom_embedding_provider_returns_mocked_dimensions_and_data() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).unwrap();
            let body = br#"{"data":[{"embedding":[0.25,0.5]}]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
        });
        let client = reqwest_middleware::ClientBuilder::new(reqwest::Client::new()).build();
        let provider = ApiEmbeddingProvider::new(
            format!("http://{address}"),
            "local-embedding".into(),
            2,
            client,
        );
        let embeddings = provider
            .embed_batch(&["custom provider"])
            .await
            .expect("custom embedding endpoint should be reachable");
        server.join().unwrap();
        assert_eq!(provider.model_name(), "local-embedding");
        assert_eq!(provider.dimensions(), 2);
        assert_eq!(embeddings, vec![vec![0.25, 0.5]]);
    }
    // ZMY SELF-ADDED CODE END

    // ZMY SELF-ADDED CODE START
    #[tokio::test]
    async fn fork_embedding_timeout_covers_the_retry_budget() {
        use std::io::{Read, Write};
        use std::sync::mpsc;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (stop_tx, stop_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\n");
            let _ = stop_rx.recv_timeout(std::time::Duration::from_secs(12));
        });
        let client = reqwest_middleware::ClientBuilder::new(reqwest::Client::new()).build();
        let provider = ApiEmbeddingProvider::new(
            format!("http://{address}"),
            "stalled-embedding".into(),
            2,
            client,
        );
        let started = std::time::Instant::now();
        let error = provider.embed_batch(&["timeout"]).await.unwrap_err();
        let elapsed = started.elapsed();
        assert!(error.to_string().contains("timed out"));
        assert!(elapsed >= std::time::Duration::from_secs(9));
        assert!(elapsed < std::time::Duration::from_secs(12));
        let _ = stop_tx.send(());
        server.join().unwrap();
    }
    // ZMY SELF-ADDED CODE END

    // ZMY SELF-ADDED CODE START
    #[tokio::test]
    async fn fork_embedding_failure_circuit_is_shared_isolated_and_expires() {
        use std::io::{Read, Write};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let server_requests = requests.clone();
        let server = std::thread::spawn(move || {
            for request_number in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 4096];
                let _ = stream.read(&mut request);
                server_requests.fetch_add(1, Ordering::SeqCst);
                if request_number == 0 {
                    stream
                        .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .unwrap();
                } else {
                    let body = br#"{"data":[{"embedding":[0.25,0.5]}]}"#;
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .unwrap();
                    stream.write_all(body).unwrap();
                }
            }
        });
        let endpoint = format!("http://{address}/v1");
        let make_provider = |model: &str| {
            ApiEmbeddingProvider::new(
                endpoint.clone(),
                model.to_owned(),
                2,
                reqwest_middleware::ClientBuilder::new(reqwest::Client::new()).build(),
            )
        };
        let cooldown = std::time::Duration::from_millis(80);
        let failed = make_provider("same-model");
        assert!(
            failed
                .embed_batch_with_policy(&["fail"], std::time::Duration::from_secs(1), cooldown)
                .await
                .is_err()
        );

        let reconstructed = make_provider("same-model");
        let started = std::time::Instant::now();
        assert!(
            reconstructed
                .embed_batch_with_policy(
                    &["suppressed"],
                    std::time::Duration::from_secs(1),
                    cooldown
                )
                .await
                .is_err()
        );
        assert!(started.elapsed() < std::time::Duration::from_millis(50));
        assert_eq!(requests.load(Ordering::SeqCst), 1);

        let other_model = make_provider("other-model");
        assert!(
            other_model
                .embed_batch_with_policy(
                    &["independent"],
                    std::time::Duration::from_secs(1),
                    cooldown
                )
                .await
                .is_ok()
        );
        assert_ne!(
            embedding_circuit_key(&endpoint, "same-model", 2),
            embedding_circuit_key(&endpoint, "same-model", 3)
        );
        assert_ne!(
            embedding_circuit_key(&endpoint, "same-model", 2),
            embedding_circuit_key("http://127.0.0.1:1/v1", "same-model", 2)
        );
        tokio::time::sleep(cooldown + std::time::Duration::from_millis(20)).await;
        let recovered = make_provider("same-model");
        assert!(
            recovered
                .embed_batch_with_policy(
                    &["recovered"],
                    std::time::Duration::from_secs(1),
                    cooldown
                )
                .await
                .is_ok()
        );
        server.join().unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn fork_embedding_circuit_rejects_concurrent_same_key() {
        let key = embedding_circuit_key("http://127.0.0.1:0/v1", "concurrent", 4);
        let request = EmbeddingRequestGuard::begin(key, EMBEDDING_FAILURE_COOLDOWN).unwrap();
        assert!(EmbeddingRequestGuard::begin(key, EMBEDDING_FAILURE_COOLDOWN).is_err());
        drop(request);
        assert!(EmbeddingRequestGuard::begin(key, EMBEDDING_FAILURE_COOLDOWN).is_err());
    }
    // ZMY SELF-ADDED CODE END
}
