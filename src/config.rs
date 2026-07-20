#![allow(dead_code)]

use std::collections::HashMap;
use std::path::Path;

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

#[derive(Debug)]
pub struct Config {
    pub routes: Vec<RouteCfg>,
    pub webhooks: HashMap<WebhookName, WebhookUrl>,
    pub ops_webhook: Option<WebhookUrl>,
    pub media: MediaCfg,
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
    }
    #[derive(Deserialize)]
    pub struct Route {
        pub name: String,
        pub from: serde_yaml::Value, // "@name" | "name" | -100123
        pub to: Vec<String>,
        pub filter: Option<Filter>,
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
            routes.push(RouteCfg {
                name: r.name,
                from,
                to,
                filter: r.filter.map(|f| Filter {
                    any_keywords: f.any_keywords,
                    exclude_hashtags: f.exclude_hashtags,
                }),
            });
        }

        let mode = match raw.media.mode.as_str() {
            "placeholder" => MediaMode::Placeholder,
            _ => MediaMode::Reupload,
        };
        Ok(Config {
            routes,
            webhooks,
            ops_webhook,
            media: MediaCfg {
                mode,
                max_bytes: raw.media.max_bytes,
            },
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
