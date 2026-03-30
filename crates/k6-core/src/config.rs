use std::collections::HashMap;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::Value;

/// Top-level test configuration, parsed from k6 script `export const options`.
#[derive(Debug, Clone)]
pub struct TestConfig {
    pub vus: u32,
    pub duration: Duration,
    pub scenarios: HashMap<String, ScenarioConfig>,
    pub discard_response_bodies: bool,
    pub max_redirects: Option<u32>,
    pub user_agent: Option<String>,
    pub no_connection_reuse: bool,
    pub no_vu_connection_reuse: bool,
    pub insecure_skip_tls_verify: bool,
    pub thresholds: HashMap<String, Vec<String>>,
    /// TLS minimum version: "tls1.0", "tls1.1", "tls1.2", "tls1.3"
    pub tls_version: Option<TlsVersionConfig>,
    /// DNS configuration
    pub dns: Option<DnsConfig>,
    /// Block requests to these IP ranges (CIDR notation)
    pub blacklist_ips: Vec<String>,
    /// Block requests to these hostnames (supports * wildcards)
    pub block_hostnames: Vec<String>,
    /// Static hostname → IP mappings
    pub hosts: HashMap<String, String>,
    /// Log full HTTP request/response: "", "full"
    pub http_debug: Option<String>,
    /// Treat HTTP errors as exceptions
    pub throw: bool,
    /// Redirect console output to file path
    pub console_output: Option<String>,
    /// Source IP addresses for outgoing requests (round-robin pool)
    pub local_ips: Vec<String>,
    /// Global rate limit (requests per second). 0 = unlimited.
    pub rps: u32,
}

/// TLS version configuration.
#[derive(Debug, Clone)]
pub struct TlsVersionConfig {
    pub min: Option<String>,
    pub max: Option<String>,
}

/// DNS resolver configuration.
#[derive(Debug, Clone)]
pub struct DnsConfig {
    /// TTL for DNS cache: "inf", "0", or duration like "5m"
    pub ttl: Option<String>,
    /// How to select from multiple IPs: "first", "random", "roundRobin"
    pub select: Option<String>,
    /// IP version policy: "preferIPv4", "preferIPv6", "onlyIPv4", "onlyIPv6", "any"
    pub policy: Option<String>,
}

impl Default for TestConfig {
    fn default() -> Self {
        Self {
            vus: 1,
            duration: Duration::from_secs(10),
            scenarios: HashMap::new(),
            discard_response_bodies: false,
            max_redirects: None,
            user_agent: None,
            no_connection_reuse: false,
            no_vu_connection_reuse: false,
            insecure_skip_tls_verify: false,
            thresholds: HashMap::new(),
            tls_version: None,
            dns: None,
            blacklist_ips: Vec::new(),
            block_hostnames: Vec::new(),
            hosts: HashMap::new(),
            http_debug: None,
            throw: false,
            console_output: None,
            local_ips: Vec::new(),
            rps: 0,
        }
    }
}

/// Configuration for a single scenario/executor.
#[derive(Debug, Clone)]
pub struct ScenarioConfig {
    pub executor: ExecutorType,
    pub exec: Option<String>,
    pub start_time: Duration,
    pub graceful_stop: Duration,
    pub env: HashMap<String, String>,
    pub tags: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExecutorType {
    ConstantVus {
        vus: u32,
        duration: Duration,
    },
    RampingVus {
        start_vus: u32,
        stages: Vec<Stage>,
        graceful_ramp_down: Duration,
    },
    ConstantArrivalRate {
        rate: u32,
        time_unit: Duration,
        duration: Duration,
        pre_allocated_vus: u32,
        max_vus: Option<u32>,
    },
    RampingArrivalRate {
        start_rate: u32,
        stages: Vec<Stage>,
        time_unit: Duration,
        pre_allocated_vus: u32,
        max_vus: Option<u32>,
    },
    PerVuIterations {
        vus: u32,
        iterations: u32,
        max_duration: Duration,
    },
    SharedIterations {
        vus: u32,
        iterations: u32,
        max_duration: Duration,
    },
    ExternallyControlled {
        vus: u32,
        max_vus: u32,
        duration: Duration,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stage {
    pub duration: Duration,
    pub target: u32,
}

/// Parse a k6 duration string like "30s", "5m", "1h30m", "100ms".
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty duration string");
    }

    let mut total_ms: u64 = 0;
    let mut num_start = 0;

    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }

