//! A reused multiplexed connection and exact scoped indexes; no database-wide KEYS.
use std::sync::Arc;
use std::time::Duration;

// All five keys share one Redis Cluster hash tag. Transactions bound capacity
// across tool/resource hashes and expire the whole scope with its last entry.
const SCRIPT: &str = r#"
local time = redis.call('TIME')
local now = time[1] * 1000 + math.floor(time[2] / 1000)
local function remove(member)
  local data = KEYS[tonumber(string.sub(member,1,1))]
  redis.call('HDEL', data, string.sub(member,3))
  redis.call('ZREM', KEYS[3], member)
  redis.call('ZREM', KEYS[4], member)
end
for _, member in ipairs(redis.call('ZRANGEBYSCORE', KEYS[3], '-inf', now)) do remove(member) end
local kind = tonumber(ARGV[2])
local member = kind .. ':' .. ARGV[3]
local answer = false
if ARGV[1] == 'get' then answer = redis.call('HGET', KEYS[kind], ARGV[3])
elseif ARGV[1] == 'put' then
  redis.call('HSET', KEYS[kind], ARGV[3], ARGV[4])
  redis.call('ZADD', KEYS[3], now + tonumber(ARGV[5]), member)
  redis.call('ZADD', KEYS[4], redis.call('INCR', KEYS[5]), member)
  local excess = redis.call('HLEN',KEYS[1]) + redis.call('HLEN',KEYS[2]) - tonumber(ARGV[6])
  if excess > 0 then
    for _, old in ipairs(redis.call('ZRANGE', KEYS[4], 0, excess-1)) do remove(old) end
  end
elseif ARGV[1] == 'clear' then
  for _, field in ipairs(redis.call('HKEYS', KEYS[kind])) do remove(kind .. ':' .. field) end
elseif ARGV[1] == 'count' then answer = tostring(redis.call('HLEN',KEYS[kind])) end
local last = redis.call('ZREVRANGE', KEYS[3], 0, 0, 'WITHSCORES')
if #last == 0 then redis.call('DEL',unpack(KEYS))
else
  local remaining = math.max(1, tonumber(last[2]) - now)
  for _, key in ipairs(KEYS) do redis.call('PEXPIRE', key, remaining) end
end
return answer
"#;

#[derive(Clone)]
pub(crate) struct Store {
    client: Option<redis::Client>,
    connection: Arc<tokio::sync::Mutex<Option<redis::aio::MultiplexedConnection>>>,
    permits: Arc<tokio::sync::Semaphore>,
    scope: String,
    kind: u8,
    ttl: Duration,
    capacity: u64,
}
impl Store {
    pub fn new(url: &str, scope: String, kind: &'static str, ttl: Duration, capacity: u64) -> Self {
        Self {
            client: redis::Client::open(url).ok(),
            connection: Default::default(),
            permits: Arc::new(tokio::sync::Semaphore::new(16)),
            scope,
            kind: if kind == "tool" { 1 } else { 2 },
            ttl,
            capacity,
        }
    }
    async fn execute(&self, operation: &str, key: &str, value: &str) -> Option<String> {
        let _permit = self.permits.clone().try_acquire_owned().ok()?;
        tokio::time::timeout(Duration::from_millis(200), async {
            let mut cached = self.connection.lock().await;
            if cached.is_none() {
                let config = redis::AsyncConnectionConfig::new()
                    .set_connection_timeout(Some(Duration::from_millis(75)))
                    .set_response_timeout(Some(Duration::from_millis(75)));
                *cached = Some(
                    self.client
                        .as_ref()?
                        .get_multiplexed_async_connection_with_config(&config)
                        .await
                        .ok()?,
                );
            }
            let mut conn = cached.as_ref()?.clone();
            drop(cached);
            let script = redis::Script::new(SCRIPT);
            let mut invocation = script.prepare_invoke();
            for suffix in ["tool", "resource", "expiry", "order", "sequence"] {
                invocation.key(format!("mcp-cache-v2:{{{}}}:{suffix}", self.scope));
            }
            let result: redis::RedisResult<Option<String>> = invocation
                .arg(operation)
                .arg(self.kind)
                .arg(key)
                .arg(value)
                .arg(self.ttl.as_millis().min(i64::MAX as u128) as u64)
                .arg(self.capacity)
                .invoke_async(&mut conn)
                .await;
            match result {
                Ok(value) => value,
                Err(_) => {
                    self.connection.lock().await.take();
                    None
                }
            }
        })
        .await
        .ok()
        .flatten()
    }
    pub async fn get(&self, key: &str) -> Option<String> {
        self.execute("get", key, "").await
    }
    pub async fn insert(&self, key: String, value: String) {
        self.execute("put", &key, &value).await;
    }
    pub async fn clear(&self) {
        self.execute("clear", "", "").await;
    }
    pub async fn count(&self) -> u64 {
        self.execute("count", "", "")
            .await
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }
}
