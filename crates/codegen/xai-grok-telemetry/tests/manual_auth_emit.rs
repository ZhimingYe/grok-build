#![allow(clippy::disallowed_methods)] // test clients hit localhost mocks
// ZMY SELF-ADDED CODE START
//! Fork regression: `log_event(ManualAuth)` must not reach the product events endpoint.
// ZMY SELF-ADDED CODE END

// ZMY SELF-ADDED CODE START
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use xai_grok_telemetry::client;
use xai_grok_telemetry::config::{TelemetryConfig, TelemetryMode};
use xai_grok_telemetry::events::{AuthTokenKind, ManualAuth, ManualAuthReason, ManualAuthSurface};
// ZMY SELF-ADDED CODE END

// ZMY SELF-ADDED CODE START
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_manual_auth_does_not_post_to_events_endpoint() {
    let bodies: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = bodies.clone();
    let app = axum::Router::new().route(
        "/events",
        axum::routing::post(move |axum::Json(v): axum::Json<serde_json::Value>| {
            let captured = captured.clone();
            async move {
                captured.lock().unwrap().push(v);
                axum::http::StatusCode::OK
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/events", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    client::init(
        TelemetryConfig {
            events_url: Some(url),
            events_api_key: Some("test-key".into()),
            mixpanel_enabled: false,
            ..TelemetryConfig::default()
        },
        TelemetryMode::Enabled,
        Some("user-xyz".into()),
        None,
        None,
        None,
        "0.0.0-test".into(),
        None,
        reqwest::Client::new(),
    );
    assert!(!client::is_enabled());
    assert!(!client::is_session_metrics_enabled());

    xai_grok_telemetry::log_event(ManualAuth {
        reason: ManualAuthReason::RefreshTokenRejected,
        trigger: ManualAuthSurface::Turn,
        token_kind: AuthTokenKind::OidcSession,
        principal: Some("user-xyz".into()),
    });

    let deadline = Instant::now() + Duration::from_millis(250);
    while Instant::now() < deadline {
        assert!(
            bodies.lock().unwrap().is_empty(),
            "fork must not send ManualAuth product telemetry"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(bodies.lock().unwrap().is_empty());
    server.abort();
}
// ZMY SELF-ADDED CODE END
