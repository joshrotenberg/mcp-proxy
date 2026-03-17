You are helping the user configure resilience policies for their mcp-proxy backends. Guide them through choosing appropriate protection for each backend.

## Resilience Options

### Timeout
Prevents slow backends from blocking requests. Recommended for all backends.

```toml
[backends.timeout]
seconds = 30
```

### Circuit Breaker
Stops sending requests to failing backends. Recommended for external APIs.

```toml
[backends.circuit_breaker]
failure_rate_threshold = 0.5     # Trip after 50% failure rate
minimum_calls = 5                # Need at least 5 calls before evaluating
wait_duration_seconds = 30       # Wait 30s before trying again
permitted_calls_in_half_open = 3 # Allow 3 probe requests
```

### Rate Limit
Caps request volume. Recommended for expensive or metered APIs.

```toml
[backends.rate_limit]
requests = 100
period_seconds = 1
```

### Retry
Automatically retries failed requests. Good for transient errors.

```toml
[backends.retry]
max_retries = 3
initial_backoff_ms = 100
max_backoff_ms = 5000
budget_percent = 20.0    # Max 20% of requests can be retries
```

### Hedging
Sends parallel requests to reduce tail latency. For latency-sensitive backends.

```toml
[backends.hedging]
delay_ms = 200    # Wait 200ms before sending hedge
max_hedges = 1
```

### Outlier Detection
Passively tracks errors and ejects unhealthy backends.

```toml
[backends.outlier_detection]
consecutive_errors = 5
interval_seconds = 10
base_ejection_seconds = 30
```

### Response Caching
Cache responses to reduce backend load.

```toml
[backends.cache]
resource_ttl_seconds = 300
tool_ttl_seconds = 60
max_entries = 1000
```

## Recommendations by Backend Type

| Backend Type | Timeout | Circuit Breaker | Rate Limit | Retry | Cache |
|-------------|---------|-----------------|------------|-------|-------|
| Local stdio | 30s | No | No | No | Optional |
| Internal API | 30s | Yes | Optional | Yes | Optional |
| External API | 60s | Yes | Yes | Yes | Yes |
| Database | 10s | Yes | Yes | No | Yes |

## Questions to Ask

1. What kind of backend is this? (local tool, internal service, external API)
2. How reliable is the backend? (very stable, occasionally flaky, unreliable)
3. Is the backend metered/rate-limited? (API quotas)
4. Are responses cacheable? (idempotent reads vs stateful mutations)
