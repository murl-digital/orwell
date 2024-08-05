use std::{
    io::{BufWriter, Cursor},
    sync::LazyLock,
    time::Duration,
};

use axum::{
    extract::{MatchedPath, Query, Request},
    http::{header, Response, StatusCode},
    response::{
        self,
        sse::{Event, KeepAlive},
        IntoResponse, Sse,
    },
    routing::{get, post},
    Json, Router,
};
use futures_util::{Stream, StreamExt, TryStreamExt};
use image::{Rgba, RgbaImage};
use serde::{Deserialize, Serialize};
use surrealdb::{
    engine::local::{Db, SpeeDb},
    opt::RecordId,
    sql::{Id, Object},
    Notification, Surreal,
};
use thiserror::Error;
use tokio::net::TcpListener;
use tracing::error;
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
struct RawTag {
    id: RecordId,
    to: String,
    subject: String,
    read: i32,
}

#[derive(Serialize, Deserialize)]
struct Tag {
    id: String,
    to: String,
    subject: String,
    read: i32,
}

#[derive(Debug)]
struct IncompatibleId;

impl TryFrom<RawTag> for Tag {
    type Error = IncompatibleId;

    fn try_from(value: RawTag) -> Result<Self, Self::Error> {
        if let Id::String(id) = value.id.id {
            Ok(Self {
                id,
                to: value.to,
                subject: value.subject,
                read: value.read,
            })
        } else {
            Err(IncompatibleId)
        }
    }
}

#[derive(Error, Debug)]
#[error(transparent)]
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
        .select::<Option<RawTag>>(("tags", id.as_str()))
        .await
        .map_err(DatabaseError)?
    {
        tag.read += 1;

        state
            .db
            .update::<Option<RawTag>>(("tags", id.as_str()))
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
        .query("CREATE tags SET to = $to, subject = $subject, read = 0 RETURN meta::id(id)")
        .bind(query.0)
        .await
        .map_err(DatabaseError)?
        .take::<Option<String>>("meta::id")
        .map_err(DatabaseError)?
        .expect("there should defeinitely be a result at this point");

    Ok(result)
}

#[derive(Serialize)]
#[serde(tag = "action", content = "d")]
enum StreamAction {
    Initialize(Vec<Tag>),
    Create(Tag),
    Update(Tag),
    Delete(Tag),
}

impl TryFrom<Notification<RawTag>> for StreamAction {
    type Error = IncompatibleId;

    fn try_from(value: Notification<RawTag>) -> Result<Self, Self::Error> {
        Ok(match value.action {
            surrealdb::Action::Create => Self::Create(value.data.try_into()?),
            surrealdb::Action::Update => Self::Update(value.data.try_into()?),
            surrealdb::Action::Delete => Self::Delete(value.data.try_into()?),
            _ => todo!(),
        })
    }
}

async fn get_tags(
    state: axum::extract::State<State>,
) -> Result<Sse<impl Stream<Item = Result<Event, DatabaseError>>>, DatabaseError> {
    state
        .db
        .use_ns("production")
        .use_db("orwell")
        .await
        .map_err(DatabaseError)?;

    let db = state.db.to_owned();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<Event, DatabaseError>>(128);

    tokio::spawn(async move {
        let query = db.select::<Vec<RawTag>>("tags");
        match query.await {
            Ok(data) => {
                if tx
                    .send(Ok(Event::default()
                        .json_data(StreamAction::Initialize(
                            data.into_iter()
                                .map(Tag::try_from)
                                .collect::<Result<_, _>>()
                                .expect("tag ids should always be compatable"),
                        ))
                        .expect("huh")))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Err(err) => {
                tx.send(Err(DatabaseError(err))).await;
                return;
            }
        }
        let mut stream = db.select::<Vec<RawTag>>("tags").live().await.expect("huh");
        while let Some(notif) = stream.next().await {
            match notif {
                Ok(notif) => {
                    if tx
                        .send(Ok(Event::default()
                            .json_data(
                                StreamAction::try_from(notif)
                                    .expect("tag ids should always be compatable"),
                            )
                            .expect("why didn't this work?")))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Err(err) => {
                    tx.send(Err(DatabaseError(err))).await;
                    return;
                }
            }
        }
    });

    let stream = async_stream::stream! {
        while let Some(item) = rx.recv().await {
            yield item;
        }
    };

    Ok(Sse::new(stream).keep_alive(KeepAlive::new()))
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
        .route("/tags", get(get_tags))
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
