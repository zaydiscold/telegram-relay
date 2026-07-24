use axum::{extract::State, http::StatusCode, routing::patch, routing::post, Router};
use std::sync::{Arc, Mutex};
use telegram_relay::config::WebhookUrl;
use telegram_relay::deliver::{Deliverer, Outcome, PostResult};

#[derive(Clone, Default)]
struct Hits(Arc<Mutex<Vec<String>>>);

async fn ok_handler(State(h): State<Hits>, body: String) -> StatusCode {
    h.0.lock().unwrap().push(body);
    StatusCode::OK
}

async fn ratelimit_then_ok(
    State(h): State<Hits>,
    body: String,
) -> (StatusCode, [(&'static str, &'static str); 1], String) {
    let mut g = h.0.lock().unwrap();
    g.push(body);
    if g.len() == 1 {
        (
            StatusCode::TOO_MANY_REQUESTS,
            [("Retry-After", "0")],
            r#"{"retry_after": 0.05}"#.into(),
        )
    } else {
        (StatusCode::OK, [("Retry-After", "0")], "{}".into())
    }
}

async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/hook")
}

#[tokio::test]
async fn posts_chunks_in_order() {
    let hits = Hits::default();
    let url = spawn(
        Router::new()
            .route("/hook", post(ok_handler))
            .with_state(hits.clone()),
    )
    .await;
    let d = Deliverer::new();
    let out = d
        .post_text(&WebhookUrl(url), "Rob", &["one".into(), "two".into()])
        .await;
    assert!(matches!(out, Outcome::Delivered));
    let bodies = hits.0.lock().unwrap();
    assert_eq!(bodies.len(), 2);
    assert!(bodies[0].contains("one") && bodies[1].contains("two"));
}

async fn always_ratelimit(
    State(h): State<Hits>,
    body: String,
) -> (StatusCode, [(&'static str, &'static str); 1], String) {
    h.0.lock().unwrap().push(body);
    (
        StatusCode::TOO_MANY_REQUESTS,
        [("Retry-After", "0")],
        "{}".into(),
    )
}

#[tokio::test]
async fn retries_on_429() {
    let hits = Hits::default();
    let url = spawn(
        Router::new()
            .route("/hook", post(ratelimit_then_ok))
            .with_state(hits.clone()),
    )
    .await;
    let d = Deliverer::new();
    let out = d.post_text(&WebhookUrl(url), "Rob", &["x".into()]).await;
    assert!(matches!(out, Outcome::Delivered));
    assert_eq!(hits.0.lock().unwrap().len(), 2); // 429 then 200
}

#[tokio::test]
async fn gives_up_after_429_budget() {
    let hits = Hits::default();
    let url = spawn(
        Router::new()
            .route("/hook", post(always_ratelimit))
            .with_state(hits.clone()),
    )
    .await;
    let d = Deliverer::new();
    let out = d.post_text(&WebhookUrl(url), "Rob", &["x".into()]).await;
    // Should be Dropped with rate limit reason
    assert!(matches!(out, Outcome::Dropped { reason: _ }));
    // Should have exactly 20 requests (the attempt limit)
    assert_eq!(hits.0.lock().unwrap().len(), 20);
}

