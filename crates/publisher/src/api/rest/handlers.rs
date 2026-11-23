//! Request handlers: each asks `AccessService` who is calling, then calls the service.
use super::AppState;
use super::dto::{
    AcceptedDto, AccessListDto, AccessRequestReq, CiInfoDto, ListQuery, LogLineDto, LogQuery,
    MeDto, QueuedCiDto, RunDetailDto, RunSummaryDto, StartCiReq, StartReleaseReq, StartedDto,
    UserDto, decision,
};
use super::error::{ApiError, ApiResult};
use crate::domain::error::DomainError;
use crate::domain::release::{ReleaseInput, STEPS};
use crate::domain::service::{CiStart, Standing};
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

/// Answers every signed-in caller: the UI shows the request form from it.
pub async fn me(Extension(app): App, Extension(ctx): Ctx) -> ApiResult<MeDto> {
    let (uid, email, name, role, note) = match app.access.standing(&ctx).await? {
        Standing::Admin(p) => (p.uid, p.email, p.name, "admin", None),
        Standing::Member(p) => (p.uid, p.email, p.name, "member", None),
        Standing::Pending(u) => (u.uid, u.email, u.name, "pending", Some(u.note)),
        Standing::Rejected(u) => (u.uid, u.email, u.name, "rejected", Some(u.note)),
        Standing::Unknown(c) => (c.uid, c.email, c.name, "none", None),
    };
    let pending_requests = if role == "admin" {
        Some(app.access.pending().await?)
    } else {
        None
    };
    Ok(Json(MeDto {
        uid,
        email,
        name,
        role,
        note,
        pending_requests,
        steps: &STEPS,
        ci: CiInfoDto::new(app.repository.clone(), app.service.ci_refusal()),
    }))
}

pub async fn list_runs(
    Extension(app): App,
    Extension(ctx): Ctx,
    Query(q): Query<ListQuery>,
) -> ApiResult<Vec<RunSummaryDto>> {
    let person = app.access.person(&ctx).await?;
    let runs = app.service.list(&person, q.kind.map(Into::into)).await?;
    Ok(Json(runs.into_iter().map(Into::into).collect()))
}

pub async fn start_release(
    Extension(app): App,
    Extension(ctx): Ctx,
    body: Option<Json<StartReleaseReq>>,
) -> Result<(StatusCode, Json<StartedDto>), ApiError> {
    let person = app.access.person(&ctx).await?;
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
    Ok((StatusCode::CREATED, Json(StartedDto::run(id))))
}

/// `201` with the run, or `202` with the queue entry when another PR's CI is running.
pub async fn start_ci(
    Extension(app): App,
    Extension(ctx): Ctx,
    Json(body): Json<StartCiReq>,
) -> Result<(StatusCode, Json<StartedDto>), ApiError> {
    let person = app.access.person(&ctx).await?;
    Ok(
        match app.service.start_ci(&person, body.pr, body.jobs).await? {
            CiStart::Started(id) => (StatusCode::CREATED, Json(StartedDto::run(id))),
            CiStart::Queued { id, position } => (
                StatusCode::ACCEPTED,
                Json(StartedDto {
                    id,
                    queued: true,
                    position: Some(position),
                }),
            ),
        },
    )
}

pub async fn ci_queue(Extension(app): App, Extension(ctx): Ctx) -> ApiResult<Vec<QueuedCiDto>> {
    let person = app.access.person(&ctx).await?;
    let queue = app.service.ci_queue(&person).await?;
    Ok(Json(queue.into_iter().map(Into::into).collect()))
}

pub async fn cancel_queued(
    Extension(app): App,
    Extension(ctx): Ctx,
    Path(id): Path<Uuid>,
) -> ApiResult<AcceptedDto> {
    let person = app.access.person(&ctx).await?;
    app.service.cancel_queued(&person, id).await?;
    Ok(Json(AcceptedDto { ok: true }))
}

pub async fn get_run(
    Extension(app): App,
    Extension(ctx): Ctx,
    Path(id): Path<Uuid>,
) -> ApiResult<RunDetailDto> {
    let person = app.access.person(&ctx).await?;
    Ok(Json(app.service.get(&person, id).await?.into()))
}

pub async fn run_log(
    Extension(app): App,
    Extension(ctx): Ctx,
    Path(id): Path<Uuid>,
    Query(q): Query<LogQuery>,
) -> ApiResult<Vec<LogLineDto>> {
    let person = app.access.person(&ctx).await?;
    let lines = app
        .service
        .log(&person, id, q.after, q.limit.unwrap_or(2000))
        .await?;
    Ok(Json(lines.into_iter().map(Into::into).collect()))
}

pub async fn cancel_run(
    Extension(app): App,
    Extension(ctx): Ctx,
    Path(id): Path<Uuid>,
) -> ApiResult<AcceptedDto> {
    let person = app.access.person(&ctx).await?;
    app.service.cancel(&person, id).await?;
    Ok(Json(AcceptedDto { ok: true }))
}

pub async fn resume_run(
    Extension(app): App,
    Extension(ctx): Ctx,
    Path(id): Path<Uuid>,
) -> ApiResult<AcceptedDto> {
    let person = app.access.person(&ctx).await?;
    app.service.resume(&person, id).await?;
    Ok(Json(AcceptedDto { ok: true }))
}

pub async fn retry_run(
    Extension(app): App,
    Extension(ctx): Ctx,
    Path(id): Path<Uuid>,
) -> ApiResult<AcceptedDto> {
    let person = app.access.person(&ctx).await?;
    app.service.retry(&person, id).await?;
    Ok(Json(AcceptedDto { ok: true }))
}

/// Ask for access, or edit the note while waiting.
pub async fn request_access(
    Extension(app): App,
    Extension(ctx): Ctx,
    Json(body): Json<AccessRequestReq>,
) -> ApiResult<UserDto> {
    Ok(Json(app.access.request(&ctx, &body.note).await?.into()))
}

pub async fn list_access(Extension(app): App, Extension(ctx): Ctx) -> ApiResult<AccessListDto> {
    app.access.admin(&ctx).await?;
    Ok(Json(AccessListDto {
        admins: app.access.admins().into_iter().map(Into::into).collect(),
        users: app
            .access
            .requests()
            .await?
            .into_iter()
            .map(Into::into)
            .collect(),
    }))
}

pub async fn decide_access(
    Extension(app): App,
    Extension(ctx): Ctx,
    Path((subject, action)): Path<(Uuid, String)>,
) -> ApiResult<UserDto> {
    let admin = app.access.admin(&ctx).await?;
    let status = decision(&action)
        .ok_or_else(|| DomainError::Invalid(format!("unknown decision `{action}`")))?;
    Ok(Json(
        app.access.decide(&admin, subject, status).await?.into(),
    ))
}