        let num: u64 = s[num_start..i]
            .parse()
            .with_context(|| format!("invalid number in duration: {s}"))?;

        // Collect unit suffix
        let unit_start = i;
        while i < chars.len() && chars[i].is_ascii_alphabetic() {
            i += 1;
        }
        let unit = &s[unit_start..i];

        total_ms += match unit {
            "ms" => num,
            "s" => num * 1000,
            "m" => num * 60 * 1000,
            "h" => num * 3600 * 1000,
            _ => bail!("unknown duration unit: {unit}"),
        };

        num_start = i;
    }

    // Handle bare number (treat as milliseconds if very large, seconds otherwise)
    if num_start == 0 && i == chars.len() && chars.iter().all(|c| c.is_ascii_digit()) {
        let num: u64 = s.parse()?;
        // k6 treats bare numbers as milliseconds in some contexts
        return Ok(Duration::from_millis(num));
    }

    if total_ms == 0 && !s.chars().all(|c| c == '0' || !c.is_ascii_digit()) {
        bail!("could not parse duration: {s}");
    }

    Ok(Duration::from_millis(total_ms))
}

/// Parse a `TestConfig` from a k6 options JSON object.
pub fn parse_options(options: &Value) -> Result<TestConfig> {
    let obj = options
        .as_object()
        .context("options must be a JSON object")?;

    let mut config = TestConfig::default();

    if let Some(v) = obj.get("vus") {
        config.vus = v.as_u64().context("vus must be a number")? as u32;
    }

    if let Some(v) = obj.get("duration") {
        let s = v.as_str().context("duration must be a string")?;
        config.duration = parse_duration(s)?;
    }

    if let Some(v) = obj.get("discardResponseBodies") {
        config.discard_response_bodies = v.as_bool().context("discardResponseBodies must be bool")?;
    }

    if let Some(v) = obj.get("maxRedirects") {
        config.max_redirects = Some(v.as_u64().context("maxRedirects must be a number")? as u32);
    }

    if let Some(v) = obj.get("userAgent") {
        config.user_agent = Some(v.as_str().context("userAgent must be a string")?.to_string());
    }

    if let Some(v) = obj.get("noConnectionReuse") {
        config.no_connection_reuse = v.as_bool().context("noConnectionReuse must be bool")?;
    }

    if let Some(v) = obj.get("noVUConnectionReuse") {
        config.no_vu_connection_reuse = v.as_bool().context("noVUConnectionReuse must be bool")?;
    }

    if let Some(v) = obj.get("insecureSkipTLSVerify") {
        config.insecure_skip_tls_verify =
            v.as_bool().context("insecureSkipTLSVerify must be bool")?;
    }

    if let Some(v) = obj.get("tlsVersion") {
        config.tls_version = Some(parse_tls_version(v)?);
    }

    if let Some(v) = obj.get("dns") {
        config.dns = Some(parse_dns_config(v)?);
    }

    if let Some(v) = obj.get("blacklistIPs") {
        config.blacklist_ips = parse_string_array(v)?;
    }

    if let Some(v) = obj.get("blockHostnames") {
        config.block_hostnames = parse_string_array(v)?;
    }

    if let Some(v) = obj.get("hosts") {
        config.hosts = parse_string_map(Some(v));
    }

    if let Some(v) = obj.get("httpDebug") {
        config.http_debug = Some(v.as_str().unwrap_or("full").to_string());
    }

    if let Some(v) = obj.get("throw") {
        config.throw = v.as_bool().context("throw must be bool")?;
    }

    if let Some(v) = obj.get("consoleOutput") {
        config.console_output = Some(
            v.as_str()
                .context("consoleOutput must be a string")?
                .to_string(),
        );
    }

    if let Some(v) = obj.get("localIPs") {
        config.local_ips = parse_string_array(v)?;
    }

    if let Some(v) = obj.get("rps") {
        config.rps = v.as_u64().context("rps must be a number")? as u32;
    }

    if let Some(v) = obj.get("thresholds") {
        config.thresholds = parse_thresholds(v)?;
    }

    if let Some(scenarios) = obj.get("scenarios") {
        config.scenarios = parse_scenarios(scenarios)?;
    }

    // If no scenarios defined but vus+duration are set, create a default scenario
    if config.scenarios.is_empty() && config.vus > 0 {
        config.scenarios.insert(
            "default".to_string(),
            ScenarioConfig {
                executor: ExecutorType::ConstantVus {
                    vus: config.vus,
                    duration: config.duration,
                },
                exec: None,
                start_time: Duration::ZERO,
                graceful_stop: Duration::from_secs(30),
                env: HashMap::new(),
                tags: HashMap::new(),
            },
        );
    }

    Ok(config)
}

