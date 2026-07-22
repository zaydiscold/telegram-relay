use crate::config::WebhookUrl;
use std::time::Duration;
use tracing::warn;

const RATE_LIMIT_BUDGET_SECS: f64 = 60.0;
const MAX_429_ATTEMPTS: usize = 20;

pub struct Deliverer {
    http: reqwest::Client,
}

#[derive(Debug)]
pub enum Outcome {
    Delivered,
    Dropped { reason: String },
}

/// Result of posting an embed, carrying the created Discord message id so the
/// refresh worker can later PATCH it.
#[derive(Debug)]
pub enum PostResult {
    Delivered { discord_msg_id: String },
    Dropped { reason: String },
}

impl Default for Deliverer {
    fn default() -> Self {
        Self::new()
    }
}

impl Deliverer {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .pool_idle_timeout(None)
            .tcp_keepalive(Some(Duration::from_secs(30)))
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client");
        Deliverer { http }
    }

    pub async fn post_text(&self, url: &WebhookUrl, username: &str, chunks: &[String]) -> Outcome {
        for chunk in chunks {
            // `allowed_mentions.parse: []` suppresses @everyone/@here/role/user
            // pings: the tokens still render as plain text, but Discord parses no
            // mentions from a relayed message's content.
            let body = serde_json::json!({
                "content": chunk,
                "username": username,
                "allowed_mentions": { "parse": [] },
            });
            match self.send_json(url, &body).await {
                Outcome::Delivered => {}
                dropped => return dropped,
            }
        }
        Outcome::Delivered
    }

    /// Post an embeds payload and capture the created Discord message id.
    ///
    /// `embeds` is the JSON array produced by [`crate::render::embed`]. Uses
    /// `?wait=true` so Discord returns the created message JSON, from which the
    /// `id` is parsed.
    pub async fn post_embed(
        &self,
        url: &WebhookUrl,
        username: &str,
        embeds: &serde_json::Value,
    ) -> PostResult {
        let body = serde_json::json!({
            "embeds": embeds,
            "username": username,
            "allowed_mentions": { "parse": [] },
        });
        let target = format!("{}?wait=true", url.0);
        match self.send_json_capture(&target, &body).await {
            Ok(text) => match parse_message_id(&text) {
                Some(id) => PostResult::Delivered { discord_msg_id: id },
                None => PostResult::Dropped {
                    reason: format!("no message id in response: {text}"),
                },
            },
            Err(reason) => PostResult::Dropped { reason },
        }
    }

    /// PATCH an already-posted embed message in place.
    ///
    /// `PATCH {webhook}/messages/{discord_msg_id}` with a new `embeds` array.
    pub async fn patch_embed(
        &self,
        url: &WebhookUrl,
        discord_msg_id: &str,
        embeds: serde_json::Value,
    ) -> Outcome {
        let target = format!("{}/messages/{discord_msg_id}", url.0);
        let body = serde_json::json!({ "embeds": embeds, "allowed_mentions": { "parse": [] } });
        let resp = self.http.patch(&target).json(&body).send().await;
        match resp {
            Ok(r) if r.status().is_success() => Outcome::Delivered,
            Ok(r) if r.status().as_u16() == 429 => {
                let wait = retry_after_secs(&r).unwrap_or(1.0);
                tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                match self.http.patch(&target).json(&body).send().await {
                    Ok(r) if r.status().is_success() => Outcome::Delivered,
                    Ok(r) => Outcome::Dropped {
                        reason: format!("patch failed after 429 retry: {}", r.status()),
                    },
                    Err(e) => Outcome::Dropped {
                        reason: format!("patch network: {}", e.without_url()),
                    },
                }
            }
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                Outcome::Dropped {
                    reason: format!("patch failed: {status}: {text}"),
                }
            }
            Err(e) => Outcome::Dropped {
                reason: format!("patch network: {}", e.without_url()),
            },
        }
    }

    /// PATCH the webhook resource itself to set its persistent `name` and/or
    /// `avatar` (a base64 `data:` URI). Discord stores these on the webhook, so
    /// every subsequent post shows the channel's name + photo without per-message
    /// overrides.
    ///
    /// Best-effort: returns an [`Outcome`] the caller logs but never fails on —
    /// avatar syncing must not block relaying. A single 429 is honored once; any
    /// other non-success is a drop. The drop reason is URL-stripped so a webhook
    /// token can never leak (same guarantee as every other method here).
    pub async fn patch_webhook(
        &self,
        url: &WebhookUrl,
        name: Option<&str>,
        avatar_data_uri: Option<&str>,
    ) -> Outcome {
        let mut body = serde_json::Map::new();
        if let Some(n) = name {
            body.insert("name".to_string(), serde_json::json!(n));
        }
        if let Some(a) = avatar_data_uri {
            body.insert("avatar".to_string(), serde_json::json!(a));
        }
        if body.is_empty() {
            return Outcome::Dropped {
                reason: "patch_webhook called with nothing to set".to_string(),
            };
        }
        let body = serde_json::Value::Object(body);

        let resp = self.http.patch(&url.0).json(&body).send().await;
        match resp {
            Ok(r) if r.status().is_success() => Outcome::Delivered,
            Ok(r) if r.status().as_u16() == 429 => {
                let wait = retry_after_secs(&r).unwrap_or(1.0);
                tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                match self.http.patch(&url.0).json(&body).send().await {
                    Ok(r) if r.status().is_success() => Outcome::Delivered,
                    Ok(r) => Outcome::Dropped {
                        reason: format!("webhook patch failed after 429 retry: {}", r.status()),
                    },
                    Err(e) => Outcome::Dropped {
                        reason: format!("webhook patch network: {}", e.without_url()),
                    },
                }
            }
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                Outcome::Dropped {
                    reason: format!("webhook patch failed: {status}: {text}"),
                }
            }
            Err(e) => Outcome::Dropped {
                reason: format!("webhook patch network: {}", e.without_url()),
            },
        }
    }

    pub async fn post_file(
        &self,
        url: &WebhookUrl,
        username: &str,
        filename: &str,
        bytes: Vec<u8>,
    ) -> Outcome {
        let part = reqwest::multipart::Part::bytes(bytes).file_name(filename.to_string());
        let payload =
            serde_json::json!({ "username": username, "allowed_mentions": { "parse": [] } })
                .to_string();
        let form = reqwest::multipart::Form::new()
            .part("files[0]", part)
            .text("payload_json", payload);
        // multipart Form is not clonable, so it cannot be re-sent after
        // consuming it in a failed request; this is a single best-effort
        // attempt with no retry.
        self.send_once(url, form).await
    }

    async fn send_json(&self, url: &WebhookUrl, body: &serde_json::Value) -> Outcome {
        let target = format!("{}?wait=true", url.0);
        match self.send_json_capture(&target, body).await {
            Ok(_) => Outcome::Delivered,
            Err(reason) => Outcome::Dropped { reason },
        }
    }

    /// Core POST-with-retry loop, returning the success body text on delivery or
    /// a drop reason string. Honors 429 `Retry-After` (bounded by attempt count
    /// and a cumulative-wait budget) and retries 5xx/network with backoff.
    async fn send_json_capture(
        &self,
        target: &str,
        body: &serde_json::Value,
    ) -> Result<String, String> {
        let backoffs = [
            Duration::from_millis(250),
            Duration::from_secs(1),
            Duration::from_secs(4),
        ];
        let mut attempt = 0;
        let mut rate_limit_attempts = 0;
        let mut rate_limit_cumulative_wait = 0.0;
        loop {
            let resp = self.http.post(target).json(body).send().await;
            match resp {
                Ok(r) if r.status().is_success() => {
                    return Ok(r.text().await.unwrap_or_default());
                }
                Ok(r) if r.status().as_u16() == 429 => {
                    rate_limit_attempts += 1;
                    if rate_limit_attempts >= MAX_429_ATTEMPTS {
                        return Err(format!("rate limited beyond {MAX_429_ATTEMPTS} attempts"));
                    }
                    let wait = retry_after_secs(&r).unwrap_or(1.0);
                    if rate_limit_cumulative_wait + wait > RATE_LIMIT_BUDGET_SECS {
                        return Err(format!(
                            "rate limited beyond {RATE_LIMIT_BUDGET_SECS:.1}s budget"
                        ));
                    }
                    warn!(wait, "discord 429; honoring retry_after");
                    rate_limit_cumulative_wait += wait;
                    tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                    // 429 does not consume a backoff attempt
                }
                Ok(r) if r.status().is_server_error() => {
                    if attempt >= backoffs.len() {
                        return Err(format!("5xx after retries: {}", r.status()));
                    }
                    tokio::time::sleep(backoffs[attempt]).await;
                    attempt += 1;
                }
                Ok(r) => {
                    let status = r.status();
                    let text = r.text().await.unwrap_or_default();
                    return Err(format!("{status}: {text}"));
                }
                Err(e) => {
                    if attempt >= backoffs.len() {
                        return Err(format!("network: {}", e.without_url()));
                    }
                    tokio::time::sleep(backoffs[attempt]).await;
                    attempt += 1;
                }
            }
        }
    }

    async fn send_once(&self, url: &WebhookUrl, form: reqwest::multipart::Form) -> Outcome {
        let target = format!("{}?wait=true", url.0);
        let resp = self.http.post(&target).multipart(form).send().await;
        match resp {
            Ok(r) if r.status().is_success() => Outcome::Delivered,
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                Outcome::Dropped {
                    reason: format!("upload failed: {status}: {text}"),
                }
            }
            Err(e) => Outcome::Dropped {
                reason: format!("upload network: {}", e.without_url()),
            },
        }
    }

    /// Post a rich embed with one or more attached files as a SINGLE Discord
    /// message, capturing the created message id (via `?wait=true`).
    ///
    /// This is how relayed media is delivered: the caption/author/stats live in
    /// `embeds` (built by [`crate::render::embed`]) and every album file rides
    /// along as `files[N]` in the same multipart request, so an album is one
    /// message rather than N. `allowed_mentions.parse: []` suppresses pings from
    /// any mention text in the embed. Errors never include the webhook URL/token.
    pub async fn post_media_embed(
        &self,
        url: &WebhookUrl,
        username: &str,
        embeds: &serde_json::Value,
        files: Vec<(String, Vec<u8>)>,
    ) -> PostResult {
        let target = format!("{}?wait=true", url.0);
        let payload = serde_json::json!({
            "username": username,
            "embeds": embeds,
            "allowed_mentions": { "parse": [] },
        })
        .to_string();
        // multipart Form is not clonable, so it is built fresh here (single
        // attempt); the bytes are owned so a retry loop could rebuild it later.
        let mut form = reqwest::multipart::Form::new().text("payload_json", payload);
        for (i, (filename, bytes)) in files.into_iter().enumerate() {
            let part = reqwest::multipart::Part::bytes(bytes).file_name(filename);
            form = form.part(format!("files[{i}]"), part);
        }
        match self.http.post(&target).multipart(form).send().await {
            Ok(r) if r.status().is_success() => {
                let text = r.text().await.unwrap_or_default();
                match parse_message_id(&text) {
                    Some(id) => PostResult::Delivered { discord_msg_id: id },
                    None => PostResult::Dropped {
                        reason: format!("no message id in media response: {text}"),
                    },
                }
            }
            Ok(r) => {
                let status = r.status();
                let text = r.text().await.unwrap_or_default();
                PostResult::Dropped {
                    reason: format!("media upload failed: {status}: {text}"),
                }
            }
            Err(e) => PostResult::Dropped {
                reason: format!("media upload network: {}", e.without_url()),
            },
        }
    }
}

/// Parse the `id` field from a Discord message JSON response.
fn parse_message_id(text: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()?
        .get("id")?
        .as_str()
        .map(|s| s.to_string())
}

fn retry_after_secs(r: &reqwest::Response) -> Option<f64> {
    r.headers()
        .get("Retry-After")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<f64>().ok())
}
