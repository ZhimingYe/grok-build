use xai_grok_telemetry::sentry::{self, Config};

// ZMY SELF-ADDED CODE START
#[test]
fn fork_sentry_ignores_environment_dsn() {
    let sink = xai_grok_test_support::EnvelopeSink::start();
    unsafe { std::env::set_var("SENTRY_DSN", sink.dsn()) };
    let guard = sentry::init(Config {
        client: "grok-pager",
        client_version: "test",
        release: "test",
        disabled: false,
    });
    assert!(guard.is_none());

    ::sentry::capture_message(
        "conformance-crash-marker token=longvalue123",
        ::sentry::Level::Error,
    );
    sentry::flush_on_shutdown();
    assert!(
        sink.bodies().is_empty(),
        "fork must not send Sentry envelopes from SENTRY_DSN"
    );
}
// ZMY SELF-ADDED CODE END
