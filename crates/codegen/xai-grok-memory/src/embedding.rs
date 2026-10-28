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
struct EmbeddingCircuitKey([u8; 32]);

struct EmbeddingCircuitEntry {
    active_requests: usize,
    failure_until: Option<std::time::Instant>,
    next_generation: u64,
    last_completed_generation: u64,
    last_touched: std::time::Instant,
}

static EMBEDDING_CIRCUITS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<EmbeddingCircuitKey, EmbeddingCircuitEntry>>,
> = std::sync::OnceLock::new();
static NEXT_EMBEDDING_INSTANCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn credential_fingerprint(snapshot: &xai_grok_auth::CredentialSnapshot) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    for (label, value) in [
        ("token", snapshot.token.as_deref()),
        ("user", snapshot.user_id.as_deref()),
        ("team", snapshot.team_id.as_deref()),
        ("deployment", snapshot.deployment_id.as_deref()),
        ("api-key", snapshot.api_key_id.as_deref()),
        ("organization", snapshot.organization_id.as_deref()),
    ] {
        hasher.update(label.as_bytes());
        hasher.update(&[0]);
        if let Some(value) = value {
            hasher.update(&[1]);
            hasher.update(&(value.len() as u64).to_le_bytes());
            hasher.update(value.as_bytes());
        } else {
            hasher.update(&[0]);
        }
    }
    *hasher.finalize().as_bytes()
}

fn unknown_credential_fingerprint(instance_identity: u64) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"unknown-embedding-client");
    hasher.update(&instance_identity.to_le_bytes());
    *hasher.finalize().as_bytes()
}

fn embedding_circuit_key_with_credential(
    api_base: &str,
    model: &str,
    dimensions: usize,
    credential_fingerprint: [u8; 32],
) -> EmbeddingCircuitKey {
    let endpoint = reqwest::Url::parse(api_base.trim())
        .map(|url| url.to_string().trim_end_matches('/').to_owned())
        .unwrap_or_else(|_| api_base.trim().trim_end_matches('/').to_owned());
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(endpoint.len() as u64).to_le_bytes());
    hasher.update(endpoint.as_bytes());
    hasher.update(&(model.len() as u64).to_le_bytes());
    hasher.update(model.as_bytes());
    hasher.update(&dimensions.to_le_bytes());
    hasher.update(&credential_fingerprint);
    EmbeddingCircuitKey(*hasher.finalize().as_bytes())
}

