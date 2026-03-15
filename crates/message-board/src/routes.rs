use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::db::Pool;

type AppState = Arc<Pool>;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/posts", get(list_posts).post(create_post))
        .route("/posts/{id}", get(get_post).patch(update_post))
        .route("/posts/{id}/replies", post(create_reply))
}

// ── Query / body structs ───────────────────────────────────────────

#[derive(Deserialize)]
struct ListQuery {
    agent: Option<String>,
    active: Option<bool>,
}

#[derive(Deserialize)]
struct CreatePostBody {
    agent: String,
    body: String,
}

#[derive(Deserialize)]
struct UpdatePostBody {
    active: Option<bool>,
    body: Option<String>,
}

#[derive(Deserialize)]
struct CreateReplyBody {
    author: String,
    body: String,
}

// ── Handlers ───────────────────────────────────────────────────────

async fn list_posts(
    State(pool): State<AppState>,
    Query(q): Query<ListQuery>,
) -> impl IntoResponse {
    match pool.list_posts(q.agent.as_deref(), q.active) {
        Ok(posts) => Json(posts).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn create_post(
    State(pool): State<AppState>,
    Json(body): Json<CreatePostBody>,
) -> impl IntoResponse {
    match pool.create_post(&body.agent, &body.body) {
        Ok(post) => (StatusCode::CREATED, Json(post)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn get_post(
    State(pool): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    match pool.get_post(id) {
        Ok(post) => Json(post).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn update_post(
    State(pool): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdatePostBody>,
) -> impl IntoResponse {
    match pool.update_post(id, body.active, body.body.as_deref()) {
        Ok(post) => Json(post).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn create_reply(
    State(pool): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<CreateReplyBody>,
) -> impl IntoResponse {
    match pool.create_reply(id, &body.author, &body.body) {
        Ok(reply) => (StatusCode::CREATED, Json(reply)).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}
