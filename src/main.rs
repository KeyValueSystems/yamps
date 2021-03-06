use axum::{
    body,
    extract::{
        multipart::{self, Multipart},
        ConnectInfo, Path, TypedHeader,
    },
    headers::{ContentLength, HeaderMap},
    http::{
        header::{self, HeaderValue},
        StatusCode,
    },
    response::{IntoResponse, Response},
    routing::get,
};
use axum_static_macro::{content_types, static_file};
use chrono::{DateTime, Local};
use dashmap::DashMap;
use parking_lot::RwLock;
use sqlx::PgPool;
use std::{
    borrow::Cow,
    collections::BinaryHeap,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tera::Tera;
use tokio::time::sleep;

#[macro_use]
extern crate sqlx;
#[macro_use]
extern crate tracing;

#[derive(serde::Deserialize, Clone, Debug)]
struct Config {
    db: String,
    port: u16,
    contact_email: String,
    size_limit: Option<u64>,
    ratelimit: Option<u64>,
    cache: Option<usize>,
}

#[derive(Clone, Debug)]
struct State {
    config: Config,
    db: PgPool,
}

struct Cache {
    data: DashMap<String, String>,
    expire_timestamps: RwLock<BinaryHeap<(DateTime<Local>, String)>>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_env_var("LOG")
                .with_default_directive(tracing_subscriber::filter::LevelFilter::INFO.into())
                .from_env()
                .unwrap(),
        )
        .init();

    let cfg_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| String::from("./config.toml"));

    let config_string = std::fs::read_to_string(&cfg_path).expect("Failed to read config");
    let config = toml::from_str::<Config>(&config_string).expect("Failed to parse config");

    let mut tera = Tera::default();
    tera.add_raw_template("paste.html", include_str!("./paste.html"))
        .expect("Failed to load paste.html as template");
    tera.autoescape_on(vec![]);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(&config.db)
        .await
        .expect("Failed to connect to database!");
    migrate!("./migrations").run(&pool).await.unwrap();
    let ratelimit_map: Arc<DashMap<String, Instant>> = Arc::new(DashMap::new());
    let state = State {
        config: config.clone(),
        db: pool,
    };
    let cache: Arc<Cache> = Arc::new(Cache {
        data: DashMap::new(),
        expire_timestamps: RwLock::new(BinaryHeap::new()),
    });
    let add_state = state.clone();
    let view_state = state.clone();
    let deleter_state = state.clone();
    let add_cache = cache.clone();
    let view_cache = cache.clone();
    let app = axum::Router::new()
        .route(
            "/",
            get(root).post(move |th, multipart, headers, addr| {
                submit(
                    th,
                    multipart,
                    headers,
                    addr,
                    add_state,
                    add_cache,
                    ratelimit_map,
                )
            }),
        )
        .route(
            "/:path",
            get(move |id| get_paste(id, view_state, view_cache, tera)),
        )
        .route(
            "/favicon.ico",
            get(|| async { (StatusCode::NO_CONTENT, "") }),
        );
    tokio::spawn(async move { delete_expired(&deleter_state.db).await });
    tokio::spawn(async move { clear_cache(cache, config.cache).await });
    warn!("Listening on http://0.0.0.0:{} (http)", config.port);
    axum::Server::bind(&SocketAddr::from(([0, 0, 0, 0], config.port)))
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .expect("Failed to bind to address, is something else using the port?");
}

static_file!(root, "index.html", content_types::HTML);

async fn submit(
    TypedHeader(length): TypedHeader<ContentLength>,
    mut multipart: Multipart,
    headers: HeaderMap,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    state: State,
    cache: Arc<Cache>,
    ratelimit_map: Arc<DashMap<String, Instant>>,
) -> Result<(StatusCode, HeaderMap, String), Error> {
    if let Some(wait_time) = state.config.ratelimit {
        let remote: String;
        if let Some(remote_ip) = headers.get("X_REAL_IP") {
            remote = remote_ip.to_str()?.to_string();
        } else {
            remote = addr.ip().to_string()
        }
        if let Some(rl) = ratelimit_map.get(&remote) {
            let last_paste = rl.value();
            if let Some(time_until_unlimited) = Duration::from_secs(wait_time)
                .checked_sub(last_paste.elapsed())
                .map(|x| x.as_secs())
            {
                return Err(Error::RateLimited(time_until_unlimited));
            }
        }
        ratelimit_map.insert(remote, Instant::now());
    }

    if length.0 > state.config.size_limit.unwrap_or(1024) * 1024 {
        return Err(Error::PasteTooLarge);
    }
    let mut data = String::new();
    while let Some(field) = multipart.next_field().await? {
        if field.name().ok_or(Error::FieldInvalid)? == "contents" {
            data = field.text().await?;
            break;
        }
    }

    let persistence_length = chrono::Duration::weeks(1);
    let expires = Local::now()
        .checked_add_signed(persistence_length)
        .ok_or(Error::TimeError)?;
    let db = &state.db;
    let contents = tera::escape_html(&data).replace("\r\n", "<br>").replace("\n", "<br>");
    let key = loop {
        let id = random_string::generate(
            8,
            "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz1234567890",
        );
        if let Ok(_) = query!(
            "INSERT INTO pastes VALUES ($1, $2, $3)",
            id,
            &contents,
            expires
        )
        .execute(db)
        .await
        {
            break id;
        }
    };
    if let Some(_) = state.config.cache {
        let mut heap = cache.expire_timestamps.write();
        cache.data.insert(key.clone(), contents);
        heap.push((Local::now(), key.clone()));
    }
    let mut headers = HeaderMap::new();
    headers.insert(
        header::LOCATION,
        HeaderValue::from_str(&format!("/{}", key))?,
    );
    Ok((StatusCode::FOUND, headers, "Paste submitted!".to_string()))
}

