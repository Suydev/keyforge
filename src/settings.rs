//! Runtime-editable settings: providers and routing policy.
//!
//! Lives at `~/.config/tabi/providers.json`. The file is the source of truth for
//! *editing*; the dashboard can verify-and-append a provider but does not
//! otherwise rewrite it. Hot-reloaded on mtime change, so `tabi` never needs
//! restarting to pick up a new host or a tuned weight.
//!
//! # Why a file rather than dashboard CRUD
//! A form that rewrites config needs validation, atomic writes, backups and a
//! confirm-before-delete flow — a lot of surface area whose failure mode is a
//! corrupted config that stops the gateway booting. A file you can edit with any
//! editor, plus a narrow "verify and append" path, gets the convenience without
//! the risk.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One upstream host. A provider may have several: the first is primary, the
/// rest are backups tried in order.
///
/// Verified for tabitoken: `tabitoken.com` and `tabitoken.cc` serve identical
/// models AND report identical `total_usage` (12640) for the same key — so they
/// are one deployment behind two names, sharing one wallet. Modelling them as one
/// provider avoids double-counting the same balance.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostCfg {
    pub host: String,
    /// Skip this host without deleting it.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Free-text note shown in the dashboard ("backup", "eu region", …).
    #[serde(default)]
    pub note: String,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderCfg {
    pub id: String,
    pub label: String,
    /// Primary first, then backups in order.
    pub hosts: Vec<HostCfg>,
    /// Key file, relative to `$HOME` or absolute.
    pub keys_file: String,
    /// Flat pre-deduction hold in USD, used until a real quota 403 reports the
    /// true figure (see `state::learn_hold`).
    pub hold: f64,
    /// Assumed starting balance for an unseen key.
    pub initial_guess: f64,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Manual priority nudge, subtracted from the routing score. Positive values
    /// make a provider more attractive.
    #[serde(default)]
    pub bias: f64,
    #[serde(default)]
    pub note: String,
    /// Models this provider serves. Declared by hand and also MERGED with
    /// whatever the free `/v1/models` probe discovers (`set_models_seen` unions
    /// into this list), so the dashboard stays truthful even when probes return
    /// nothing.
    #[serde(default)]
    pub models: Vec<String>,
    /// Client-asked model → the model name the upstream actually serves. Applied
    /// to the request body before sending, so `claude-opus-5` can be rewritten to
    /// `tabi/claude-opus-5` (or whatever the channel name is) per provider.
    #[serde(default)]
    pub model_map: std::collections::HashMap<String, String>,
}

/// Routing policy — every weight tunable without a rebuild.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoutingCfg {
    /// A provider is "slow" when its p50 TTFB exceeds the best provider's by
    /// more than this multiple.
    ///
    /// Relative rather than absolute on purpose: an absolute threshold has to be
    /// retuned whenever conditions change, and on a bad network every provider
    /// trips it at once. Relative always answers the question that matters —
    /// "is there something better right now?"
    #[serde(default = "d_slow_mult")]
    pub slow_multiplier: f64,

    /// Samples required before latency is trusted enough to demote on.
    #[serde(default = "d_min_samples")]
    pub min_samples: u32,

    /// Weight on recent error rate (0..1) in the score. Errors are weighted well
    /// above latency because a failure costs a whole extra round trip.
    #[serde(default = "d_err_weight")]
    pub error_weight: f64,

    /// Weight on the consecutive-failure streak, squared.
    #[serde(default = "d_streak_weight")]
    pub streak_weight: f64,

    /// Seconds after which a failure streak has fully decayed.
    ///
    /// Without decay the penalty is self-reinforcing: a demoted provider is
    /// never chosen, so it never succeeds, so its streak never clears. Observed
    /// live — gorouter carried a 3-failure streak for 32 minutes while every
    /// free uptime probe passed.
    #[serde(default = "d_streak_halflife")]
    pub streak_halflife_secs: u64,

    /// Consecutive failures before the circuit opens.
    #[serde(default = "d_breaker_trip")]
    pub breaker_trip: u32,

    /// Circuit-open durations in seconds, indexed by how deep the streak is.
    #[serde(default = "d_breaker_backoff")]
    pub breaker_backoff_secs: Vec<u64>,

    /// Penalty for a provider that does not advertise the requested model.
    /// Deprioritise, never exclude: `/v1/models` can be stale.
    #[serde(default = "d_model_penalty")]
    pub missing_model_penalty: f64,

    /// Should a successful free uptime probe clear a failure streak?
    ///
    /// The probe already runs every 2 minutes at zero cost and proves the host is
    /// reachable. Using it as a recovery signal is what breaks the loop above.
    #[serde(default = "yes")]
    pub probe_heals_score: bool,

    /// Keep a session on its current provider even if another is faster.
    /// Switching mid-conversation means a new key, a new wallet, and a lost
    /// prompt cache.
    #[serde(default = "yes")]
    pub session_stickiness: bool,

    /// Abandon stickiness if the pinned provider is this many times slower.
    #[serde(default = "d_sticky_escape")]
    pub sticky_escape_multiplier: f64,
}

