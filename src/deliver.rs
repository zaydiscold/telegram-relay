#![allow(dead_code)]

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
            let body = serde_json::json!({ "content": chunk, "username": username });
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
        let body = serde_json::json!({ "embeds": embeds, "username": username });
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
        let body = serde_json::json!({ "embeds": embeds });
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
                        reason: format!("patch network: {e}"),
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
                reason: format!("patch network: {e}"),
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
        let payload = serde_json::json!({ "username": username }).to_string();
        let form = reqwest::multipart::Form::new()
            .part("files[0]", part)
            .text("payload_json", payload);
        // multipart Form is not clonable; single attempt + one retry on 429 only
        self.send_once_with_429_retry(url, form).await
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
                        return Err(format!("rate limited beyond {} attempts", MAX_429_ATTEMPTS));
                    }
                    let wait = retry_after_secs(&r).unwrap_or(1.0);
                    if rate_limit_cumulative_wait + wait > RATE_LIMIT_BUDGET_SECS {
                        return Err(format!(
                            "rate limited beyond {:.1}s budget",
                            RATE_LIMIT_BUDGET_SECS
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
                        return Err(format!("network: {e}"));
                    }
                    tokio::time::sleep(backoffs[attempt]).await;
                    attempt += 1;
                }
            }
        }
    }

    async fn send_once_with_429_retry(
        &self,
        url: &WebhookUrl,
        form: reqwest::multipart::Form,
    ) -> Outcome {
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
                reason: format!("upload network: {e}"),
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