async fn get_paste(
    Path(id): Path<String>,
    state: State,
    cache: Arc<Cache>,
    tera: Tera,
) -> Result<(StatusCode, HeaderMap, String), Error> {
    let contents: String;
    // TODO replace this with let chaining when rust 1.62 is released
    if let (Some(_), Some(item)) = (state.config.cache, cache.data.get(&id)) {
        contents = item.value().to_string();
        trace!("Cache hit!");
    } else {
        let db = &state.db;
        let res = match query!("SELECT contents FROM pastes WHERE key = $1", id)
            .fetch_one(db)
            .await
        {
            Ok(data) => data,
            Err(sqlx::Error::RowNotFound) => {
                return Err(Error::NotFound);
            }
            Err(e) => return Err(Error::Sqlx(e)),
        };
        contents = res.contents.ok_or(Error::InternalError)?;
        if let Some(_) = state.config.cache {
            let mut heap = cache.expire_timestamps.write();
            cache.data.insert(id.clone(), contents.clone());
            heap.push((Local::now(), id.clone()));
        }
    };
    let mut context = tera::Context::new();
    context.insert("contact_email", &state.config.contact_email);
    context.insert("paste_contents", &contents);
    context.insert("id", &id);
    let final_contents = tera.render("paste.html", &context)?;
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html"));

    Ok((StatusCode::OK, headers, final_contents))
}

async fn delete_expired(db: &PgPool) {
    loop {
        info!("Deleting old pastes...");
        let now: DateTime<Local> = Local::now();
        match query!("DELETE FROM pastes WHERE expires < $1", now)
            .execute(db)
            .await
        {
            Ok(_) => {}
            Err(e) => tracing::error!("Error deleting expired pastes: {}", e),
        };
        sleep(Duration::from_secs(3600)).await;
    }
}

// This was O(n^n), thanks to tazz4843 for fixing that
async fn clear_cache(cache: Arc<Cache>, max: Option<usize>) {
    if let Some(max_size) = max {
        let max_size = max_size * 1_048_576;
        loop {
            debug!("Clearing cache...");
            let mut size: usize = 0;
            for item in cache.data.iter() {
                size += item.value().capacity();
            }
            while size > max_size {
                let heap = cache.expire_timestamps.upgradable_read();
                if let Some(item) = heap.peek() {
                    size -= item.1.capacity();
                    cache.data.remove(&item.1);
                    let mut heap_rw = parking_lot::RwLockUpgradableReadGuard::upgrade(heap);
                    heap_rw.pop();
                }
            }
            sleep(Duration::from_secs(5)).await;
        }
    }
}

#[derive(Debug)]
enum Error {
    // Errors
    TimeError,
    FieldInvalid,
    InternalError,
    ToStr(header::ToStrError),
    InvalidHeaderValue(header::InvalidHeaderValue),
    Sqlx(sqlx::Error),
    Multipart(multipart::MultipartError),
    TemplatingError(tera::Error),

    // Errors that might happen to a normal user
    RateLimited(u64),
    PasteTooLarge,
    NotFound,
}

impl From<header::InvalidHeaderValue> for Error {
    fn from(e: header::InvalidHeaderValue) -> Self {
        Self::InvalidHeaderValue(e)
    }
}

impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        Self::Sqlx(e)
    }
}

impl From<multipart::MultipartError> for Error {
    fn from(e: multipart::MultipartError) -> Self {
        Self::Multipart(e)
    }
}

impl From<tera::Error> for Error {
    fn from(e: tera::Error) -> Self {
        Self::TemplatingError(e)
    }
}

impl From<header::ToStrError> for Error {
    fn from(e: header::ToStrError) -> Self {
        Self::ToStr(e)
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (body, status): (Cow<str>, StatusCode) = match self {
            Error::TimeError => ("Bad request".into(), StatusCode::BAD_REQUEST),
            Error::FieldInvalid => ("HTTP field invalid".into(), StatusCode::BAD_REQUEST),
            Error::Multipart(_) => ("MultiPartFormData invalid".into(), StatusCode::BAD_REQUEST),
            Error::InternalError => (
                "Unknown internal error".into(),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            Error::ToStr(_) => (
                "Error converting header to string".into(),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            Error::InvalidHeaderValue(_) => (
                "Invalid redirect value (this should be impossible)".into(),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            Error::Sqlx(_) => (
                "Database lookup failed".into(),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            Error::TemplatingError(_) => (
                "Templating library error".into(),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            Error::RateLimited(seconds) => (
                format!(
                    "You have been ratelimited! Try again in {} seconds.",
                    seconds
                )
                .into(),
                StatusCode::TOO_MANY_REQUESTS,
            ),
            Error::PasteTooLarge => ("Paste too large!".into(), StatusCode::TOO_MANY_REQUESTS),
            Error::NotFound => (
                include_str!("./404.html").into(),
                StatusCode::TOO_MANY_REQUESTS,
            ),
        };
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            error!("{:#?}", self);
        } else {
            warn!("{:?}", self);
        }
        let body_and_error = include_str!("./error.html").replace("{{ error }}", &body);
        Response::builder()
            .status(status)
            .header("Content-Type", "text/html")
            .body(body::boxed(body::Full::from(body_and_error)))
            .unwrap()
    }
}
