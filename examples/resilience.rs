//! Resilience patterns -- configure protection for unreliable backends.
//!
//! Demonstrates the full resilience stack: timeout, circuit breaker,
//! rate limit, retry, caching, and failover for a production deployment.
//!
//! Run with: `cargo run --example resilience`

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = mcp_proxy::ProxyConfig::parse(
        r#"
        [proxy]
        name = "resilient-proxy"
        [proxy.listen]

        # Primary backend with full resilience stack
        [[backends]]
        name = "api"
        transport = "http"
        url = "http://api:8080"

        [backends.timeout]
        seconds = 30

        [backends.circuit_breaker]
        failure_rate_threshold = 0.5
        minimum_calls = 10
        wait_duration_seconds = 60
        permitted_calls_in_half_open = 3

        [backends.rate_limit]
        requests = 100
        period_seconds = 1

        [backends.retry]
        max_retries = 3
        initial_backoff_ms = 100
        max_backoff_ms = 5000
        budget_percent = 20.0

        [backends.cache]
        tool_ttl_seconds = 30
        max_entries = 500

        # Failover backend -- takes over when primary fails
        [[backends]]
        name = "api-fallback"
        transport = "http"
        url = "http://api-fallback:8080"
        failover_for = "api"
        priority = 1

        [backends.timeout]
        seconds = 60
        "#,
    )?;

    println!("Resilience config:");
    for backend in &config.backends {
        println!("  Backend: {}", backend.name);
        if backend.timeout.is_some() {
            println!("    - timeout");
        }
        if backend.circuit_breaker.is_some() {
            println!("    - circuit breaker");
        }
        if backend.rate_limit.is_some() {
            println!("    - rate limit");
        }
        if backend.retry.is_some() {
            println!("    - retry");
        }
        if backend.cache.is_some() {
            println!("    - caching");
        }
        if backend.failover_for.is_some() {
            println!(
                "    - failover for: {}",
                backend.failover_for.as_deref().unwrap()
            );
        }
    }

    Ok(())
}
