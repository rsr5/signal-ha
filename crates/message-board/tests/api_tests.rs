use std::sync::Arc;

use axum::body::Body;
use axum::Router;
use http_body_util::BodyExt;
use hyper::Request;
use serde_json::{json, Value};
use tower::ServiceExt;

use message_board::db::{Pool, Post, Reply};
use message_board::routes;

fn test_app() -> Router {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("test.db");
    let path_str = path.to_str().unwrap().to_string();
    // Leak so SQLite file survives the test
    std::mem::forget(dir);

    let pool = Pool::open(&path_str).unwrap();
    pool.migrate().unwrap();
    let state = Arc::new(pool);
    Router::new().merge(routes::router()).with_state(state)
}

async fn json_body(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn body_as<T: serde::de::DeserializeOwned>(resp: axum::response::Response) -> T {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn post_request(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

fn patch_request(uri: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("PATCH")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

fn get_request(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

/// Helper: send request to a cloneable app
async fn send(app: &Router, req: Request<Body>) -> axum::response::Response {
    app.clone().oneshot(req).await.unwrap()
}

// ── POST /posts ────────────────────────────────────────────────────

#[tokio::test]
async fn create_post_returns_201() {
    let app = test_app();
    let resp = send(&app, post_request(
        "/posts",
        json!({"agent": "porch-lights", "body": "Lux sensor offline"}),
    )).await;

    assert_eq!(resp.status(), 201);
    let body = json_body(resp).await;
    assert_eq!(body["agent"], "porch-lights");
    assert_eq!(body["body"], "Lux sensor offline");
    assert_eq!(body["active"], true);
    assert_eq!(body["id"], 1);
}

// ── GET /posts ─────────────────────────────────────────────────────

#[tokio::test]
async fn list_posts_empty() {
    let app = test_app();
    let resp = send(&app, get_request("/posts")).await;

    assert_eq!(resp.status(), 200);
    let body = json_body(resp).await;
    assert_eq!(body, json!([]));
}

#[tokio::test]
async fn list_posts_with_filter() {
    let app = test_app();

    send(&app, post_request("/posts", json!({"agent": "porch", "body": "one"}))).await;
    send(&app, post_request("/posts", json!({"agent": "garage", "body": "two"}))).await;
    send(&app, post_request("/posts", json!({"agent": "porch", "body": "three"}))).await;

    // Filter by agent
    let posts: Vec<Post> = body_as(send(&app, get_request("/posts?agent=porch")).await).await;
    assert_eq!(posts.len(), 2);
    assert!(posts.iter().all(|p| p.agent == "porch"));

    // Filter by active
    let posts: Vec<Post> = body_as(send(&app, get_request("/posts?active=true")).await).await;
    assert_eq!(posts.len(), 3);

    // Combined filter
    let posts: Vec<Post> = body_as(
        send(&app, get_request("/posts?agent=garage&active=true")).await,
    ).await;
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].agent, "garage");
}

// ── GET /posts/:id ─────────────────────────────────────────────────

#[tokio::test]
async fn get_post_includes_replies() {
    let app = test_app();

    send(&app, post_request("/posts", json!({"agent": "porch", "body": "issue"}))).await;
    send(&app, post_request("/posts/1/replies", json!({"author": "user", "body": "I fixed it"}))).await;
    send(&app, post_request("/posts/1/replies", json!({"author": "porch-agent", "body": "Confirmed fixed"}))).await;

    let resp = send(&app, get_request("/posts/1")).await;
    assert_eq!(resp.status(), 200);

    let post: Post = body_as(resp).await;
    assert_eq!(post.body, "issue");
    let replies = post.replies.unwrap();
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0].author, "user");
    assert_eq!(replies[1].author, "porch-agent");
}

#[tokio::test]
async fn get_nonexistent_post_returns_404() {
    let app = test_app();
    let resp = send(&app, get_request("/posts/999")).await;
    assert_eq!(resp.status(), 404);
}

// ── PATCH /posts/:id ───────────────────────────────────────────────

#[tokio::test]
async fn close_post() {
    let app = test_app();

    send(&app, post_request("/posts", json!({"agent": "garage", "body": "motion stuck"}))).await;

    let resp = send(&app, patch_request("/posts/1", json!({"active": false}))).await;
    assert_eq!(resp.status(), 200);

    let post: Post = body_as(resp).await;
    assert!(!post.active);

    // Verify it's gone from active list
    let posts: Vec<Post> = body_as(send(&app, get_request("/posts?active=true")).await).await;
    assert_eq!(posts.len(), 0);
}

#[tokio::test]
async fn update_post_body() {
    let app = test_app();

    send(&app, post_request("/posts", json!({"agent": "office", "body": "original"}))).await;

    let resp = send(&app, patch_request("/posts/1", json!({"body": "amended finding"}))).await;
    assert_eq!(resp.status(), 200);

    let post: Post = body_as(resp).await;
    assert_eq!(post.body, "amended finding");
    assert!(post.active);
}

#[tokio::test]
async fn patch_nonexistent_post_returns_404() {
    let app = test_app();
    let resp = send(&app, patch_request("/posts/999", json!({"active": false}))).await;
    assert_eq!(resp.status(), 404);
}

// ── POST /posts/:id/replies ────────────────────────────────────────

#[tokio::test]
async fn create_reply_returns_201() {
    let app = test_app();

    send(&app, post_request("/posts", json!({"agent": "porch", "body": "question"}))).await;

    let resp = send(&app, post_request(
        "/posts/1/replies",
        json!({"author": "user", "body": "here's the answer"}),
    )).await;
    assert_eq!(resp.status(), 201);

    let reply: Reply = body_as(resp).await;
    assert_eq!(reply.author, "user");
    assert_eq!(reply.body, "here's the answer");
    assert_eq!(reply.post_id, 1);
}

#[tokio::test]
async fn reply_to_nonexistent_post_returns_404() {
    let app = test_app();
    let resp = send(&app, post_request(
        "/posts/999/replies",
        json!({"author": "user", "body": "hello"}),
    )).await;
    assert_eq!(resp.status(), 404);
}

// ── Full workflow ──────────────────────────────────────────────────

#[tokio::test]
async fn full_agent_user_lifecycle() {
    let app = test_app();

    // 1. Agent creates three posts
    for body in ["Frigate offline 26h", "Suggest lux hysteresis", "Is face light plug still used?"] {
        send(&app, post_request("/posts", json!({"agent": "garage-agent", "body": body}))).await;
    }

    // 2. User reads active posts
    let posts: Vec<Post> = body_as(send(&app, get_request("/posts?active=true")).await).await;
    assert_eq!(posts.len(), 3);

    // 3. User replies to the question
    send(&app, post_request(
        "/posts/3/replies",
        json!({"author": "user", "body": "No, it's been decommissioned"}),
    )).await;

    // 4. User closes the Frigate issue (they fixed it)
    send(&app, post_request(
        "/posts/1/replies",
        json!({"author": "user", "body": "Restarted Frigate container"}),
    )).await;
    send(&app, patch_request("/posts/1", json!({"active": false}))).await;

    // 5. Next agent session — reads open posts
    let open: Vec<Post> = body_as(
        send(&app, get_request("/posts?agent=garage-agent&active=true")).await,
    ).await;
    assert_eq!(open.len(), 2);

    // 6. Agent reads the question post and sees the answer
    let post: Post = body_as(send(&app, get_request("/posts/3")).await).await;
    let replies = post.replies.unwrap();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].body, "No, it's been decommissioned");

    // 7. Agent replies acknowledging
    send(&app, post_request(
        "/posts/3/replies",
        json!({"author": "garage-agent", "body": "Understood, will ignore face light plug going forward"}),
    )).await;

    // 8. User closes the question
    send(&app, patch_request("/posts/3", json!({"active": false}))).await;

    // 9. Only the recommendation remains
    let remaining: Vec<Post> = body_as(
        send(&app, get_request("/posts?agent=garage-agent&active=true")).await,
    ).await;
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].body, "Suggest lux hysteresis");
}

// ── Bad requests ───────────────────────────────────────────────────

#[tokio::test]
async fn create_post_missing_fields_returns_422() {
    let app = test_app();
    let resp = send(&app, post_request("/posts", json!({"agent": "porch"}))).await;
    assert_eq!(resp.status(), 422);
}

#[tokio::test]
async fn create_reply_missing_fields_returns_422() {
    let app = test_app();
    send(&app, post_request("/posts", json!({"agent": "a", "body": "b"}))).await;

    let resp = send(&app, post_request(
        "/posts/1/replies",
        json!({"author": "user"}),
    )).await;
    assert_eq!(resp.status(), 422);
}