fn parse_thresholds(value: &Value) -> Result<HashMap<String, Vec<String>>> {
    let obj = value
        .as_object()
        .context("thresholds must be an object")?;

    let mut thresholds = HashMap::new();
    for (name, val) in obj {
        let conditions = match val {
            Value::Array(arr) => arr
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(|s| s.to_string())
                        .context("threshold value must be a string")
                })
                .collect::<Result<Vec<_>>>()?,
            Value::String(s) => vec![s.clone()],
            _ => bail!("threshold for {name} must be a string or array of strings"),
        };
        thresholds.insert(name.clone(), conditions);
    }
    Ok(thresholds)
}

fn parse_scenarios(value: &Value) -> Result<HashMap<String, ScenarioConfig>> {
    let obj = value
        .as_object()
        .context("scenarios must be an object")?;

    let mut scenarios = HashMap::new();
    for (name, val) in obj {
        let scenario =
            parse_scenario(val).with_context(|| format!("in scenario '{name}'"))?;
        scenarios.insert(name.clone(), scenario);
    }
    Ok(scenarios)
}

fn parse_scenario(val: &Value) -> Result<ScenarioConfig> {
    let obj = val.as_object().context("scenario must be an object")?;

    let executor_type = obj
        .get("executor")
        .and_then(|v| v.as_str())
        .context("scenario must have an 'executor' string")?;

    let executor = match executor_type {
        "constant-vus" => ExecutorType::ConstantVus {
            vus: get_u32(obj, "vus").unwrap_or(1),
            duration: get_duration(obj, "duration")?,
        },
        "ramping-vus" => ExecutorType::RampingVus {
            start_vus: get_u32(obj, "startVUs").unwrap_or(1),
            stages: parse_stages(obj.get("stages").context("ramping-vus requires 'stages'")?)?,
            graceful_ramp_down: get_duration_or(obj, "gracefulRampDown", Duration::from_secs(30)),
        },
        "constant-arrival-rate" => ExecutorType::ConstantArrivalRate {
            rate: get_u32(obj, "rate").context("constant-arrival-rate requires 'rate'")?,
            time_unit: get_duration_or(obj, "timeUnit", Duration::from_secs(1)),
            duration: get_duration(obj, "duration")?,
            pre_allocated_vus: get_u32(obj, "preAllocatedVUs")
                .context("constant-arrival-rate requires 'preAllocatedVUs'")?,
            max_vus: get_u32(obj, "maxVUs").ok(),
        },
        "ramping-arrival-rate" => ExecutorType::RampingArrivalRate {
            start_rate: get_u32(obj, "startRate").unwrap_or(0),
            stages: parse_stages(
                obj.get("stages")
                    .context("ramping-arrival-rate requires 'stages'")?,
            )?,
            time_unit: get_duration_or(obj, "timeUnit", Duration::from_secs(1)),
            pre_allocated_vus: get_u32(obj, "preAllocatedVUs")
                .context("ramping-arrival-rate requires 'preAllocatedVUs'")?,
            max_vus: get_u32(obj, "maxVUs").ok(),
        },
        "per-vu-iterations" => ExecutorType::PerVuIterations {
            vus: get_u32(obj, "vus").unwrap_or(1),
            iterations: get_u32(obj, "iterations").unwrap_or(1),
            max_duration: get_duration_or(obj, "maxDuration", Duration::from_secs(600)),
        },
        "shared-iterations" => ExecutorType::SharedIterations {
            vus: get_u32(obj, "vus").unwrap_or(1),
            iterations: get_u32(obj, "iterations").unwrap_or(1),
            max_duration: get_duration_or(obj, "maxDuration", Duration::from_secs(600)),
        },
        "externally-controlled" => ExecutorType::ExternallyControlled {
            vus: get_u32(obj, "vus").unwrap_or(1),
            max_vus: get_u32(obj, "maxVUs").unwrap_or(10),
            duration: get_duration_or(obj, "duration", Duration::from_secs(0)),
        },
        other => bail!("unknown executor type: {other}"),
    };

    let exec = obj.get("exec").and_then(|v| v.as_str()).map(String::from);

    let start_time = get_duration_or(obj, "startTime", Duration::ZERO);
    let graceful_stop = get_duration_or(obj, "gracefulStop", Duration::from_secs(30));

    let env = parse_string_map(obj.get("env"));
    let tags = parse_string_map(obj.get("tags"));

    Ok(ScenarioConfig {
        executor,
        exec,
        start_time,
        graceful_stop,
        env,
        tags,
    })
}

