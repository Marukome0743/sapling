/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use arc_swap::ArcSwap;
use async_trait::async_trait;
use cloned::cloned;
use fbinit::FacebookInit;
use mononoke_macros::mononoke;
use mononoke_types::RepositoryId;

#[cfg(fbcode_build)]
mod facebook;

#[cfg(fbcode_build)]
pub use facebook::cdn;
#[cfg(fbcode_build)]
pub use facebook::sql;

#[cfg(fbcode_build)]
pub async fn create_bundle_uri(
    fb: FacebookInit,
    manifold_api_key: &str,
    manifold_bucket_name: &str,
    connections: sql_ext::SqlConnections,
    update_cadence: Duration,
    tracked_repos: TrackedRepos,
) -> Result<BundleUri<sql::SqlGitBundleMetadataStorage, cdn::CdnManifoldBundleUrlGenerator>> {
    let url_gen = cdn::CdnManifoldBundleUrlGenerator::new(
        fb,
        manifold_api_key.to_string(),
        manifold_bucket_name.to_string(),
    );
    let storage = sql::SqlGitBundleMetadataStorage::new(connections);
    BundleUri::new(update_cadence, storage, url_gen, tracked_repos).await
}

#[cfg(not(fbcode_build))]

pub async fn create_bundle_uri(
    _fb: FacebookInit,
    _manifold_api_key: &str,
    _manifold_bucket_name: &str,
    _connections: sql_ext::SqlConnections,
    update_cadence: Duration,
    tracked_repos: TrackedRepos,
) -> Result<BundleUri<EmptyGitBundleMetadataStorage, AlwaysFailBundleGenerator>> {
    BundleUri::new(
        update_cadence,
        EmptyGitBundleMetadataStorage {},
        AlwaysFailBundleGenerator {},
        tracked_repos,
    )
    .await
}

#[async_trait]
pub trait GitBundleMetadataStorage {
    async fn get_newest_bundle_list_for_repo(
        &self,
        repo_id: RepositoryId,
    ) -> Result<Option<BundleList>>;
    async fn get_newest_bundle_lists(&self) -> Result<HashMap<RepositoryId, BundleList>>;
}

#[async_trait]
pub trait GitBundleUrlGenerator {
    type Options;
    async fn get_url_for_bundle_handle(
        &self,
        handle: &str,
        options: &Self::Options,
    ) -> Result<String>;
}

#[derive(Clone)]
pub struct AlwaysFailBundleGenerator {}

#[async_trait]
impl GitBundleUrlGenerator for AlwaysFailBundleGenerator {
    type Options = ();
    async fn get_url_for_bundle_handle(
        &self,
        _handle: &str,
        _options: &Self::Options,
    ) -> Result<String> {
        anyhow::bail!("This genereator always fails generating urls");
    }
}

#[derive(Clone)]
pub struct EmptyGitBundleMetadataStorage {}

#[async_trait]
impl GitBundleMetadataStorage for EmptyGitBundleMetadataStorage {
    async fn get_newest_bundle_list_for_repo(
        &self,
        _repo_id: RepositoryId,
    ) -> Result<Option<BundleList>> {
        Ok(None)
    }
    async fn get_newest_bundle_lists(&self) -> Result<HashMap<RepositoryId, BundleList>> {
        Ok(HashMap::new())
    }
}

type BundleLists = Arc<ArcSwap<HashMap<RepositoryId, BundleList>>>;

#[derive(Clone, Debug)]
pub struct Bundle {
    pub handle: String,
    pub fingerprint: String,
    pub in_bundle_list_order: u64,
}

#[derive(Clone, Debug)]
pub struct BundleList {
    pub bundle_list_num: u64,
    pub bundles: Vec<Bundle>,
}

pub struct BundleUri<S, U> {
    pub available_bundle_lists: BundleLists,
    pub update_cadence: Duration,
    pub bundle_metadata_storage: Arc<S>,
    pub bundle_url_generator: U,
    pub tracked_repos: TrackedRepos,
}

pub enum TrackedRepos {
    All,
    One(RepositoryId),
}

impl<S, U> BundleUri<S, U> {
    pub async fn new(
        update_cadence: Duration,
        storage: S,
        bundle_url_generator: U,
        tracked_repos: TrackedRepos,
    ) -> Result<Self>
    where
        S: GitBundleMetadataStorage + Clone + Send + 'static,
        U: GitBundleUrlGenerator + Clone + Send + 'static,
    {
        let initial_data = match tracked_repos {
            TrackedRepos::All => storage.get_newest_bundle_lists().await?,
            TrackedRepos::One(repo_id) => {
                let mut h = HashMap::new();
                if let Some(bundle_list) = storage.get_newest_bundle_list_for_repo(repo_id).await? {
                    h.insert(repo_id, bundle_list);
                }
                h
            }
        };

        let data = Arc::new(ArcSwap::new(Arc::new(initial_data)));

        match tracked_repos {
            TrackedRepos::All => {
                mononoke::spawn_task({
                    cloned!(data, storage);
                    async move {
                        loop {
                            tokio::time::sleep(update_cadence).await;

                            if let Ok(new_data) = storage.get_newest_bundle_lists().await {
                                data.swap(Arc::new(new_data));
                            } else {
                                eprintln!("failed to update");
                            }
                        }
                    }
                });
            }
            TrackedRepos::One(repo_id) => {
                mononoke::spawn_task({
                    cloned!(data, storage);
                    async move {
                        loop {
                            tokio::time::sleep(update_cadence).await;
                            if let Ok(bundle_list) =
                                storage.get_newest_bundle_list_for_repo(repo_id).await
                            {
                                let mut new_data = HashMap::new();
                                if let Some(bundle_list) = bundle_list {
                                    new_data.insert(repo_id, bundle_list);
                                }
                                data.swap(Arc::new(new_data));
                            } else {
                                eprintln!("failed to update");
                            }
                        }
                    }
                });
            }
        }

        Ok(Self {
            available_bundle_lists: data,
            update_cadence,
            bundle_metadata_storage: Arc::new(storage),
            bundle_url_generator,
            tracked_repos,
        })
    }

    pub fn bundle_list_for_repo(&self, repo: RepositoryId) -> Option<BundleList> {
        self.available_bundle_lists.load().get(&repo).cloned()
    }

    pub fn bundle_lists(&self) -> Arc<HashMap<RepositoryId, BundleList>> {
        self.available_bundle_lists.load().clone()
    }
}
