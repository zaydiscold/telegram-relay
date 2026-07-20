#![allow(dead_code)]

use crate::config::WebhookUrl;
use std::time::Duration;
use tracing::warn;

pub struct Deliverer {
    http: reqwest::Client,
}

#[derive(Debug)]
pub enum Outcome {
    Delivered,
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
        let backoffs = [
            Duration::from_millis(250),
            Duration::from_secs(1),
            Duration::from_secs(4),
        ];
        let mut attempt = 0;
        loop {
            let resp = self.http.post(&target).json(body).send().await;
            match resp {
                Ok(r) if r.status().is_success() => return Outcome::Delivered,
                Ok(r) if r.status().as_u16() == 429 => {
                    let wait = retry_after_secs(&r).unwrap_or(1.0);
                    warn!(wait, "discord 429; honoring retry_after");
                    tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                    // 429 does not consume a backoff attempt
                }
                Ok(r) if r.status().is_server_error() => {
                    if attempt >= backoffs.len() {
                        return Outcome::Dropped {
                            reason: format!("5xx after retries: {}", r.status()),
                        };
                    }
                    tokio::time::sleep(backoffs[attempt]).await;
                    attempt += 1;
                }
                Ok(r) => {
                    let status = r.status();
                    let text = r.text().await.unwrap_or_default();
                    return Outcome::Dropped {
                        reason: format!("{status}: {text}"),
                    };
                }
                Err(e) => {
                    if attempt >= backoffs.len() {
                        return Outcome::Dropped {
                            reason: format!("network: {e}"),
                        };
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

fn retry_after_secs(r: &reqwest::Response) -> Option<f64> {
    r.headers()
        .get("Retry-After")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<f64>().ok())
}