fn parse_stages(value: &Value) -> Result<Vec<Stage>> {
    let arr = value.as_array().context("stages must be an array")?;
    arr.iter()
        .map(|v| {
            let obj = v.as_object().context("stage must be an object")?;
            Ok(Stage {
                duration: get_duration(obj, "duration")?,
                target: get_u32(obj, "target").context("stage requires 'target'")?,
            })
        })
        .collect()
}

fn get_u32(obj: &serde_json::Map<String, Value>, key: &str) -> Result<u32> {
    obj.get(key)
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .context(format!("missing or invalid '{key}'"))
}

fn get_duration(obj: &serde_json::Map<String, Value>, key: &str) -> Result<Duration> {
    let s = obj
        .get(key)
        .and_then(|v| v.as_str())
        .context(format!("missing or invalid '{key}'"))?;
    parse_duration(s)
}

fn get_duration_or(
    obj: &serde_json::Map<String, Value>,
    key: &str,
    default: Duration,
) -> Duration {
    obj.get(key)
        .and_then(|v| v.as_str())
        .and_then(|s| parse_duration(s).ok())
        .unwrap_or(default)
}

fn parse_tls_version(value: &Value) -> Result<TlsVersionConfig> {
    match value {
        Value::String(s) => Ok(TlsVersionConfig {
            min: Some(s.clone()),
            max: Some(s.clone()),
        }),
        Value::Object(obj) => Ok(TlsVersionConfig {
            min: obj
                .get("min")
                .and_then(|v| v.as_str())
                .map(String::from),
            max: obj
                .get("max")
                .and_then(|v| v.as_str())
                .map(String::from),
        }),
        _ => bail!("tlsVersion must be a string or object with min/max"),
    }
}

fn parse_dns_config(value: &Value) -> Result<DnsConfig> {
    let obj = value.as_object().context("dns must be an object")?;
    Ok(DnsConfig {
        ttl: obj.get("ttl").and_then(|v| v.as_str()).map(String::from),
        select: obj
            .get("select")
            .and_then(|v| v.as_str())
            .map(String::from),
        policy: obj
            .get("policy")
            .and_then(|v| v.as_str())
            .map(String::from),
    })
}

fn parse_string_array(value: &Value) -> Result<Vec<String>> {
    let arr = value.as_array().context("expected an array")?;
    arr.iter()
        .map(|v| {
            v.as_str()
                .map(String::from)
                .context("array elements must be strings")
        })
        .collect()
}

