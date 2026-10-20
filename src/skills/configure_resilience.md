You are helping the user configure resilience policies for their mcp-proxy backends. Guide them through choosing appropriate protection for each backend.

Each TOML block below is an independent configuration using defaults. Merge the
desired settings into your actual backend; do not concatenate repeated backend
blocks with the same name. The example HTTP peer must be supplied separately.

## Resilience Options

### Timeout
Prevents slow backends from blocking requests. Recommended for all backends.

```toml
[proxy]
name = "example"
[proxy.listen]
host = "127.0.0.1"
port = 8080

[[backends]]
name = "api"
transport = "http"
url = "http://127.0.0.1:9000/mcp"

[backends.timeout]
seconds = 30
```

### Circuit Breaker
Stops sending requests to failing backends. Recommended for external APIs.

```toml
[proxy]
name = "example"
[proxy.listen]
host = "127.0.0.1"
port = 8080

[[backends]]
name = "api"
transport = "http"
url = "http://127.0.0.1:9000/mcp"

[backends.circuit_breaker]
failure_rate_threshold = 0.5     # Trip after 50% failure rate
minimum_calls = 5                # Need at least 5 calls before evaluating
wait_duration_seconds = 30       # Wait 30s before trying again
permitted_calls_in_half_open = 3 # Allow 3 probe requests
```

### Rate Limit
Caps request volume. Recommended for expensive or metered APIs.

```toml
[proxy]
name = "example"
[proxy.listen]
host = "127.0.0.1"
port = 8080

[[backends]]
name = "api"
transport = "http"
url = "http://127.0.0.1:9000/mcp"

[backends.rate_limit]
requests = 100
period_seconds = 1
```

### Retry
Automatically retries failed requests. Good for transient errors.

```toml
[proxy]
name = "example"
[proxy.listen]
host = "127.0.0.1"
port = 8080

[[backends]]
name = "api"
transport = "http"
url = "http://127.0.0.1:9000/mcp"

[backends.retry]
max_retries = 3
initial_backoff_ms = 100
max_backoff_ms = 5000
budget_percent = 20.0    # Retry reservations up to 20% of original calls per window
min_retries_per_sec = 0  # No low-traffic allowance independent of request volume
```

The budget is shared across layer clones in non-overlapping ten-second windows.
It allows the larger of the volume allowance and `min_retries_per_sec * 10`
(default floor: 100 reservations per window). Credits reset rather than refill
continuously; boundary bursts are possible. Reservations are charged before
backoff, including if cancelled. This replaces the former fixed token bucket.
The first and every later delay obey `max_backoff_ms`. Configuration rejects
more than 1000 retries or over five minutes of planned backoff; configure the
backend timeout to bound execution and sleep together. Retrying tool calls can
duplicate remote side effects.

### Hedging
Duplicates execution to seek a successful alternative. Use only where duplicate
side effects are acceptable; tool annotations alone do not prove safety.

```toml
[proxy]
name = "example"
[proxy.listen]
host = "127.0.0.1"
port = 8080

[[backends]]
name = "api"
transport = "http"
url = "http://127.0.0.1:9000/mcp"

[backends.hedging]
delay_ms = 200    # Wait 200ms before sending hedge
max_hedges = 1
```

### Outlier Detection
Passively tracks errors and ejects unhealthy backends.

```toml
[proxy]
name = "example"
[proxy.listen]
host = "127.0.0.1"
port = 8080

[[backends]]
name = "api"
transport = "http"
url = "http://127.0.0.1:9000/mcp"

[backends.outlier_detection]
consecutive_errors = 5
interval_seconds = 10 # Compatibility setting; no background evaluation timer
base_ejection_seconds = 30
max_ejection_percent = 50
```

Quota admission and recovery are atomic across the detector's registered
outlier-enabled backends. Nonzero percentages allow at least one slot; configured
proxies use the largest backend percentage. Recovery occurs on the next call
after the monotonic deadline, and stale responses cannot change the new streak.
Clones and pending calls share one registration; final drop releases it. See
outlier rustdoc for quota shrink and low-level manual accounting semantics.

### Response Caching
Cache responses to reduce backend load.

```toml
[proxy]
name = "example"
[proxy.listen]
host = "127.0.0.1"
port = 8080

[[backends]]
name = "api"
transport = "http"
url = "http://127.0.0.1:9000/mcp"

[backends.cache]
resource_ttl_seconds = 300
tool_ttl_seconds = 60
max_entries = 1000
```

Caching and coalescing can suppress independent writes, while retries/hedges
can repeat them. Apply the following suggestions only after checking each
backend's tools and idempotency contract; they are not safe defaults for writes.

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
