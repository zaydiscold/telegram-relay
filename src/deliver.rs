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
/// refresh worker can later PATCH it, plus the Discord CDN URLs of any attached
/// images (so a later PATCH can re-reference them — a PATCH sends no attachments,
/// so an `attachment://` reference would otherwise be dropped).
#[derive(Debug)]
pub enum PostResult {
    Delivered {
        discord_msg_id: String,
        image_urls: Vec<String>,
    },
    Dropped {
        reason: String,
    },
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
        content: Option<&str>,
    ) -> PostResult {
        let mut body = serde_json::json!({
            "embeds": embeds,
            "username": username,
            "allowed_mentions": { "parse": [] },
        });
        // Plain content outside the embed (e.g. contract addresses) so bots that
        // scan message text can act on it. `allowed_mentions.parse: []` still
        // suppresses pings from anything in it.
        if let Some(c) = content {
            body["content"] = serde_json::json!(c);
        }
        let target = format!("{}?wait=true", url.0);
        match self.send_json_capture(&target, &body).await {
            Ok(text) => match parse_message_id(&text) {
                Some(id) => PostResult::Delivered {
                    discord_msg_id: id,
                    image_urls: Vec::new(),
                },
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
    ///
    /// `keep_attachments` is the `(id, filename)` of every image the message
    /// should retain. On an edit, Discord DELETES any existing attachment not
    /// listed in the `attachments` array — so an empty array wipes the media
    /// post's image (its embed url then 404s and the post goes blank). Listing
    /// the existing attachment by id keeps the file; the embed references it via
    /// `attachment://filename`. Text posts pass an empty slice.
    pub async fn patch_embed(
        &self,
        url: &WebhookUrl,
        discord_msg_id: &str,
        embeds: serde_json::Value,
        keep_attachments: &[(String, String)],
    ) -> Outcome {
        let target = format!("{}/messages/{discord_msg_id}", url.0);
        let attachments: Vec<serde_json::Value> = keep_attachments
            .iter()
            .map(|(id, filename)| serde_json::json!({ "id": id, "filename": filename }))
            .collect();
        let body = serde_json::json!({
            "embeds": embeds,
            "allowed_mentions": { "parse": [] },
            "attachments": attachments,
        });
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
        content: Option<&str>,
    ) -> PostResult {
        let target = format!("{}?wait=true", url.0);
        // Discord (v10) only registers uploaded files as attachments if
        // payload_json DECLARES them in an `attachments` array whose `id` matches
        // the `files[N]` part index. Without this, the files are silently ignored
        // (`attachments: []` in the response) and any `attachment://` embed image
        // shows nothing. Declaring them here is what makes the inline image work.
        let attachment_meta: Vec<serde_json::Value> = files
            .iter()
            .enumerate()
            .map(|(i, (filename, _))| serde_json::json!({ "id": i, "filename": filename }))
            .collect();
        let mut payload_val = serde_json::json!({
            "username": username,
            "embeds": embeds,
            "allowed_mentions": { "parse": [] },
            "attachments": attachment_meta,
        });
        // Plain content outside the embed (e.g. contract addresses) so scanning
        // bots can act on it, same as the text path.
        if let Some(c) = content {
            payload_val["content"] = serde_json::json!(c);
        }
        let payload = payload_val.to_string();
        // multipart Form is not clonable, so rebuild it from the owned bytes on
        // each attempt. Media deserves the same resilience as text posts: a
        // single transient 429/5xx must not silently drop a whole album (the old
        // single-attempt path was how media went missing under load).
        let build_form = || {
            let mut form = reqwest::multipart::Form::new().text("payload_json", payload.clone());
            for (i, (filename, bytes)) in files.iter().enumerate() {
                let part =
                    reqwest::multipart::Part::bytes(bytes.clone()).file_name(filename.clone());
                form = form.part(format!("files[{i}]"), part);
            }
            form
        };
        let backoffs = [
            Duration::from_millis(250),
            Duration::from_secs(1),
            Duration::from_secs(4),
        ];
        let mut attempt = 0;
        let mut rate_limit_attempts = 0;
        let mut rate_limit_cumulative_wait = 0.0;
        loop {
            match self.http.post(&target).multipart(build_form()).send().await {
                Ok(r) if r.status().is_success() => {
                    let text = r.text().await.unwrap_or_default();
                    return match parse_message_id(&text) {
                        Some(id) => PostResult::Delivered {
                            discord_msg_id: id,
                            image_urls: parse_delivered_media_urls(&text),
                        },
                        None => PostResult::Dropped {
                            reason: format!("no message id in media response: {text}"),
                        },
                    };
                }
                Ok(r) if r.status().as_u16() == 429 => {
                    rate_limit_attempts += 1;
                    if rate_limit_attempts >= MAX_429_ATTEMPTS {
                        return PostResult::Dropped {
                            reason: format!(
                                "media rate limited beyond {MAX_429_ATTEMPTS} attempts"
                            ),
                        };
                    }
                    let wait = retry_after_secs(&r).unwrap_or(1.0);
                    if rate_limit_cumulative_wait + wait > RATE_LIMIT_BUDGET_SECS {
                        return PostResult::Dropped {
                            reason: format!(
                                "media rate limited beyond {RATE_LIMIT_BUDGET_SECS:.1}s budget"
                            ),
                        };
                    }
                    warn!(wait, "discord 429 on media; honoring retry_after");
                    rate_limit_cumulative_wait += wait;
                    tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                }
                Ok(r) if r.status().is_server_error() => {
                    if attempt >= backoffs.len() {
                        return PostResult::Dropped {
                            reason: format!("media 5xx after retries: {}", r.status()),
                        };
                    }
                    tokio::time::sleep(backoffs[attempt]).await;
                    attempt += 1;
                }
                Ok(r) => {
                    let status = r.status();
                    let text = r.text().await.unwrap_or_default();
                    return PostResult::Dropped {
                        reason: format!("media upload failed: {status}: {text}"),
                    };
                }
                Err(e) => {
                    if attempt >= backoffs.len() {
                        return PostResult::Dropped {
                            reason: format!("media upload network: {}", e.without_url()),
                        };
                    }
                    tokio::time::sleep(backoffs[attempt]).await;
                    attempt += 1;
                }
            }
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

/// Parse the resolved CDN URLs of every piece of media attached to a Discord
/// message response, so a later refresh PATCH can keep them instead of deleting
/// the files.
///
/// Two places to look, because Discord splits media by type:
///   * **images** referenced via `attachment://` are consumed into the embed —
///     Discord moves the `cdn.discordapp.com` URL into `embeds[].image.url` and
///     empties the top-level `attachments`.
///   * **video / audio / documents** can't render in an embed, so they stay in
///     the top-level `attachments[].url`.
///
/// Both are needed: capturing only the embed images (the old behavior) meant a
/// video post stored an empty list, so the first refresh PATCH sent
/// `attachments: []` and Discord deleted the video — the post went blank. The
/// returned URLs carry Discord's `(attachment_id, filename)`, which the refresh
/// path re-attaches by id (see `refresh::reattach_stored_media`). Order-preserving
/// dedup guards the theoretical case of a URL appearing in both places.
fn parse_delivered_media_urls(text: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let mut urls: Vec<String> = Vec::new();
    if let Some(embeds) = v.get("embeds").and_then(|e| e.as_array()) {
        for e in embeds {
            if let Some(u) = e
                .get("image")
                .and_then(|i| i.get("url"))
                .and_then(|u| u.as_str())
            {
                urls.push(u.to_string());
            }
        }
    }
    if let Some(atts) = v.get("attachments").and_then(|a| a.as_array()) {
        for a in atts {
            if let Some(u) = a.get("url").and_then(|u| u.as_str()) {
                urls.push(u.to_string());
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    urls.retain(|u| seen.insert(u.clone()));
    urls
}

fn retry_after_secs(r: &reqwest::Response) -> Option<f64> {
    r.headers()
        .get("Retry-After")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<f64>().ok())
}

#[cfg(test)]
mod tests {
    use super::{parse_delivered_media_urls, parse_message_id};

    #[test]
    fn extracts_cdn_image_url_from_embed() {
        // Discord empties top-level `attachments` and moves the resolved CDN url
        // into embeds[].image.url once an attachment:// reference is consumed.
        let resp = r#"{"id":"999","attachments":[],"embeds":[
            {"type":"rich","description":"x",
             "image":{"url":"https://cdn.discordapp.com/attachments/1/2/photo.jpg"}}]}"#;
        assert_eq!(parse_message_id(resp).as_deref(), Some("999"));
        assert_eq!(
            parse_delivered_media_urls(resp),
            vec!["https://cdn.discordapp.com/attachments/1/2/photo.jpg"]
        );
    }

    #[test]
    fn captures_non_image_attachment_url() {
        // A video can't render in an embed, so Discord leaves it in the top-level
        // attachments[]. It MUST be captured or the first refresh PATCH deletes it.
        let resp = r#"{"id":"7","attachments":[
            {"id":"42","filename":"clip.mp4",
             "url":"https://cdn.discordapp.com/attachments/1/42/clip.mp4"}],"embeds":[]}"#;
        assert_eq!(
            parse_delivered_media_urls(resp),
            vec!["https://cdn.discordapp.com/attachments/1/42/clip.mp4"]
        );
    }

    #[test]
    fn captures_mixed_album_image_and_video_without_dupes() {
        // Image consumed into the embed + a video left in attachments: both kept,
        // image first (gallery order), no duplication.
        let resp = r#"{"id":"8","attachments":[
            {"id":"2","filename":"v.mp4","url":"https://cdn.discordapp.com/attachments/1/2/v.mp4"}],
            "embeds":[{"image":{"url":"https://cdn.discordapp.com/attachments/1/1/p.jpg"}}]}"#;
        assert_eq!(
            parse_delivered_media_urls(resp),
            vec![
                "https://cdn.discordapp.com/attachments/1/1/p.jpg",
                "https://cdn.discordapp.com/attachments/1/2/v.mp4",
            ]
        );
    }

    #[test]
    fn no_media_yields_empty() {
        let resp = r#"{"id":"1","embeds":[{"type":"rich","description":"text only"}]}"#;
        assert!(parse_delivered_media_urls(resp).is_empty());
    }
}