fn parse_string_map(value: Option<&Value>) -> HashMap<String, String> {
    value
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_duration_seconds() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
    }

    #[test]
    fn parse_duration_minutes() {
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
    }

    #[test]
    fn parse_duration_compound() {
        assert_eq!(
            parse_duration("1h30m").unwrap(),
            Duration::from_secs(5400)
        );
    }

    #[test]
    fn parse_duration_milliseconds() {
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
    }

    #[test]
    fn parse_duration_empty_fails() {
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn parse_duration_unknown_unit_fails() {
        assert!(parse_duration("5d").is_err());
    }

    #[test]
    fn parse_simple_options() {
        let opts = json!({
            "vus": 10,
            "duration": "30s"
        });

        let config = parse_options(&opts).unwrap();
        assert_eq!(config.vus, 10);
        assert_eq!(config.duration, Duration::from_secs(30));
        // Should create a default constant-vus scenario
        assert_eq!(config.scenarios.len(), 1);
        let default = &config.scenarios["default"];
        assert_eq!(
            default.executor,
            ExecutorType::ConstantVus {
                vus: 10,
                duration: Duration::from_secs(30),
            }
        );
    }

    #[test]
    fn parse_with_thresholds() {
        let opts = json!({
            "vus": 1,
            "duration": "10s",
            "thresholds": {
                "http_req_duration": ["p(95)<2000"],
                "http_req_failed": ["rate<0.01"]
            }
        });

        let config = parse_options(&opts).unwrap();
        assert_eq!(config.thresholds.len(), 2);
        assert_eq!(
            config.thresholds["http_req_duration"],
            vec!["p(95)<2000"]
        );
    }

    #[test]
    fn parse_constant_arrival_rate_scenario() {
        let opts = json!({
            "scenarios": {
                "load": {
                    "executor": "constant-arrival-rate",
                    "rate": 100,
                    "timeUnit": "1s",
                    "duration": "8h",
                    "preAllocatedVUs": 50,
                    "maxVUs": 500
                }
            }
        });

        let config = parse_options(&opts).unwrap();
        let load = &config.scenarios["load"];
        assert_eq!(
            load.executor,
            ExecutorType::ConstantArrivalRate {
                rate: 100,
                time_unit: Duration::from_secs(1),
                duration: Duration::from_secs(8 * 3600),
                pre_allocated_vus: 50,
                max_vus: Some(500),
            }
        );
    }

    #[test]
    fn parse_ramping_vus_scenario() {
        let opts = json!({
            "scenarios": {
                "ramp": {
                    "executor": "ramping-vus",
                    "startVUs": 0,
                    "stages": [
                        { "duration": "1m", "target": 10 },
                        { "duration": "3m", "target": 10 },
                        { "duration": "1m", "target": 0 }
                    ]
                }
            }
        });

        let config = parse_options(&opts).unwrap();
        let ramp = &config.scenarios["ramp"];
        match &ramp.executor {
            ExecutorType::RampingVus {
                start_vus, stages, ..
            } => {
                assert_eq!(*start_vus, 0);
                assert_eq!(stages.len(), 3);
                assert_eq!(stages[0].target, 10);
                assert_eq!(stages[0].duration, Duration::from_secs(60));
            }
            _ => panic!("expected RampingVus"),
        }
    }

    #[test]
    fn parse_multiple_scenarios() {
        let opts = json!({
            "scenarios": {
                "light": {
                    "executor": "ramping-vus",
                    "exec": "lightUserScenario",
                    "startVUs": 0,
                    "stages": [
                        { "duration": "1m", "target": 7 }
                    ]
                },
                "heavy": {
                    "executor": "constant-vus",
                    "vus": 5,
                    "duration": "5m",
                    "env": { "MODE": "heavy" },
                    "tags": { "type": "power" }
                }
            }
        });

        let config = parse_options(&opts).unwrap();
        assert_eq!(config.scenarios.len(), 2);

        let light = &config.scenarios["light"];
        assert_eq!(light.exec, Some("lightUserScenario".to_string()));

        let heavy = &config.scenarios["heavy"];
        assert_eq!(heavy.env.get("MODE").unwrap(), "heavy");
        assert_eq!(heavy.tags.get("type").unwrap(), "power");
    }

    #[test]
    fn parse_discard_response_bodies() {
        let opts = json!({
            "vus": 1,
            "duration": "10s",
            "discardResponseBodies": true
        });

        let config = parse_options(&opts).unwrap();
        assert!(config.discard_response_bodies);
    }

    #[test]
    fn defaults_when_minimal() {
        let opts = json!({});

        let config = parse_options(&opts).unwrap();
        assert_eq!(config.vus, 1);
        assert_eq!(config.duration, Duration::from_secs(10));
        assert!(!config.discard_response_bodies);
    }

    #[test]
    fn unknown_executor_fails() {
        let opts = json!({
            "scenarios": {
                "bad": {
                    "executor": "turbo-mode",
                    "duration": "10s"
                }
            }
        });

        assert!(parse_options(&opts).is_err());
    }

    #[test]
    fn missing_required_field_fails() {
        let opts = json!({
            "scenarios": {
                "bad": {
                    "executor": "constant-arrival-rate",
                    "duration": "10s"
                    // missing rate and preAllocatedVUs
                }
            }
        });

        assert!(parse_options(&opts).is_err());
    }

    #[test]
    fn parse_networking_options() {
        let opts = json!({
            "vus": 1,
            "duration": "10s",
            "noConnectionReuse": true,
            "noVUConnectionReuse": true,
            "insecureSkipTLSVerify": true,
            "throw": true,
            "httpDebug": "full",
            "blacklistIPs": ["10.0.0.0/8", "192.168.0.0/16"],
            "blockHostnames": ["*.internal.com", "secret.example.com"],
            "hosts": { "test.local": "127.0.0.1" }
        });

        let config = parse_options(&opts).unwrap();
        assert!(config.no_connection_reuse);
        assert!(config.no_vu_connection_reuse);
        assert!(config.insecure_skip_tls_verify);
        assert!(config.throw);
        assert_eq!(config.http_debug, Some("full".to_string()));
        assert_eq!(config.blacklist_ips.len(), 2);
        assert_eq!(config.block_hostnames.len(), 2);
        assert_eq!(config.hosts.get("test.local").unwrap(), "127.0.0.1");
    }

    #[test]
    fn parse_tls_version_string() {
        let opts = json!({
            "vus": 1,
            "duration": "10s",
            "tlsVersion": "tls1.2"
        });

        let config = parse_options(&opts).unwrap();
        let tls = config.tls_version.unwrap();
        assert_eq!(tls.min, Some("tls1.2".to_string()));
        assert_eq!(tls.max, Some("tls1.2".to_string()));
    }

    #[test]
    fn parse_tls_version_object() {
        let opts = json!({
            "vus": 1,
            "duration": "10s",
            "tlsVersion": { "min": "tls1.2", "max": "tls1.3" }
        });

        let config = parse_options(&opts).unwrap();
        let tls = config.tls_version.unwrap();
        assert_eq!(tls.min, Some("tls1.2".to_string()));
        assert_eq!(tls.max, Some("tls1.3".to_string()));
    }

    #[test]
    fn parse_dns_config() {
        let opts = json!({
            "vus": 1,
            "duration": "10s",
            "dns": {
                "ttl": "5m",
                "select": "random",
                "policy": "preferIPv4"
            }
        });

        let config = parse_options(&opts).unwrap();
        let dns = config.dns.unwrap();
        assert_eq!(dns.ttl, Some("5m".to_string()));
        assert_eq!(dns.select, Some("random".to_string()));
        assert_eq!(dns.policy, Some("preferIPv4".to_string()));
    }

    #[test]
    fn parse_console_output() {
        let opts = json!({
            "vus": 1,
            "duration": "10s",
            "consoleOutput": "/tmp/k6_console.log"
        });

        let config = parse_options(&opts).unwrap();
        assert_eq!(config.console_output, Some("/tmp/k6_console.log".to_string()));
    }

    #[test]
    fn parse_local_ips_and_rps() {
        let opts = json!({
            "vus": 1,
            "duration": "10s",
            "localIPs": ["192.168.1.1", "192.168.1.2"],
            "rps": 100
        });

        let config = parse_options(&opts).unwrap();
        assert_eq!(config.local_ips, vec!["192.168.1.1", "192.168.1.2"]);
        assert_eq!(config.rps, 100);
    }

    #[test]
    fn parse_externally_controlled() {
        let opts = json!({
            "scenarios": {
                "ext": {
                    "executor": "externally-controlled",
                    "vus": 5,
                    "maxVUs": 20,
                    "duration": "10m"
                }
            }
        });

        let config = parse_options(&opts).unwrap();
        let scenario = config.scenarios.get("ext").unwrap();
        match &scenario.executor {
            ExecutorType::ExternallyControlled { vus, max_vus, duration } => {
                assert_eq!(*vus, 5);
                assert_eq!(*max_vus, 20);
                assert_eq!(*duration, Duration::from_secs(600));
            }
            other => panic!("expected ExternallyControlled, got {other:?}"),
        }
    }
}
