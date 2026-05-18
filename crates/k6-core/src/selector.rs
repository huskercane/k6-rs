//! Structural representation of a metric reference (CG-5).
//!
//! Replaces stringly-typed selector handling that previously lived in three
//! places independently:
//!   - `thresholds::resolve_stat` — flat string equality lookup against the
//!     snapshot's stored metric names (`http_req_duration{status:200}` only
//!     matched if the storage key was byte-for-byte identical).
//!   - `k6-conformance/canonical::selector_string` — owned the canonical
//!     `name{k:v,k:v}` encoding for diff selector keys.
//!   - `k6-conformance/adapters/{upstream,k6rs}::split_selector` — two
//!     near-identical local parsers.
//!
//! All three now route through `MetricSelector`. The canonical form is
//! `name{k1:v1,k2:v2}` with tags in **alphabetical key order**; parsing
//! tolerates whitespace around keys/values/separators and accepts tags in
//! any order, but rejects trailing garbage after the closing `}`.
//!
//! Grammar (intentionally minimal — see [[project-conformance-spike]]):
//!   selector := name [ "{" tag_list "}" ]
//!   tag_list := tag ( "," tag )*
//!   tag      := key ":" value
//!   name, key := UTF-8 bytes excluding any of `{}:,` (no quoting)
//!   value     := UTF-8 bytes excluding `{`, `}`, `,` (CG-3: real-world tag
//!                values can contain `:`, e.g. WebSocket URLs `ws://host`;
//!                only the FIRST `:` in a tag pair is the key/value separator).
//!
//! If a future input requires `,` or `{`/`}` inside a value, widen the
//! grammar then; do not pre-emptively add quoting.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricSelector {
    pub name: String,
    /// Tag map. `BTreeMap` so iteration order — and therefore canonical
    /// serialization — is deterministic by key.
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SelectorParseError {
    /// `""` or `"{...}"` — no name before the brace.
    EmptyName,
    /// Opening brace without a matching close, close without an open, or
    /// non-whitespace bytes after the close.
    UnbalancedBraces,
    /// Bare name (the part before any `{`) contained a reserved character:
    /// any of `{`, `}`, `:`, `,`. The inner string is the offending name.
    /// Separate from `MalformedTagPair` because the failure isn't in a
    /// tag pair — `name:bad` and `bad,name{k:v}` are name-shape errors,
    /// not tag-shape errors.
    InvalidName(String),
    /// A tag pair didn't contain exactly one `:`, had an empty key, had a
    /// reserved character (`{`, `}`, `:`, `,`) inside the key or value
    /// beyond the single separator `:`, or repeated a key already present.
    /// The inner string is a short diagnostic — either the offending pair
    /// or `"duplicate key: <key>"`.
    MalformedTagPair(String),
}

impl fmt::Display for SelectorParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyName => write!(f, "metric selector has empty name"),
            Self::UnbalancedBraces => write!(f, "unbalanced braces in metric selector"),
            Self::InvalidName(n) => write!(f, "invalid character in metric name: {n:?}"),
            Self::MalformedTagPair(p) => write!(f, "malformed tag pair: {p:?}"),
        }
    }
}

impl std::error::Error for SelectorParseError {}

/// Reserved characters in `name` or tag `key` — anything that would
/// confuse the structural grammar.
fn contains_reserved(s: &str) -> bool {
    s.bytes().any(|b| matches!(b, b'{' | b'}' | b':' | b','))
}

/// Reserved characters in tag `value`. CG-3: `:` is permitted (only the
/// first `:` in a pair is the key/value separator; values like
/// `ws://localhost` are common in real k6 tags). `{`, `}`, `,` remain
/// structural and disallowed.
fn contains_reserved_value(s: &str) -> bool {
    s.bytes().any(|b| matches!(b, b'{' | b'}' | b','))
}

