use axum::{extract::State, http::StatusCode, routing::post, Router};
use std::sync::{Arc, Mutex};
use telegram_relay::config::WebhookUrl;
use telegram_relay::deliver::{Deliverer, Outcome};

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