struct EmbeddingRequestGuard {
    key: Option<EmbeddingCircuitKey>,
    generation: Option<u64>,
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
            entry.active_requests > 0 || entry.failure_until.is_some_and(|until| until > now)
        });
        if let Some(entry) = circuits.get_mut(&key) {
            if entry.failure_until.is_some_and(|until| until > now) {
                return Err("embedding temporarily unavailable after a recent failure".into());
            }
            entry.active_requests += 1;
            entry.next_generation = entry.next_generation.wrapping_add(1);
            let generation = entry.next_generation;
            entry.failure_until = None;
            entry.last_touched = now;
            return Ok(Self {
                key: Some(key),
                generation: Some(generation),
                cooldown,
                completed: false,
            });
        }
        if circuits.len() >= MAX_EMBEDDING_CIRCUITS {
            circuits.retain(|_, entry| entry.active_requests > 0 || entry.failure_until.is_some());
        }
        if circuits.len() >= MAX_EMBEDDING_CIRCUITS {
            // Do not turn bookkeeping pressure into a provider outage. This request is
            // still allowed, but it cannot contribute to bounded circuit state.
            return Ok(Self {
                key: None,
                generation: None,
                cooldown,
                completed: false,
            });
        }
        circuits.insert(
            key,
            EmbeddingCircuitEntry {
                active_requests: 1,
                failure_until: None,
                next_generation: 1,
                last_completed_generation: 0,
                last_touched: now,
            },
        );
        Ok(Self {
            key: Some(key),
            generation: Some(1),
            cooldown,
            completed: false,
        })
    }

    fn succeed(mut self) {
        self.finish(None);
        self.completed = true;
    }

    fn finish(&self, failure_until: Option<std::time::Instant>) {
        let Some(key) = self.key else {
            return;
        };
        let circuits = EMBEDDING_CIRCUITS.get_or_init(Default::default);
        let mut circuits = circuits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = circuits.get_mut(&key) {
            entry.active_requests = entry.active_requests.saturating_sub(1);
            if self
                .generation
                .is_some_and(|generation| generation >= entry.last_completed_generation)
            {
                entry.last_completed_generation = self.generation.unwrap_or_default();
                entry.failure_until = failure_until;
            }
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
    // ZMY SELF-ADDED CODE START
    credential_provider: Option<std::sync::Arc<dyn xai_grok_auth::AuthCredentialProvider>>,
    instance_identity: u64,
    // ZMY SELF-ADDED CODE END
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
            // ZMY SELF-ADDED CODE START
            credential_provider: None,
            instance_identity: NEXT_EMBEDDING_INSTANCE
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            // ZMY SELF-ADDED CODE END
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

    // ZMY SELF-ADDED CODE START
    pub fn from_config_with_credentials(
        config: &xai_grok_config_types::MemoryEmbeddingConfig,
        api_base: String,
        client: reqwest_middleware::ClientWithMiddleware,
        credentials: std::sync::Arc<dyn xai_grok_auth::AuthCredentialProvider>,
    ) -> Option<Self> {
        let mut provider = Self::from_config(config, api_base, client)?;
        provider.credential_provider = Some(credentials);
        Some(provider)
    }

    pub fn from_session(
        config: &xai_grok_config_types::MemoryEmbeddingConfig,
        proxy_base_url: String,
        auth_key: String,
    ) -> Option<Self> {
        let credentials = std::sync::Arc::new(xai_grok_auth::StaticAuthCredentialProvider::new(
            Box::new(NoopHttpAuth),
            Some(auth_key),
        ));
        let client = build_middleware_client(credentials.clone());
        Self::from_config_with_credentials(config, proxy_base_url, client, credentials)
    }
    // ZMY SELF-ADDED CODE END
}

pub(super) fn build_middleware_client(
    credentials: std::sync::Arc<dyn xai_grok_auth::AuthCredentialProvider>,
) -> reqwest_middleware::ClientWithMiddleware {
    xai_grok_http::with_auth_retry(xai_grok_http::shared_client(), credentials)
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
            let fingerprint = self
                .credential_provider
                .as_ref()
                .map(|provider| credential_fingerprint(&provider.snapshot()))
                .unwrap_or_else(|| unknown_credential_fingerprint(self.instance_identity));
            let key = embedding_circuit_key_with_credential(
                &self.api_base,
                &self.model,
                self.dimensions,
                fingerprint,
            );
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
            for request_number in 0..4 {
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
        let make_provider = |model: &str, api_key: &str| {
            ApiEmbeddingProvider::from_session(
                &xai_grok_config_types::MemoryEmbeddingConfig {
                    model: Some(model.to_owned()),
                    dimensions: 2,
                    ..Default::default()
                },
                endpoint.clone(),
                api_key.to_owned(),
            )
            .unwrap()
        };
        let cooldown = std::time::Duration::from_millis(80);
        let failed = make_provider("same-model", "invalid-key");
        assert!(
            failed
                .embed_batch_with_policy(&["fail"], std::time::Duration::from_secs(1), cooldown)
                .await
                .is_err()
        );

        let reconstructed = make_provider("same-model", "invalid-key");
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

        let rotated = make_provider("same-model", "valid-key");
        assert!(
            rotated
                .embed_batch_with_policy(&["rotated"], std::time::Duration::from_secs(1), cooldown)
                .await
                .is_ok()
        );

        let other_model = make_provider("other-model", "invalid-key");
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
            embedding_circuit_key_with_credential(&endpoint, "same-model", 2, [0; 32]),
            embedding_circuit_key_with_credential(&endpoint, "same-model", 3, [0; 32])
        );
        assert_ne!(
            embedding_circuit_key_with_credential(&endpoint, "same-model", 2, [0; 32]),
            embedding_circuit_key_with_credential(
                "http://127.0.0.1:1/v1",
                "same-model",
                2,
                [0; 32]
            )
        );
        tokio::time::sleep(cooldown + std::time::Duration::from_millis(20)).await;
        let recovered = make_provider("same-model", "invalid-key");
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
        assert_eq!(requests.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn fork_embedding_circuit_tracks_rotated_credential_snapshots() {
        use std::io::{Read, Write};
        use std::sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        };

        struct RotatingCredentials(Mutex<String>);

        impl xai_grok_auth::HttpAuth for RotatingCredentials {
            fn apply(
                &self,
                builder: reqwest::RequestBuilder,
                _base_url: &str,
            ) -> reqwest::RequestBuilder {
                let token = self.0.lock().unwrap().clone();
                builder.bearer_auth(token)
            }
        }

        #[async_trait::async_trait]
        impl xai_grok_auth::AuthCredentialProvider for RotatingCredentials {
            fn snapshot(&self) -> xai_grok_auth::CredentialSnapshot {
                xai_grok_auth::CredentialSnapshot {
                    token: Some(self.0.lock().unwrap().clone()),
                    ..Default::default()
                }
            }

            async fn refresh_after_unauthorized(&self) -> bool {
                false
            }
        }

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let server_requests = requests.clone();
        let server = std::thread::spawn(move || {
            for request_number in 0..2 {
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
        let credentials = Arc::new(RotatingCredentials(Mutex::new("old-token".into())));
        let credential_provider: Arc<dyn xai_grok_auth::AuthCredentialProvider> =
            credentials.clone();
        let client = build_middleware_client(credential_provider.clone());
        let provider = ApiEmbeddingProvider::from_config_with_credentials(
            &xai_grok_config_types::MemoryEmbeddingConfig {
                model: Some("rotating-model".into()),
                dimensions: 2,
                ..Default::default()
            },
            endpoint,
            client,
            credential_provider,
        )
        .unwrap();
        assert!(
            provider
                .embed_batch_with_policy(
                    &["initial token"],
                    std::time::Duration::from_secs(1),
                    EMBEDDING_FAILURE_COOLDOWN,
                )
                .await
                .is_err()
        );

        *credentials.0.lock().unwrap() = "new-token".into();
        assert!(
            provider
                .embed_batch_with_policy(
                    &["rotated token"],
                    std::time::Duration::from_secs(1),
                    EMBEDDING_FAILURE_COOLDOWN,
                )
                .await
                .is_ok()
        );
        server.join().unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn fork_embedding_circuit_allows_concurrent_healthy_requests() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut streams = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 4096];
                let _ = stream.read(&mut request);
                streams.push(stream);
            }
            for mut stream in streams {
                let body = br#"{"data":[{"embedding":[0.25,0.5,0.75,1.0]}]}"#;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(body).unwrap();
            }
        });
        let provider = ApiEmbeddingProvider::from_session(
            &xai_grok_config_types::MemoryEmbeddingConfig {
                model: Some("concurrent".into()),
                dimensions: 4,
                ..Default::default()
            },
            format!("http://{address}"),
            "concurrent-key".into(),
        )
        .unwrap();
        let (first, second) = tokio::join!(
            provider.embed_batch_with_policy(
                &["first"],
                std::time::Duration::from_secs(1),
                EMBEDDING_FAILURE_COOLDOWN,
            ),
            provider.embed_batch_with_policy(
                &["second"],
                std::time::Duration::from_secs(1),
                EMBEDDING_FAILURE_COOLDOWN,
            )
        );
        assert!(first.is_ok());
        assert!(second.is_ok());
        server.join().unwrap();

        let generation_key = embedding_circuit_key_with_credential(
            "http://127.0.0.1:0/generation",
            "model",
            4,
            [1; 32],
        );
        let older =
            EmbeddingRequestGuard::begin(generation_key, EMBEDDING_FAILURE_COOLDOWN).unwrap();
        let newer =
            EmbeddingRequestGuard::begin(generation_key, EMBEDDING_FAILURE_COOLDOWN).unwrap();
        newer.succeed();
        drop(older);
        EmbeddingRequestGuard::begin(generation_key, EMBEDDING_FAILURE_COOLDOWN)
            .expect("an older cancellation must not overwrite newer success")
            .succeed();
    }
    // ZMY SELF-ADDED CODE END
}