impl MetricSelector {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            tags: BTreeMap::new(),
        }
    }

    /// Builder helper for ergonomic construction in tests and call sites.
    pub fn with_tag(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.tags.insert(k.into(), v.into());
        self
    }

    pub fn parse(s: &str) -> Result<Self, SelectorParseError> {
        let s = s.trim();
        if s.is_empty() {
            return Err(SelectorParseError::EmptyName);
        }
        match s.find('{') {
            None => {
                // No tags. Reject stray closing braces, AND any other
                // reserved char in the bare name (`:`, `,`). Without this
                // the parser silently accepted `name:bad` as a tagless
                // name, masking concatenated-selector or appended-stat
                // bugs at call sites.
                if s.contains('}') {
                    return Err(SelectorParseError::UnbalancedBraces);
                }
                if contains_reserved(s) {
                    return Err(SelectorParseError::InvalidName(s.to_string()));
                }
                Ok(Self::new(s.to_string()))
            }
            Some(open) => {
                let name = s[..open].trim();
                if name.is_empty() {
                    return Err(SelectorParseError::EmptyName);
                }
                // Name appears before `{`, so the slice cannot itself
                // contain `{`. Reject the other reserved chars (`:`, `,`,
                // `}`) so e.g. `na}me{k:v}` doesn't silently pass.
                if contains_reserved(name) {
                    return Err(SelectorParseError::InvalidName(name.to_string()));
                }
                let Some(close_rel) = s[open + 1..].find('}') else {
                    return Err(SelectorParseError::UnbalancedBraces);
                };
                let close = open + 1 + close_rel;
                // Trailing garbage check: anything after the closing brace
                // (other than whitespace) is an error. Catches bugs where
                // the caller accidentally concatenated multiple selectors
                // or appended a stat name.
                if !s[close + 1..].trim().is_empty() {
                    return Err(SelectorParseError::UnbalancedBraces);
                }
                let inner = &s[open + 1..close];
                let mut tags = BTreeMap::new();
                if !inner.trim().is_empty() {
                    for pair in inner.split(',') {
                        let pair_trim = pair.trim();
                        if pair_trim.is_empty() {
                            return Err(SelectorParseError::MalformedTagPair(
                                pair_trim.to_string(),
                            ));
                        }
                        // Exactly one colon expected. `splitn(2, ':')`
                        // gives at most two halves; if either still
                        // contains a `:` after split, that's an extra
                        // colon and the grammar rejects it.
                        let mut parts = pair_trim.splitn(2, ':');
                        let key = parts.next().unwrap_or("").trim();
                        let Some(value_raw) = parts.next() else {
                            return Err(SelectorParseError::MalformedTagPair(
                                pair_trim.to_string(),
                            ));
                        };
                        let value = value_raw.trim();
                        if key.is_empty() {
                            return Err(SelectorParseError::MalformedTagPair(
                                pair_trim.to_string(),
                            ));
                        }
                        // Reserved chars per the grammar: keys reject the
                        // full set (`{}:,`) so `{a:b:c}` can't be reread as
                        // a key of `a:b`. Values reject the structural
                        // chars (`{}` and `,`) but permit `:` — real-world
                        // tag values include things like `ws://localhost`.
                        if contains_reserved(key) || contains_reserved_value(value) {
                            return Err(SelectorParseError::MalformedTagPair(
                                pair_trim.to_string(),
                            ));
                        }
                        // Duplicate key check: previously a second
                        // `{a:2}` after `{a:1}` silently overwrote the
                        // first. The grammar doesn't permit it.
                        if tags.contains_key(key) {
                            return Err(SelectorParseError::MalformedTagPair(format!(
                                "duplicate key: {key}"
                            )));
                        }
                        tags.insert(key.to_string(), value.to_string());
                    }
                }
                Ok(Self {
                    name: name.to_string(),
                    tags,
                })
            }
        }
    }

    /// Canonical serialization: `name{k1:v1,k2:v2}` with tags alphabetical
    /// by key, no whitespace. Round-trips through `parse`.
    pub fn canonical(&self) -> String {
        if self.tags.is_empty() {
            return self.name.clone();
        }
        let inner: Vec<String> = self.tags.iter().map(|(k, v)| format!("{k}:{v}")).collect();
        format!("{}{{{}}}", self.name, inner.join(","))
    }
}

impl fmt::Display for MetricSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.canonical())
    }
}

