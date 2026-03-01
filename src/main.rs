use axum::{extract::{Path, Query, State}, http::{header, HeaderMap, StatusCode}, response::IntoResponse, routing::{get, post}, Router, Json};
use redis::{AsyncCommands, RedisResult};
use std::{env, sync::Arc};
use serde::{Deserialize, Serialize};
use tracing::{info, error, warn, Level};

pub struct AppState {
    pub redis_client: redis::Client,
}

#[tokio::main]
async fn main() {
     tracing_subscriber::fmt()
        .with_max_level(Level::ERROR)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())// Log only errors and above
        .init();

    let redis_url = env::var("REDIS_URL").unwrap_or("redis://127.0.0.1/".to_string());
    let redis_client = redis::Client::open(redis_url).unwrap();
    let app_state = AppState { redis_client };

    if env::var("AUTH_TOKEN").is_err() {
        warn!("AUTH_TOKEN not set - admin endpoints are unprotected");
    }

    let router = Router::new()
        .route("/p/:path", get(get_page))
        .route("/pages", get(list_pages))
        .route("/create_page/:path", post(create_page))
        .with_state(Arc::new(app_state));

    let host = env::var("HOST").unwrap_or("127.0.0.1".to_string());
    let port = env::var("PORT").unwrap_or("3000".to_string());
    let bind_address = format!("{}:{}", host, port);
    info!("Listening on {}", bind_address);
    let listener = tokio::net::TcpListener::bind(bind_address)
        .await
        .unwrap();

    axum::serve(listener, router.into_make_service()).await.unwrap();
}

fn check_auth(headers: &HeaderMap) -> Result<(), StatusCode> {
    let token = match env::var("AUTH_TOKEN") {
        Ok(t) => t,
        Err(_) => return Ok(()),
    };
    let raw = headers.get("Authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let provided = raw.strip_prefix("Bearer ").unwrap_or(raw);
    if token != provided { return Err(StatusCode::UNAUTHORIZED); }
    Ok(())
}

#[derive(Serialize)]
struct CreatePageResponse {
    success: bool,
}

#[derive(Deserialize)]
struct CreatePageQuery {
    name: Option<String>,
}

async fn create_page(
    Path(path): Path<String>,
    Query(query): Query<CreatePageQuery>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<CreatePageResponse>, StatusCode> {
    check_auth(&headers)?;

    let mut redis_conn = redis_connection(&state.redis_client).await.map_err(|e| log_err("redis_connection", e))?;

    let page_key = format!("page:{}", path);
    redis_conn.hset(&page_key, "html", body.to_vec()).await.map_err(|e| log_err("0", e))?;
    redis_conn.hset(&page_key, "name", query.name.unwrap_or_default()).await.map_err(|e| log_err("2", e))?;
    redis_conn.expire(&page_key, 60 * 60 * 24 * 30).await.map_err(|e| log_err("1", e))?;

    Ok(Json(CreatePageResponse { success: true }))
}

#[derive(Serialize)]
struct PageInfo {
    path: String,
    name: String,
}

async fn list_pages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<PageInfo>>, StatusCode> {
    check_auth(&headers)?;

    let mut redis_conn = redis_connection(&state.redis_client).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut pages = Vec::new();
    let mut cursor: u64 = 0;

    loop {
        let (next_cursor, keys): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg("page:*")
            .arg("COUNT")
            .arg(100)
            .query_async(&mut redis_conn)
            .await
            .map_err(|e| log_err("scan", e))?;

        for key in keys {
            let name: String = redis_conn.hget(&key, "name").await.unwrap_or_default();
            let path = key.strip_prefix("page:").unwrap_or(&key).to_string();
            pages.push(PageInfo { path, name });
        }

        cursor = next_cursor;
        if cursor == 0 { break; }
    }

    Ok(Json(pages))
}

async fn redis_connection(redis_client: &redis::Client) -> RedisResult<redis::aio::MultiplexedConnection> {
    redis_client.get_multiplexed_async_connection().await
}

fn log_err<T: std::fmt::Display>(tag: &str, err: T) -> StatusCode {
    error!("{} - error - {}", tag, err);
    StatusCode::INTERNAL_SERVER_ERROR
}

async fn get_page(
    Path(path): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let mut redis_conn = match redis_connection(&state.redis_client).await {
        Ok(conn) => conn,
        Err(err) => {
            return (log_err("redis_connection", err),
                    [(header::CONTENT_TYPE, "text/html")],
                    "Internal error".to_string());
        }
    };

    let page_key = format!("page:{}", path);

    let page_content = match redis_conn.hget(&page_key, "html").await {
        Ok(Some(content)) => content,
        _ => return (StatusCode::NOT_FOUND,
                     [(header::CONTENT_TYPE, "text/html")],
                     "".to_string()),
    };

    (StatusCode::OK, [(header::CONTENT_TYPE, "text/html")], page_content)
}