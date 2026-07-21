#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChatId(pub i64);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WebhookName(pub String);

#[derive(Clone)]
pub struct WebhookUrl(pub String);

impl std::fmt::Debug for WebhookUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WebhookUrl(«redacted»)")
    }
}

#[derive(Debug, Clone)]
pub enum ChatRef {
    Username(String), // stored without leading '@'
    Id(ChatId),
}

#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub any_keywords: Vec<String>,
    pub exclude_hashtags: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RouteCfg {
    pub name: String,
    pub from: ChatRef,
    pub to: Vec<WebhookName>,
    pub filter: Option<Filter>,
    /// Embed stripe color as a 24-bit RGB integer, already parsed from the
    /// optional `color: "#RRGGBB"` key (defaults to [`DEFAULT_EMBED_COLOR`]).
    pub color: u32,
}

/// Parse a `#RRGGBB` (or bare `RRGGBB`) hex color into a 24-bit RGB integer.
///
/// Strict by design — this runs at config load so a typo fails the boot with
/// the offending route named, rather than silently rendering a black stripe.
/// Exactly six hex digits: no 3-digit shorthand, no alpha channel, no named
/// colors. YAML treats an unquoted `#` as a comment, so `color: "#29B6F6"`
/// must be quoted; the bare form is accepted for the people who forget.
pub fn parse_hex_color(s: &str) -> Option<u32> {
    let hex = s.strip_prefix('#').unwrap_or(s);
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaMode {
    Reupload,
    Placeholder,
}

#[derive(Debug, Clone)]
pub struct MediaCfg {
    pub mode: MediaMode,
    pub max_bytes: u64,
}

/// Refresh-worker cadence + how far back to keep updating posts.
///
/// Two concerns run on different schedules (queued-polish §1):
/// * **edits + deletes** — checked every `interval_mins` for `horizon_hours`
///   (the most pertinent changes; kept for the full 48h horizon).
/// * **reactions + comments** — QoL only, so checked a few times early
///   (~`reaction_early_check_secs`, then at `interval_mins` and again at
///   `reaction_horizon_mins`) and then frozen once the post ages past
///   `reaction_horizon_mins`.
#[derive(Debug, Clone, Copy)]
pub struct RefreshCfg {
    pub interval_mins: u64,
    pub horizon_hours: u64,
    /// Stop refreshing reactions/comments once a post is older than this.
    pub reaction_horizon_mins: u64,
    /// The early ("+1 min") reaction burst check, in seconds after posting.
    pub reaction_early_check_secs: u64,
}

impl Default for RefreshCfg {
    fn default() -> Self {
        RefreshCfg {
            interval_mins: 30,
            horizon_hours: 48,
            reaction_horizon_mins: 60,
            reaction_early_check_secs: 60,
        }
    }
}

/// Where the sqlite message store lives.
#[derive(Debug, Clone)]
pub struct StoreCfg {
    pub path: PathBuf,
}

impl Default for StoreCfg {
    fn default() -> Self {
        StoreCfg {
            path: PathBuf::from("relay.db"),
        }
    }
}

#[derive(Debug)]
pub struct Config {
    pub routes: Vec<RouteCfg>,
    pub webhooks: HashMap<WebhookName, WebhookUrl>,
    pub ops_webhook: Option<WebhookUrl>,
    pub media: MediaCfg,
    pub refresh: RefreshCfg,
    pub store: StoreCfg,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid YAML: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("webhook '{name}': env var {env} is not set")]
    MissingEnv { name: String, env: String },
    #[error("route '{route}' references unknown webhook '{hook}'")]
    UnknownWebhook { route: String, hook: String },
    #[error("route '{route}': 'to' must list at least one webhook")]
    EmptyTo { route: String },
    #[error("route '{route}': 'from' must be a string or integer, got {got}")]
    InvalidFrom { route: String, got: String },
    #[error("route '{route}': invalid color '{got}'; expected a quoted \"#RRGGBB\" hex string")]
    InvalidColor { route: String, got: String },
}