impl FromStr for MetricSelector {
    type Err = SelectorParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_no_tags() {
        let s = MetricSelector::parse("http_reqs").unwrap();
        assert_eq!(s.name, "http_reqs");
        assert!(s.tags.is_empty());
        assert_eq!(s.canonical(), "http_reqs");
    }

    #[test]
    fn parse_single_tag() {
        let s = MetricSelector::parse("http_req_duration{status:200}").unwrap();
        assert_eq!(s.name, "http_req_duration");
        assert_eq!(s.tags.len(), 1);
        assert_eq!(s.tags["status"], "200");
        assert_eq!(s.canonical(), "http_req_duration{status:200}");
    }

    #[test]
    fn parse_multi_tag_normalizes_order() {
        // Input has tags in {b,a} order; the BTreeMap reorders to {a,b}
        // and canonical reflects that. This is the bug-fix path: today,
        // threshold storage and threshold keys with different tag order
        // silently miss each other.
        let s = MetricSelector::parse("name{b:2,a:1}").unwrap();
        // Iterating the BTreeMap yields a before b.
        let keys: Vec<&String> = s.tags.keys().collect();
        assert_eq!(keys, vec!["a", "b"]);
        assert_eq!(s.canonical(), "name{a:1,b:2}");
    }

    #[test]
    fn canonical_round_trips() {
        let inputs = [
            "http_reqs",
            "http_req_duration{status:200}",
            "http_req_duration{method:GET,status:200}",
            "name{x:y}",
        ];
        for input in inputs {
            let parsed = MetricSelector::parse(input).unwrap();
            let canon = parsed.canonical();
            let reparsed = MetricSelector::parse(&canon).unwrap();
            assert_eq!(parsed, reparsed, "round-trip diverged for {input:?}");
        }
    }

    #[test]
    fn parse_tolerates_whitespace() {
        // Threshold expressions in real scripts sometimes carry spaces
        // around the tag separator or the colon. The canonical form
        // strips that whitespace; matching against storage must still work.
        let s = MetricSelector::parse("name{ a : 1 , b : 2 }").unwrap();
        assert_eq!(s.tags["a"], "1");
        assert_eq!(s.tags["b"], "2");
        assert_eq!(s.canonical(), "name{a:1,b:2}");
    }

    #[test]
    fn parse_rejects_empty_name() {
        assert_eq!(
            MetricSelector::parse(""),
            Err(SelectorParseError::EmptyName)
        );
        assert_eq!(
            MetricSelector::parse("   "),
            Err(SelectorParseError::EmptyName)
        );
        assert_eq!(
            MetricSelector::parse("{k:v}"),
            Err(SelectorParseError::EmptyName)
        );
    }

    #[test]
    fn parse_rejects_unbalanced_braces() {
        // Missing close.
        assert_eq!(
            MetricSelector::parse("name{k:v"),
            Err(SelectorParseError::UnbalancedBraces)
        );
        // Stray close without open.
        assert_eq!(
            MetricSelector::parse("name}"),
            Err(SelectorParseError::UnbalancedBraces)
        );
    }

    #[test]
    fn parse_rejects_trailing_garbage() {
        // Important defense per CG-5 design review: bytes after the
        // closing `}` must be an error, otherwise a caller that
        // accidentally appends a stat name (e.g. `name{k:v}.p99`)
        // would silently parse only the prefix and behave wrong.
        assert_eq!(
            MetricSelector::parse("name{k:v}.p99"),
            Err(SelectorParseError::UnbalancedBraces)
        );
        assert_eq!(
            MetricSelector::parse("name{k:v} extra"),
            Err(SelectorParseError::UnbalancedBraces)
        );
        // Trailing whitespace alone is fine — that's tolerated trim.
        assert!(MetricSelector::parse("name{k:v}   ").is_ok());
    }

    #[test]
    fn parse_rejects_malformed_tag_pair() {
        // Missing colon.
        assert!(matches!(
            MetricSelector::parse("name{kv}"),
            Err(SelectorParseError::MalformedTagPair(_))
        ));
        // Empty key.
        assert!(matches!(
            MetricSelector::parse("name{:v}"),
            Err(SelectorParseError::MalformedTagPair(_))
        ));
        // Empty pair (`,,` or trailing `,`).
        assert!(matches!(
            MetricSelector::parse("name{a:1,,b:2}"),
            Err(SelectorParseError::MalformedTagPair(_))
        ));
    }

