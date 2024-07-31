use std::{
    io::{BufWriter, Cursor},
    sync::LazyLock,
};

use axum::{
    extract::{MatchedPath, Query, Request},
    http::{header, Response, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use image::{Rgba, RgbaImage};
use serde::{Deserialize, Serialize};
use surrealdb::{
    engine::local::{Db, SpeeDb},
    Surreal,
};
use tokio::net::TcpListener;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

static PIXEL: LazyLock<Vec<u8>> = LazyLock::new(|| {
    let image = RgbaImage::from_pixel(1, 1, Rgba([0, 0, 0, 0]));
    let mut cursor = Cursor::new(Vec::new());
    image
        .write_to(&mut cursor, image::ImageFormat::Png)
        .expect("couldn't create image bytes");
    cursor.into_inner()
});

#[derive(Serialize, Deserialize)]
struct Tag {
    to: String,
    subject: String,
    read: bool,
}

struct DatabaseError(surrealdb::Error);

impl IntoResponse for DatabaseError {
    fn into_response(self) -> axum::response::Response {
        tracing::error!("Database error: {}", self.0);
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    }
}

#[derive(Deserialize)]
struct GetParams {
    id: String,
}

async fn get_image(
    axum::extract::State(state): axum::extract::State<State>,
    query: Query<GetParams>,
) -> Result<impl IntoResponse, DatabaseError> {
    state
        .db
        .use_ns("production")
        .use_db("orwell")
        .await
        .expect("this shouldn't fail for a local database");

    let id = query.id.clone();

    if let Some(mut tag) = state
        .db
        .select::<Option<Tag>>(("tags", id.as_str()))
        .await
        .map_err(DatabaseError)?
    {
        tag.read = true;

        state
            .db
            .update::<Option<Tag>>(("tags", id.as_str()))
            .merge(tag)
            .await
            .map_err(DatabaseError)?;
    }

    Ok(([(header::CONTENT_TYPE, "image/png")], PIXEL.clone()))
}

#[derive(Deserialize, Serialize)]
struct CreateOptions {
    to: String,
    subject: String,
}

async fn create_tag(
    state: axum::extract::State<State>,
    query: Json<CreateOptions>,
) -> Result<String, DatabaseError> {
    state
        .db
        .use_ns("production")
        .use_db("orwell")
        .await
        .map_err(DatabaseError)?;

    let result: String = state
        .db
        .query("CREATE tags SET to = $to, subject = $subject, read = false RETURN meta::id(id)")
        .bind(query.0)
        .await
        .map_err(DatabaseError)?
        .take::<Option<String>>("meta::id")
        .map_err(DatabaseError)?
        .expect("there should defeinitely be a result at this point");

    Ok(result)
}

#[derive(Clone)]
struct State {
    db: Surreal<Db>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let db = Surreal::new::<SpeeDb>("./data.db").await?;

    let app = Router::new()
        .route("/", get(get_image))
        .route("/tags", post(create_tag))
        .layer(
            tower_http::trace::TraceLayer::new_for_http()
                .make_span_with(|req: &Request| {
                    let method = req.method();
                    let uri = req.uri();

                    let matched_path = req
                        .extensions()
                        .get::<MatchedPath>()
                        .map(|matched_path| matched_path.as_str());

                    tracing::error_span!("request", %method, %uri, matched_path)
                })
                .on_failure(()),
        )
        .with_state(State { db });

    let listener = TcpListener::bind("0.0.0.0:3000").await?;
    axum::serve(listener, app).await?;

    Ok(())
}
