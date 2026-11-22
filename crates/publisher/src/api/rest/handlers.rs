//! Request handlers: each checks the caller is configured, then calls the service.
use super::AppState;
use super::dto::{
    AcceptedDto, CiInfoDto, ListQuery, LogLineDto, LogQuery, MeDto, RunDetailDto, RunSummaryDto,
    StartCiReq, StartReleaseReq, StartedDto,
};
use super::error::{ApiError, ApiResult};
use crate::domain::release::{ReleaseInput, STEPS};
use axum::{
    Extension, Json,
    extract::{Path, Query},
    http::StatusCode,
};
use std::sync::Arc;
use toolkit_security::SecurityContext;
use uuid::Uuid;

type App = Extension<Arc<AppState>>;
type Ctx = Extension<SecurityContext>;

pub async fn me(Extension(app): App, Extension(ctx): Ctx) -> ApiResult<MeDto> {
    let person = app.service.person(&ctx)?;
    Ok(Json(MeDto {
        uid: person.uid.clone(),
        email: person.email.clone(),
        steps: &STEPS,
        ci: CiInfoDto::new(app.repository.clone(), app.service.ci_refusal()),
    }))
}

pub async fn list_runs(
    Extension(app): App,
    Extension(ctx): Ctx,
    Query(q): Query<ListQuery>,
) -> ApiResult<Vec<RunSummaryDto>> {
    app.service.person(&ctx)?;
    let runs = app.service.list(q.kind.map(Into::into)).await?;
    Ok(Json(runs.into_iter().map(Into::into).collect()))
}

pub async fn start_release(
    Extension(app): App,
    Extension(ctx): Ctx,
    body: Option<Json<StartReleaseReq>>,
) -> Result<(StatusCode, Json<StartedDto>), ApiError> {
    let person = app.service.person(&ctx)?.clone();
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let id = app
        .service
        .start_release(
            &person,
            ReleaseInput {
                dry_run: body.dry_run,
            },
        )
        .await?;
    Ok((StatusCode::CREATED, Json(StartedDto { id })))
}

pub async fn start_ci(
    Extension(app): App,
    Extension(ctx): Ctx,
    Json(body): Json<StartCiReq>,
) -> Result<(StatusCode, Json<StartedDto>), ApiError> {
    let person = app.service.person(&ctx)?.clone();
    let id = app.service.start_ci(&person, body.pr, body.jobs).await?;
    Ok((StatusCode::CREATED, Json(StartedDto { id })))
}

pub async fn get_run(
    Extension(app): App,
    Extension(ctx): Ctx,
    Path(id): Path<Uuid>,
) -> ApiResult<RunDetailDto> {
    app.service.person(&ctx)?;
    Ok(Json(app.service.get(id).await?.into()))
}

pub async fn run_log(
    Extension(app): App,
    Extension(ctx): Ctx,
    Path(id): Path<Uuid>,
    Query(q): Query<LogQuery>,
) -> ApiResult<Vec<LogLineDto>> {
    app.service.person(&ctx)?;
    let lines = app
        .service
        .log(id, q.after, q.limit.unwrap_or(2000))
        .await?;
    Ok(Json(lines.into_iter().map(Into::into).collect()))
}

pub async fn cancel_run(
    Extension(app): App,
    Extension(ctx): Ctx,
    Path(id): Path<Uuid>,
) -> ApiResult<AcceptedDto> {
    let person = app.service.person(&ctx)?;
    app.service.cancel(person, id).await?;
    Ok(Json(AcceptedDto { ok: true }))
}

pub async fn resume_run(
    Extension(app): App,
    Extension(ctx): Ctx,
    Path(id): Path<Uuid>,
) -> ApiResult<AcceptedDto> {
    let person = app.service.person(&ctx)?;
    app.service.resume(person, id).await?;
    Ok(Json(AcceptedDto { ok: true }))
}

pub async fn retry_run(
    Extension(app): App,
    Extension(ctx): Ctx,
    Path(id): Path<Uuid>,
) -> ApiResult<AcceptedDto> {
    let person = app.service.person(&ctx)?;
    app.service.retry(person, id).await?;
    Ok(Json(AcceptedDto { ok: true }))
}