    #[test]
    fn empty_tag_block_parses_as_no_tags() {
        // `name{}` should yield no tags (not an error). Canonical form
        // drops the braces — they're not part of the no-tag encoding.
        let s = MetricSelector::parse("name{}").unwrap();
        assert!(s.tags.is_empty());
        assert_eq!(s.canonical(), "name");
    }

    #[test]
    fn builder_with_tag_chains() {
        let s = MetricSelector::new("http_req_duration")
            .with_tag("status", "200")
            .with_tag("method", "GET");
        assert_eq!(s.canonical(), "http_req_duration{method:GET,status:200}");
    }

    #[test]
    fn display_writes_canonical() {
        let s = MetricSelector::new("name")
            .with_tag("b", "2")
            .with_tag("a", "1");
        assert_eq!(format!("{s}"), "name{a:1,b:2}");
    }

    #[test]
    fn from_str_calls_parse() {
        let s: MetricSelector = "name{x:y}".parse().unwrap();
        assert_eq!(s.name, "name");
        assert_eq!(s.tags["x"], "y");
    }

    #[test]
    fn parse_rejects_reserved_chars_in_bare_name() {
        // CG-5 follow-up: the module docstring excludes `:`, `,`, `{`, `}`
        // from name. Before the fix, parse silently accepted `name:bad`
        // and `name,extra` as tagless names, masking concatenated-selector
        // bugs at call sites.
        assert!(matches!(
            MetricSelector::parse("name:bad"),
            Err(SelectorParseError::InvalidName(_))
        ));
        assert!(matches!(
            MetricSelector::parse("name,extra"),
            Err(SelectorParseError::InvalidName(_))
        ));
    }

    #[test]
    fn parse_rejects_reserved_chars_in_name_before_brace() {
        // Name slice ends at the first `{`, so `{` itself can't appear in
        // it. The other reserved chars must still be rejected — without
        // this, `na}me{k:v}` would silently pass.
        assert!(matches!(
            MetricSelector::parse("na}me{k:v}"),
            Err(SelectorParseError::InvalidName(_))
        ));
        assert!(matches!(
            MetricSelector::parse("na:me{k:v}"),
            Err(SelectorParseError::InvalidName(_))
        ));
        assert!(matches!(
            MetricSelector::parse("na,me{k:v}"),
            Err(SelectorParseError::InvalidName(_))
        ));
    }

    #[test]
    fn parse_allows_colon_in_tag_value() {
        // CG-3 fix: WS tags (`{url:ws://localhost}`) and other real-world
        // tag values contain `:` inside the value half. Only the FIRST
        // `:` in a pair is the key/value separator. Earlier CG-5 versions
        // rejected this; the grammar now permits `:` in values.
        let s = MetricSelector::parse("name{a:1:2}").unwrap();
        assert_eq!(s.tags["a"], "1:2");
        assert_eq!(s.canonical(), "name{a:1:2}");

        let s = MetricSelector::parse("ws_metric{url:ws://localhost:8080/path}").unwrap();
        assert_eq!(s.name, "ws_metric");
        assert_eq!(s.tags["url"], "ws://localhost:8080/path");
        // Round-trips losslessly.
        assert_eq!(MetricSelector::parse(&s.canonical()).unwrap(), s);
    }

    #[test]
    fn parse_rejects_duplicate_tag_key() {
        // CG-5 follow-up: BTreeMap::insert previously silently overwrote
        // the first value for the same key. `name{a:1,a:2}` is now a
        // hard error.
        let err = MetricSelector::parse("name{a:1,a:2}").unwrap_err();
        match err {
            SelectorParseError::MalformedTagPair(d) => {
                assert!(
                    d.contains("duplicate"),
                    "diagnostic should mention duplicate: {d:?}"
                );
                assert!(d.contains('a'), "diagnostic should name the key: {d:?}");
            }
            other => panic!("expected MalformedTagPair, got {other:?}"),
        }
    }
}
