/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use bookmarks_movement::BookmarkKindRestrictions;
use bookmarks_movement::HookRejection;
use bookmarks_movement::describe_hook_rejections;
use borrowed::borrowed;
use context::CoreContext;
use futures_watchdog::WatchdogExt;
use hooks::PushAuthoredBy;
use mononoke_api::ChangesetSpecifier;
use mononoke_api::MononokeError;
use pushrebase::PushrebaseConflict;
use repo_identity::RepoIdentityRef;
use scs_errors::LoggableError;
use scs_errors::ServiceErrorResultExt;
use scs_errors::Status;
use service::RepoLandStackExn;
use source_control as thrift;
use source_control_services::errors::source_control_service as service;

use crate::from_request::FromRequest;
use crate::from_request::convert_pushvars;
use crate::into_response::AsyncIntoResponseWith;
use crate::source_control_impl::SourceControlServiceImpl;

pub(crate) enum LandStackError {
    Service(scs_errors::ServiceError),
    PushrebaseConflicts(Vec<PushrebaseConflict>),
    HookRejections(Vec<HookRejection>),
}

impl From<scs_errors::ServiceError> for LandStackError {
    fn from(e: scs_errors::ServiceError) -> Self {
        Self::Service(e)
    }
}

impl From<MononokeError> for LandStackError {
    fn from(e: MononokeError) -> Self {
        match e {
            MononokeError::HookFailure(rejections) => Self::HookRejections(rejections),
            MononokeError::PushrebaseConflicts(conflicts) => Self::PushrebaseConflicts(conflicts),
            e => Self::Service(e.into()),
        }
    }
}

impl From<anyhow::Error> for LandStackError {
    fn from(e: anyhow::Error) -> Self {
        Self::Service(scs_errors::internal_error(e).into())
    }
}

impl From<thrift::RequestError> for LandStackError {
    fn from(e: thrift::RequestError) -> Self {
        Self::Service(e.into())
    }
}

fn reason_rejections(rejections: &Vec<HookRejection>) -> String {
    format!(
        "Hooks failed:\n{}",
        describe_hook_rejections(rejections.as_slice())
    )
}

fn reason_conflicts(conflicts: &Vec<PushrebaseConflict>) -> String {
    format!("Conflicts while pushrebasing: {:?}", conflicts)
}

fn convert_rejection(rejection: HookRejection) -> thrift::HookRejection {
    thrift::HookRejection {
        hook_name: rejection.hook_name,
        cs_id: Vec::from(rejection.cs_id.as_ref()),
        reason: thrift::HookOutcomeRejected {
            description: rejection.reason.description.to_string(),
            long_description: rejection.reason.long_description,
            ..Default::default()
        },
        ..Default::default()
    }
}

impl From<LandStackError> for RepoLandStackExn {
    fn from(e: LandStackError) -> RepoLandStackExn {
        match e {
            LandStackError::Service(e) => e.into(),
            LandStackError::HookRejections(rejections) => {
                RepoLandStackExn::hook_rejections(thrift::HookRejectionsException {
                    reason: reason_rejections(&rejections),
                    rejections: rejections.into_iter().map(convert_rejection).collect(),
                    ..Default::default()
                })
            }
            LandStackError::PushrebaseConflicts(conflicts) => {
                RepoLandStackExn::pushrebase_conflicts(thrift::PushrebaseConflictsException {
                    reason: reason_conflicts(&conflicts),
                    conflicts: conflicts
                        .into_iter()
                        .map(|c| thrift::PushrebaseConflict {
                            left: c.left.to_string(),
                            right: c.right.to_string(),
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                })
            }
        }
    }
}

impl LoggableError for LandStackError {
    fn status_and_description(&self) -> (Status, String) {
        match self {
            Self::Service(svc) => svc.status_and_description(),
            Self::HookRejections(rejections) => {
                (Status::RequestError, reason_rejections(rejections))
            }
            Self::PushrebaseConflicts(conflicts) => {
                (Status::RequestError, reason_conflicts(conflicts))
            }
        }
    }
}

impl SourceControlServiceImpl {
    pub(crate) async fn repo_land_stack(
        &self,
        ctx: CoreContext,
        repo: thrift::RepoSpecifier,
        params: thrift::RepoLandStackParams,
    ) -> Result<thrift::RepoLandStackResponse, LandStackError> {
        let push_authored_by = if params.service_identity.is_some() {
            PushAuthoredBy::Service
        } else {
            PushAuthoredBy::User
        };
        let repo = self
            .repo_for_service(ctx.clone(), &repo, params.service_identity)
            .watched(ctx.logger())
            .await?;
        borrowed!(params.head, params.base);
        let head = repo
            .changeset(ChangesetSpecifier::from_request(head)?)
            .watched(ctx.logger())
            .await
            .context("failed to resolve head commit")?
            .ok_or_else(|| scs_errors::commit_not_found(head.to_string()))?;
        let base = repo
            .changeset(ChangesetSpecifier::from_request(base)?)
            .watched(ctx.logger())
            .await
            .context("failed to resolve base commit")?
            .ok_or_else(|| scs_errors::commit_not_found(base.to_string()))?;
        let pushvars = convert_pushvars(params.pushvars);
        let bookmark_restrictions =
            BookmarkKindRestrictions::from_request(&params.bookmark_restrictions)?;
        // default to true since scs_server has a permission issue to use land_service for now
        let force_local_pushrebase = justknobs::eval(
            "scm/mononoke:scs_force_local_pushrebase",
            None,
            Some(repo.repo().repo_identity().name()),
        )
        .unwrap_or(true);

        let pushrebase_outcome = repo
            .land_stack(
                &params.bookmark,
                head.id(),
                base.id(),
                pushvars.as_ref(),
                bookmark_restrictions,
                push_authored_by,
                force_local_pushrebase,
            )
            .watched(ctx.logger())
            .await?
            .into_response_with(&(
                repo.clone(),
                params.identity_schemes,
                params.old_identity_schemes,
            ))
            .watched(ctx.logger())
            .await?;

        Ok(thrift::RepoLandStackResponse {
            pushrebase_outcome,
            ..Default::default()
        })
    }
}