async fn embed_ok_handler(State(h): State<Hits>, body: String) -> (StatusCode, String) {
    h.0.lock().unwrap().push(body);
    (StatusCode::OK, r#"{"id": "998877665544332211"}"#.into())
}

#[tokio::test]
async fn post_embed_captures_message_id() {
    let hits = Hits::default();
    let url = spawn(
        Router::new()
            .route("/hook", post(embed_ok_handler))
            .with_state(hits.clone()),
    )
    .await;
    let d = Deliverer::new();
    let embeds = serde_json::json!([{ "description": "hi" }]);
    let out = d.post_embed(&WebhookUrl(url), "Rob", &embeds).await;
    match out {
        PostResult::Delivered { discord_msg_id, .. } => {
            assert_eq!(discord_msg_id, "998877665544332211");
        }
        PostResult::Dropped { reason } => panic!("expected delivered, got drop: {reason}"),
    }
    let bodies = hits.0.lock().unwrap();
    assert_eq!(bodies.len(), 1);
    assert!(bodies[0].contains("\"embeds\""));
    assert!(bodies[0].contains("\"username\":\"Rob\""));
}

#[tokio::test]
async fn post_text_suppresses_mentions() {
    let hits = Hits::default();
    let url = spawn(
        Router::new()
            .route("/hook", post(ok_handler))
            .with_state(hits.clone()),
    )
    .await;
    let d = Deliverer::new();
    let out = d
        .post_text(&WebhookUrl(url), "Rob", &["@everyone gm".into()])
        .await;
    assert!(matches!(out, Outcome::Delivered));
    let bodies = hits.0.lock().unwrap();
    let v: serde_json::Value = serde_json::from_str(&bodies[0]).unwrap();
    // parse:[] is the load-bearing guard: it renders @everyone as plain text but
    // suppresses the actual ping.
    assert_eq!(v["allowed_mentions"]["parse"], serde_json::json!([]));
}

async fn media_ok_handler(State(h): State<Hits>, body: String) -> (StatusCode, String) {
    h.0.lock().unwrap().push(body);
    (StatusCode::OK, r#"{"id": "424242"}"#.into())
}

#[tokio::test]
async fn post_media_embed_uploads_files_with_embed_and_suppresses_mentions() {
    let hits = Hits::default();
    let url = spawn(
        Router::new()
            .route("/hook", post(media_ok_handler))
            .with_state(hits.clone()),
    )
    .await;
    let d = Deliverer::new();
    let embeds = serde_json::json!([{ "description": "album caption" }]);
    // UTF-8 "bytes" so the multipart body stays a valid String for the extractor.
    let files = vec![
        ("photo_1.jpg".to_string(), b"AAAAA".to_vec()),
        ("photo_2.jpg".to_string(), b"BBBBB".to_vec()),
    ];
    let out = d
        .post_media_embed(&WebhookUrl(url), "Rob", &embeds, files)
        .await;
    match out {
        PostResult::Delivered { discord_msg_id, .. } => assert_eq!(discord_msg_id, "424242"),
        PostResult::Dropped { reason } => panic!("expected delivered, got drop: {reason}"),
    }
    let bodies = hits.0.lock().unwrap();
    assert_eq!(bodies.len(), 1);
    let body = &bodies[0];
    assert!(body.contains("payload_json"));
    assert!(body.contains("allowed_mentions"));
    assert!(body.contains("album caption"));
    // Both files present, as distinct multipart parts with their filenames+bytes.
    assert!(body.contains("files[0]") && body.contains("files[1]"));
    assert!(body.contains("photo_1.jpg") && body.contains("photo_2.jpg"));
    assert!(body.contains("AAAAA") && body.contains("BBBBB"));
}

#[tokio::test]
async fn network_error_reason_never_leaks_webhook_token() {
    // Point at a closed port with a token-bearing URL; the connect error must
    // not carry the token into the drop reason (reqwest Error::without_url).
    let d = Deliverer::new();
    let url = WebhookUrl("http://127.0.0.1:1/api/webhooks/123456789/SUPERSECRETTOKEN".into());
    let out = d.post_text(&url, "Rob", &["x".into()]).await;
    match out {
        Outcome::Dropped { reason } => {
            assert!(
                !reason.contains("SUPERSECRETTOKEN"),
                "drop reason leaked the webhook token: {reason}"
            );
        }
        Outcome::Delivered => panic!("expected a network failure against a closed port"),
    }
}

async fn patch_handler(State(h): State<Hits>, body: String) -> StatusCode {
    h.0.lock().unwrap().push(body);
    StatusCode::OK
}

#[tokio::test]
async fn patch_webhook_sets_name_and_avatar() {
    let hits = Hits::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // PATCH is on the webhook resource itself (no /messages/{id} suffix).
    let app = Router::new()
        .route("/hook", patch(patch_handler))
        .with_state(hits.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let url = format!("http://{addr}/hook");

    let d = Deliverer::new();
    let out = d
        .patch_webhook(
            &WebhookUrl(url),
            Some("Rob's Channel"),
            Some("data:image/jpeg;base64,SGk="),
        )
        .await;
    assert!(matches!(out, Outcome::Delivered));
    let bodies = hits.0.lock().unwrap();
    assert_eq!(bodies.len(), 1);
    let v: serde_json::Value = serde_json::from_str(&bodies[0]).unwrap();
    assert_eq!(v["name"], "Rob's Channel");
    assert_eq!(v["avatar"], "data:image/jpeg;base64,SGk=");
}

#[tokio::test]
async fn patch_webhook_name_only_omits_avatar() {
    let hits = Hits::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/hook", patch(patch_handler))
        .with_state(hits.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let url = format!("http://{addr}/hook");

    let d = Deliverer::new();
    let out = d
        .patch_webhook(&WebhookUrl(url), Some("Nameless Chan"), None)
        .await;
    assert!(matches!(out, Outcome::Delivered));
    let bodies = hits.0.lock().unwrap();
    let v: serde_json::Value = serde_json::from_str(&bodies[0]).unwrap();
    assert_eq!(v["name"], "Nameless Chan");
    assert!(
        v.get("avatar").is_none(),
        "avatar should be omitted, not null"
    );
}

#[tokio::test]
async fn patch_webhook_error_reason_never_leaks_token() {
    // Closed port + token-bearing URL: the drop reason must not carry the token.
    let d = Deliverer::new();
    let url = WebhookUrl("http://127.0.0.1:1/api/webhooks/123456789/SUPERSECRETTOKEN".into());
    let out = d
        .patch_webhook(&url, Some("x"), Some("data:image/jpeg;base64,AA=="))
        .await;
    match out {
        Outcome::Dropped { reason } => assert!(
            !reason.contains("SUPERSECRETTOKEN"),
            "webhook patch drop reason leaked the token: {reason}"
        ),
        Outcome::Delivered => panic!("expected a network failure against a closed port"),
    }
}

#[tokio::test]
async fn patch_embed_hits_messages_endpoint() {
    let hits = Hits::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/hook/messages/{id}", patch(patch_handler))
        .with_state(hits.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let url = format!("http://{addr}/hook");

    let d = Deliverer::new();
    let embeds = serde_json::json!([{ "description": "edited" }]);
    let out = d.patch_embed(&WebhookUrl(url), "12345", embeds, &[]).await;
    assert!(matches!(out, Outcome::Delivered));
    let bodies = hits.0.lock().unwrap();
    assert_eq!(bodies.len(), 1);
    assert!(bodies[0].contains("edited"));
}