fn d_slow_mult() -> f64 {
    2.0
}
fn d_min_samples() -> u32 {
    3
}
fn d_err_weight() -> f64 {
    4.0
}
fn d_streak_weight() -> f64 {
    0.5
}
fn d_streak_halflife() -> u64 {
    120
}
fn d_breaker_trip() -> u32 {
    3
}
fn d_breaker_backoff() -> Vec<u64> {
    vec![15, 45, 120]
}
fn d_model_penalty() -> f64 {
    50.0
}
fn d_sticky_escape() -> f64 {
    4.0
}

impl Default for RoutingCfg {
    fn default() -> Self {
        Self {
            slow_multiplier: d_slow_mult(),
            min_samples: d_min_samples(),
            error_weight: d_err_weight(),
            streak_weight: d_streak_weight(),
            streak_halflife_secs: d_streak_halflife(),
            breaker_trip: d_breaker_trip(),
            breaker_backoff_secs: d_breaker_backoff(),
            missing_model_penalty: d_model_penalty(),
            probe_heals_score: true,
            session_stickiness: true,
            sticky_escape_multiplier: d_sticky_escape(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "d_version")]
    pub version: u32,
    pub providers: Vec<ProviderCfg>,
    #[serde(default)]
    pub routing: RoutingCfg,
}

fn d_version() -> u32 {
    1
}

impl Default for Settings {
    /// Built-in defaults, written out on first run so the file is
    /// self-documenting rather than something you have to invent.
    fn default() -> Self {
        Self {
            version: 1,
            routing: RoutingCfg::default(),
            providers: vec![
                ProviderCfg {
                    id: "tabi".into(),
                    label: "TabiToken".into(),
                    hosts: vec![
                        HostCfg {
                            host: "tabitoken.com".into(),
                            enabled: true,
                            note: "primary".into(),
                        },
                        // Verified same models AND same total_usage => same wallet.
                        HostCfg {
                            host: "tabitoken.cc".into(),
                            enabled: true,
                            note: "backup".into(),
                        },
                    ],
                    keys_file: "git_gorouter_tabitoken/tabitoken-keys.txt".into(),
                    hold: 0.80,
                    initial_guess: 120.0,
                    enabled: true,
                    bias: 0.0,
                     note: "flat per-request hold; refundable".into(),
                    models: vec![],
                    model_map: std::collections::HashMap::new(),
                },
                ProviderCfg {
                    id: "gorouter".into(),
                    label: "GoRouter".into(),
                    hosts: vec![HostCfg {
                        host: "gorouter.app".into(),
                        enabled: true,
                        note: "primary".into(),
                    }],
                    keys_file: "git_gorouter_tabitoken/gorouter-keys.txt".into(),
                    hold: 0.30,
                    initial_guess: 50.0,
                    enabled: true,
                    bias: 0.0,
                    note: "".into(),
                    models: vec![],
                    model_map: std::collections::HashMap::new(),
                },
                ProviderCfg {
                    id: "justwoker".into(),
                    label: "JustWoker".into(),
                    hosts: vec![HostCfg {
                        host: "api.justwoker.icu".into(),
                        enabled: true,
                        note: "primary".into(),
                    }],
                    keys_file: "git_gorouter_tabitoken/justwoker-keys.txt".into(),
                    hold: 0.10,
                    initial_guess: 70.0,
                    enabled: true,
                    bias: 0.0,
                    note: "per-token pricing: measured ~$1/turn on long thinking".into(),
                     models: vec![],
                    model_map: std::collections::HashMap::new(),
                },
            ],
        }
    }
}

impl Settings {
    pub fn path() -> PathBuf {
        if let Ok(p) = std::env::var("TABI_PROVIDERS") {
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
        crate::config::home().join(".config/tabi/providers.json")
    }

    /// Load, writing defaults if the file does not exist yet.
    pub fn load_or_init() -> (Self, Option<String>) {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Settings>(&text) {
                Ok(s) => (s, None),
                // A malformed file must not stop the gateway: fall back to
                // defaults and surface the reason.
                Err(e) => (
                    Settings::default(),
                    Some(format!(
                        "{} is invalid ({e}); using built-in defaults",
                        path.display()
                    )),
                ),
            },
            Err(_) => {
                let s = Settings::default();
                let _ = s.save();
                (s, None)
            }
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Self::path();
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        let body = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &body).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn mtime() -> u64 {
        std::fs::metadata(Self::path())
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    pub fn provider(&self, id: &str) -> Option<&ProviderCfg> {
        self.providers.iter().find(|p| p.id == id && p.enabled)
    }

    /// Enabled hosts for a provider, primary first.
    pub fn hosts(&self, id: &str) -> Vec<String> {
        self.provider(id)
            .map(|p| {
                p.hosts
                    .iter()
                    .filter(|h| h.enabled)
                    .map(|h| h.host.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Append a verified provider, or add a host to an existing one.
    /// Returns true when something changed.
    pub fn upsert_provider(&mut self, cfg: ProviderCfg) -> bool {
        if let Some(existing) = self.providers.iter_mut().find(|p| p.id == cfg.id) {
            let mut changed = false;
            for h in cfg.hosts {
                if !existing.hosts.iter().any(|e| e.host == h.host) {
                    existing.hosts.push(h);
                    changed = true;
                }
            }
            changed
        } else {
            self.providers.push(cfg);
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_include_the_verified_tabi_backup() {
        let s = Settings::default();
        let hosts = s.hosts("tabi");
        assert_eq!(hosts, vec!["tabitoken.com", "tabitoken.cc"]);
    }

    #[test]
    fn disabled_host_is_skipped_but_retained() {
        let mut s = Settings::default();
        let p = s.providers.iter_mut().find(|p| p.id == "tabi").unwrap();
        p.hosts[1].enabled = false;
        assert_eq!(s.hosts("tabi"), vec!["tabitoken.com"]);
        assert_eq!(
            s.provider("tabi").unwrap().hosts.len(),
            2,
            "kept, not deleted"
        );
    }

    #[test]
    fn disabled_provider_disappears_from_lookup() {
        let mut s = Settings::default();
        s.providers
            .iter_mut()
            .find(|p| p.id == "gorouter")
            .unwrap()
            .enabled = false;
        assert!(s.provider("gorouter").is_none());
    }

    #[test]
    fn upsert_adds_a_new_provider() {
        let mut s = Settings::default();
        let n = s.providers.len();
        assert!(s.upsert_provider(ProviderCfg {
            id: "openrouter".into(),
            label: "OpenRouter".into(),
            hosts: vec![HostCfg {
                host: "openrouter.ai".into(),
                enabled: true,
                note: String::new()
            }],
            keys_file: "keys/openrouter.txt".into(),
            hold: 0.0,
            initial_guess: 10.0,
            enabled: true,
            bias: 0.0,
            note: String::new(),
            models: vec![],
            model_map: std::collections::HashMap::new(),
        }));
        assert_eq!(s.providers.len(), n + 1);
    }

    #[test]
    fn upsert_adds_a_host_to_an_existing_provider_without_duplicating() {
        let mut s = Settings::default();
        let add = |h: &str| ProviderCfg {
            id: "gorouter".into(),
            label: "GoRouter".into(),
            hosts: vec![HostCfg {
                host: h.into(),
                enabled: true,
                note: "mirror".into(),
            }],
            keys_file: "x".into(),
            hold: 0.3,
            initial_guess: 50.0,
            enabled: true,
            bias: 0.0,
            note: String::new(),
            models: vec![],
            model_map: std::collections::HashMap::new(),
        };
        assert!(s.upsert_provider(add("gorouter2.app")));
        assert!(
            !s.upsert_provider(add("gorouter2.app")),
            "second add is a no-op"
        );
        assert_eq!(s.hosts("gorouter"), vec!["gorouter.app", "gorouter2.app"]);
    }

    #[test]
    fn routing_defaults_are_sane() {
        let r = RoutingCfg::default();
        assert!(r.error_weight > r.streak_weight, "errors must dominate");
        assert!(
            r.probe_heals_score,
            "free probe must be able to clear a streak"
        );
        assert_eq!(r.breaker_backoff_secs.len(), 3);
        assert!(r.streak_halflife_secs > 0, "streaks must decay");
    }

    #[test]
    fn missing_routing_block_falls_back_to_defaults() {
        // An older or hand-trimmed file must still load.
        let json = r#"{"version":1,"providers":[]}"#;
        let s: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(s.routing.slow_multiplier, 2.0);
    }
}