// ---- raw (serde) layer: never leaves this module ----
mod raw {
    use serde::Deserialize;
    use std::collections::HashMap;

    #[derive(Deserialize)]
    pub struct Root {
        pub routes: Vec<Route>,
        pub webhooks: HashMap<String, EnvRef>,
        pub ops_webhook: Option<EnvRef>,
        pub media: Media,
        #[serde(default)]
        pub refresh: Option<Refresh>,
        #[serde(default)]
        pub store: Option<Store>,
    }
    #[derive(Deserialize)]
    pub struct Refresh {
        #[serde(default = "default_interval_mins")]
        pub interval_mins: u64,
        #[serde(default = "default_horizon_hours")]
        pub horizon_hours: u64,
        #[serde(default = "default_reaction_horizon_mins")]
        pub reaction_horizon_mins: u64,
        #[serde(default = "default_reaction_early_check_secs")]
        pub reaction_early_check_secs: u64,
    }
    fn default_interval_mins() -> u64 {
        30
    }
    fn default_horizon_hours() -> u64 {
        48
    }
    fn default_reaction_horizon_mins() -> u64 {
        60
    }
    fn default_reaction_early_check_secs() -> u64 {
        60
    }
    #[derive(Deserialize)]
    pub struct Store {
        #[serde(default = "default_store_path")]
        pub path: String,
    }
    fn default_store_path() -> String {
        "relay.db".to_string()
    }
    #[derive(Deserialize)]
    pub struct Route {
        pub name: String,
        pub from: serde_yaml::Value, // "@name" | "name" | -100123
        pub to: Vec<String>,
        pub filter: Option<Filter>,
        #[serde(default)]
        pub color: Option<String>, // "#RRGGBB"
    }
    #[derive(Deserialize, Default)]
    pub struct Filter {
        #[serde(default)]
        pub any_keywords: Vec<String>,
        #[serde(default)]
        pub exclude_hashtags: Vec<String>,
    }
    #[derive(Deserialize)]
    pub struct EnvRef {
        pub env: String,
    }
    #[derive(Deserialize)]
    pub struct Media {
        pub mode: String,
        pub max_bytes: u64,
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
            path: path.display().to_string(),
            source: e,
        })?;
        let raw: raw::Root = serde_yaml::from_str(&text)?;

        let mut webhooks = HashMap::new();
        for (name, r) in raw.webhooks {
            let url = std::env::var(&r.env).map_err(|_| ConfigError::MissingEnv {
                name: name.clone(),
                env: r.env.clone(),
            })?;
            webhooks.insert(WebhookName(name), WebhookUrl(url));
        }
        let ops_webhook = match raw.ops_webhook {
            None => None,
            Some(r) => Some(WebhookUrl(std::env::var(&r.env).map_err(|_| {
                ConfigError::MissingEnv {
                    name: "ops_webhook".into(),
                    env: r.env.clone(),
                }
            })?)),
        };

        let mut routes = Vec::new();
        for r in raw.routes {
            if r.to.is_empty() {
                return Err(ConfigError::EmptyTo { route: r.name });
            }
            let to: Vec<WebhookName> = r.to.into_iter().map(WebhookName).collect();
            for h in &to {
                if !webhooks.contains_key(h) {
                    return Err(ConfigError::UnknownWebhook {
                        route: r.name.clone(),
                        hook: h.0.clone(),
                    });
                }
            }
            let from = match &r.from {
                serde_yaml::Value::Number(n) => {
                    let id = n.as_i64().ok_or_else(|| ConfigError::InvalidFrom {
                        route: r.name.clone(),
                        got: format!("number {}", n),
                    })?;
                    ChatRef::Id(ChatId(id))
                }
                serde_yaml::Value::String(s) => {
                    ChatRef::Username(s.trim_start_matches('@').to_string())
                }
                other => {
                    return Err(ConfigError::InvalidFrom {
                        route: r.name.clone(),
                        got: match other {
                            serde_yaml::Value::Bool(b) => format!("boolean {}", b),
                            serde_yaml::Value::Null => "null".to_string(),
                            serde_yaml::Value::Sequence(_) => "sequence/array".to_string(),
                            serde_yaml::Value::Mapping(_) => "mapping/object".to_string(),
                            _ => format!("{:?}", other),
                        },
                    })
                }
            };
            let color = match &r.color {
                None => crate::render::DEFAULT_EMBED_COLOR,
                Some(raw) => parse_hex_color(raw).ok_or_else(|| ConfigError::InvalidColor {
                    route: r.name.clone(),
                    got: raw.clone(),
                })?,
            };
            routes.push(RouteCfg {
                name: r.name,
                from,
                to,
                filter: r.filter.map(|f| Filter {
                    any_keywords: f.any_keywords,
                    exclude_hashtags: f.exclude_hashtags,
                }),
                color,
            });
        }

        let mode = match raw.media.mode.as_str() {
            "placeholder" => MediaMode::Placeholder,
            _ => MediaMode::Reupload,
        };
        let refresh = raw
            .refresh
            .map_or_else(RefreshCfg::default, |r| RefreshCfg {
                interval_mins: r.interval_mins,
                horizon_hours: r.horizon_hours,
                reaction_horizon_mins: r.reaction_horizon_mins,
                reaction_early_check_secs: r.reaction_early_check_secs,
            });
        let store = raw.store.map_or_else(StoreCfg::default, |s| StoreCfg {
            path: PathBuf::from(s.path),
        });

        Ok(Config {
            routes,
            webhooks,
            ops_webhook,
            media: MediaCfg {
                mode,
                max_bytes: raw.media.max_bytes,
            },
            refresh,
            store,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp(name: &str, yaml: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("tr-cfg-{}-{}.yaml", std::process::id(), name));
        std::fs::write(&p, yaml).unwrap();
        p
    }

    #[test]
    fn loads_valid_config() {
        std::env::set_var("TEST_HOOK", "https://discord.com/api/webhooks/1/x");
        let p = write_tmp(
            "loads_valid_config",
            "routes:\n  - name: r1\n    from: \"@chan\"\n    to: [h1]\nwebhooks:\n  h1: { env: TEST_HOOK }\nmedia: { mode: reupload, max_bytes: 1000 }\n",
        );
        let c = Config::load(&p).unwrap();
        assert_eq!(c.routes.len(), 1);
        assert!(matches!(c.routes[0].from, ChatRef::Username(ref u) if u == "chan"));
    }

    #[test]
    fn refresh_and_store_default_when_absent() {
        std::env::set_var("TEST_HOOK_DEF", "https://discord.com/api/webhooks/1/x");
        let p = write_tmp(
            "refresh_store_defaults",
            "routes: []\nwebhooks:\n  h1: { env: TEST_HOOK_DEF }\nmedia: { mode: reupload, max_bytes: 1000 }\n",
        );
        let c = Config::load(&p).unwrap();
        assert_eq!(c.refresh.interval_mins, 30);
        assert_eq!(c.refresh.horizon_hours, 48);
        assert_eq!(c.refresh.reaction_horizon_mins, 60);
        assert_eq!(c.refresh.reaction_early_check_secs, 60);
        assert_eq!(c.store.path, std::path::PathBuf::from("relay.db"));
    }

    #[test]
    fn refresh_reaction_knobs_override_parses() {
        std::env::set_var("TEST_HOOK_RXN", "https://discord.com/api/webhooks/1/x");
        let p = write_tmp(
            "refresh_reaction_override",
            "routes: []\nwebhooks:\n  h1: { env: TEST_HOOK_RXN }\nmedia: { mode: reupload, max_bytes: 1000 }\nrefresh: { reaction_horizon_mins: 90, reaction_early_check_secs: 30 }\n",
        );
        let c = Config::load(&p).unwrap();
        // Unset edit/delete knobs keep their defaults...
        assert_eq!(c.refresh.interval_mins, 30);
        assert_eq!(c.refresh.horizon_hours, 48);
        // ...while the reaction knobs take the configured values.
        assert_eq!(c.refresh.reaction_horizon_mins, 90);
        assert_eq!(c.refresh.reaction_early_check_secs, 30);
    }

    #[test]
    fn refresh_and_store_override_parses() {
        std::env::set_var("TEST_HOOK_OVR", "https://discord.com/api/webhooks/1/x");
        let p = write_tmp(
            "refresh_store_override",
            "routes: []\nwebhooks:\n  h1: { env: TEST_HOOK_OVR }\nmedia: { mode: reupload, max_bytes: 1000 }\nrefresh: { interval_mins: 15, horizon_hours: 72 }\nstore: { path: /tmp/custom.db }\n",
        );
        let c = Config::load(&p).unwrap();
        assert_eq!(c.refresh.interval_mins, 15);
        assert_eq!(c.refresh.horizon_hours, 72);
        assert_eq!(c.store.path, std::path::PathBuf::from("/tmp/custom.db"));
    }

    #[test]
    fn numeric_from_parses_to_id() {
        std::env::set_var("TEST_HOOK_2", "https://discord.com/api/webhooks/1/x");
        let p = write_tmp(
            "numeric_from_parses_to_id",
            "routes:\n  - name: r1\n    from: -1001234\n    to: [h1]\nwebhooks:\n  h1: { env: TEST_HOOK_2 }\nmedia: { mode: reupload, max_bytes: 1000 }\n",
        );
        let c = Config::load(&p).unwrap();
        assert!(matches!(c.routes[0].from, ChatRef::Id(ChatId(-1001234))));
    }

    #[test]
    fn missing_env_names_the_field() {
        std::env::remove_var("NOPE_HOOK");
        let p = write_tmp(
            "missing_env_names_the_field",
            "routes: []\nwebhooks:\n  h1: { env: NOPE_HOOK }\nmedia: { mode: reupload, max_bytes: 1 }\n",
        );
        let e = Config::load(&p).unwrap_err().to_string();
        assert!(
            e.contains("NOPE_HOOK"),
            "error should name the env var: {e}"
        );
    }

    #[test]
    fn route_to_unknown_webhook_rejected() {
        let p = write_tmp(
            "route_to_unknown_webhook_rejected",
            "routes:\n  - name: r1\n    from: \"@c\"\n    to: [ghost]\nwebhooks: {}\nmedia: { mode: reupload, max_bytes: 1 }\n",
        );
        let e = Config::load(&p).unwrap_err().to_string();
        assert!(e.contains("ghost"));
    }

    #[test]
    fn webhook_url_debug_is_redacted() {
        let u = WebhookUrl("https://discord.com/api/webhooks/1/SECRET".into());
        assert!(!format!("{u:?}").contains("SECRET"));
    }

    #[test]
    fn bool_from_rejected() {
        std::env::set_var("TEST_HOOK_BOOL", "https://discord.com/api/webhooks/1/x");
        let p = write_tmp(
            "bool_from_rejected",
            "routes:\n  - name: r1\n    from: true\n    to: [h1]\nwebhooks:\n  h1: { env: TEST_HOOK_BOOL }\nmedia: { mode: reupload, max_bytes: 1000 }\n",
        );
        let e = Config::load(&p).unwrap_err().to_string();
        assert!(
            e.contains("'from'") && e.contains("from"),
            "error should mention 'from' field: {e}"
        );
    }

    #[test]
    fn parse_hex_color_accepts_valid_forms() {
        assert_eq!(parse_hex_color("#29B6F6"), Some(0x29B6F6));
        assert_eq!(parse_hex_color("29B6F6"), Some(0x29B6F6)); // bare, no '#'
        assert_eq!(parse_hex_color("#000000"), Some(0));
        assert_eq!(parse_hex_color("#ffffff"), Some(0xFFFFFF)); // lowercase ok
        assert_eq!(parse_hex_color("#FfEeDd"), Some(0xFFEEDD)); // mixed case ok
    }

    #[test]
    fn parse_hex_color_rejects_malformed() {
        assert_eq!(parse_hex_color(""), None);
        assert_eq!(parse_hex_color("#"), None);
        assert_eq!(parse_hex_color("#29B6F"), None); // 5 digits
        assert_eq!(parse_hex_color("#29B6F6F"), None); // 7 digits
        assert_eq!(parse_hex_color("#FFF"), None); // no 3-digit shorthand
        assert_eq!(parse_hex_color("#GGGGGG"), None); // non-hex
        assert_eq!(parse_hex_color("#29B6F6\n"), None); // trailing junk
        assert_eq!(parse_hex_color("blue"), None); // named color
        assert_eq!(parse_hex_color("#29B6F6ff"), None); // 8-digit rgba
    }

    #[test]
    fn color_defaults_to_neon_blue_when_unset() {
        std::env::set_var(
            "TEST_HOOK_COLOR_DEF",
            "https://discord.com/api/webhooks/1/x",
        );
        let p = write_tmp(
            "color_default",
            "routes:\n  - name: r1\n    from: \"@c\"\n    to: [h1]\nwebhooks:\n  h1: { env: TEST_HOOK_COLOR_DEF }\nmedia: { mode: reupload, max_bytes: 1000 }\n",
        );
        let c = Config::load(&p).unwrap();
        assert_eq!(c.routes[0].color, crate::render::DEFAULT_EMBED_COLOR);
    }

    #[test]
    fn color_override_parses_to_u32() {
        std::env::set_var(
            "TEST_HOOK_COLOR_OVR",
            "https://discord.com/api/webhooks/1/x",
        );
        let p = write_tmp(
            "color_override",
            "routes:\n  - name: r1\n    from: \"@c\"\n    to: [h1]\n    color: \"#FF8800\"\nwebhooks:\n  h1: { env: TEST_HOOK_COLOR_OVR }\nmedia: { mode: reupload, max_bytes: 1000 }\n",
        );
        let c = Config::load(&p).unwrap();
        assert_eq!(c.routes[0].color, 0xFF8800);
    }

    #[test]
    fn malformed_color_rejected_naming_the_route() {
        std::env::set_var(
            "TEST_HOOK_COLOR_BAD",
            "https://discord.com/api/webhooks/1/x",
        );
        let p = write_tmp(
            "color_bad",
            "routes:\n  - name: neon-route\n    from: \"@c\"\n    to: [h1]\n    color: \"not-a-color\"\nwebhooks:\n  h1: { env: TEST_HOOK_COLOR_BAD }\nmedia: { mode: reupload, max_bytes: 1000 }\n",
        );
        let e = Config::load(&p).unwrap_err().to_string();
        assert!(e.contains("neon-route"), "error should name the route: {e}");
        assert!(
            e.contains("not-a-color"),
            "error should echo the bad value: {e}"
        );
    }

    #[test]
    fn float_from_rejected() {
        std::env::set_var("TEST_HOOK_FLOAT", "https://discord.com/api/webhooks/1/x");
        let p = write_tmp(
            "float_from_rejected",
            "routes:\n  - name: r1\n    from: 1.5\n    to: [h1]\nwebhooks:\n  h1: { env: TEST_HOOK_FLOAT }\nmedia: { mode: reupload, max_bytes: 1000 }\n",
        );
        let e = Config::load(&p).unwrap_err().to_string();
        assert!(
            e.contains("'from'") && e.contains("from"),
            "error should mention 'from' field: {e}"
        );
    }
}
