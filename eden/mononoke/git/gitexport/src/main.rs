/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::env;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Error;
use anyhow::Ok;
use anyhow::Result;
use anyhow::anyhow;
use bookmarks_types::BookmarkKey;
use commit_id::parse_commit_id;
use derived_data_remote::RemoteDerivationArgs;
use fbinit::FacebookInit;
use futures::StreamExt;
use futures::stream::TryStreamExt;
use futures::stream::{self};
use futures_stats::TimedTryFutureExt;
use gitexport_tools::ExportPathInfo;
use gitexport_tools::MASTER_BOOKMARK;
use gitexport_tools::build_partial_commit_graph_for_export;
use gitexport_tools::create_git_repo_on_disk;
use gitexport_tools::rewrite_partial_changesets;
use mononoke_api::BookmarkFreshness;
use mononoke_api::ChangesetContext;
use mononoke_api::ChangesetId;
use mononoke_api::CoreContext;
use mononoke_api::MononokeRepo;
use mononoke_api::Repo;
use mononoke_api::RepoContext;
use mononoke_app::MononokeApp;
use mononoke_app::MononokeAppBuilder;
use mononoke_app::monitoring::AliveService;
use mononoke_app::monitoring::MonitoringAppExtension;
use mononoke_repos::MononokeRepos;
use mononoke_types::NonRootMPath;
use print_graph::PrintGraphOptions;
use print_graph::print_graph;
use repo_authorization::AuthorizationContext;
use repo_factory::ReadOnlyStorage;
use scuba_ext::FutureStatsScubaExt;
use slog::info;
use slog::trace;
use slog::warn;
use types::ExportPathsInfoArg;
use types::HeadChangesetArg;

use crate::types::GitExportArgs;

pub mod types {
    use std::path::PathBuf;

    use clap::Args;
    use clap::Parser;
    use mononoke_app::args::RepoArgs;
    use serde::Deserialize;
    use serde::Serialize;

    #[derive(Debug, Args)]
    pub struct PrintGraphArgs {
        // Graph printing args for debugging and tests
        #[clap(long)]
        /// Print the commit graph of the source repo to the provided file.
        /// Used for integration tests.
        pub source_graph_output: Option<PathBuf>,

        #[clap(long)]
        /// Print the commit graph of the partial repo to the provided file
        /// Used for integration tests.
        pub partial_graph_output: Option<PathBuf>,

        /// Maximum distance from the initial changeset to any displayed
        /// changeset when printing a commit graph.
        #[clap(long, short, default_value_t = 10)]
        pub distance_limit: usize,
    }

    /// Mononoke Git Exporter
    #[derive(Debug, Parser)]
    pub struct GitExportArgs {
        /// Name of the hg repo being exported
        #[clap(flatten)]
        pub hg_repo_args: RepoArgs,

        /// Path to the git repo being created
        #[clap(long = "git-output", short = 'o')]
        pub git_repo_path: PathBuf,

        /// List of directories in `hg_repo` to be exported to a git repo
        #[clap(long, short('p'))]
        /// Paths in the source hg repo that should be exported to a git repo.
        pub export_paths: Vec<PathBuf>,
        // Specify the changeset used to lookup the history of the exported
        // directories. Any exported changeset will be its ancestor.
        // Provide either a changeset id or bookmark name.
        #[clap(
            long,
            short = 'i',
            conflicts_with = "latest_cs_bookmark",
            required = true
        )]
        pub latest_cs_id: Option<String>,
        #[clap(long, short = 'B', conflicts_with = "latest_cs_id", required = true)]
        pub latest_cs_bookmark: Option<String>,

        /// JSON file storing a list of export paths with different changeset
        /// upper bounds.
        ///
        /// The JSON file should contain a list of JSON serialized
        /// `ExportPathsInfoArg`, e.g.
        /// ```
        /// [ { "paths": ["foo"], "head": { "ID": "abcde123" }  } ]
        /// ```
        /// or
        /// ```
        /// [
        ///     { "paths": ["bar"], "head": { "Bookmark": "master" }  },
        ///     { "paths": ["foo"], "head": { "ID": "abcde123" }  },
        ///
        /// ]
        /// ```
        #[clap(long, short = 'f')]
        pub bounded_export_paths_file: Option<String>,

        // Consider history until the provided timestamp, i.e. all exported
        // commits will have its creation time greater than or equal to it.
        #[clap(long)]
        pub oldest_commit_ts: Option<i64>,

        // -----------------------------------------------------------------
        // Graph printing args for debugging and tests
        #[clap(flatten)]
        pub print_graph_args: PrintGraphArgs,

        /// Size of the buffer in the stream that pre-fetches implicit deletes
        /// of each commit before its rewritten.
        #[clap(long = "impl-del-concurrency", short = 'b', default_value_t = 100)]
        pub implicit_delete_prefetch_buffer_size: usize,
    }

    /// Data type used  to get upper bound changesets for export paths through
    /// JSON files.
    #[derive(Debug, Serialize, Deserialize)]
    pub enum HeadChangesetArg {
        ID(String),
        Bookmark(String),
    }

    /// Data type used  to get export paths with specific upper bound changesets
    /// through JSON files. After deserilization, these will be converted to
    /// `ExportPathInfo`.
    #[derive(Debug, Serialize, Deserialize)]
    pub struct ExportPathsInfoArg {
        pub paths: Vec<PathBuf>,
        pub head: HeadChangesetArg,
    }
}

#[fbinit::main]
fn main(fb: FacebookInit) -> Result<(), Error> {
    let read_only_storage = ReadOnlyStorage(true);

    let remove_derivation_args = RemoteDerivationArgs {
        derive_remotely: true,
    };

    let app: MononokeApp = MononokeAppBuilder::new(fb)
        .with_arg_defaults(read_only_storage)
        .with_arg_defaults(remove_derivation_args)
        .with_default_scuba_dataset("mononoke_gitexport")
        .with_app_extension(MonitoringAppExtension {})
        .build::<GitExportArgs>()?;

    app.run_with_monitoring_and_logging(async_main, "gitexport", AliveService)
}

async fn async_main(app: MononokeApp) -> Result<(), Error> {
    let start_time = std::time::Instant::now();

    let args: GitExportArgs = app.args()?;
    let ctx = app.new_basic_context();
    let metadata = ctx.session().metadata();
    let session_id = metadata.session_id();
    let logger = ctx.logger().clone();

    let ctx = ctx.with_mutated_scuba(|mut scuba| {
        scuba.add_metadata(metadata);
        if let Result::Ok(unixname) = env::var("USER") {
            scuba.add("unixname", unixname);
        }
        scuba
    });
    info!(logger, "Starting session with id {}", session_id);

    async_main_impl(app, args, ctx.clone())
        .try_timed()
        .await?
        .log_future_stats(ctx.scuba().clone(), "Gitexport execution", None);

    info!(
        &logger,
        "Finished export in {} seconds",
        start_time.elapsed().as_secs()
    );

    Ok(())
}

async fn async_main_impl(
    app: MononokeApp,
    args: GitExportArgs,
    ctx: CoreContext,
) -> Result<(), Error> {
    let logger = ctx.logger().clone();
    let repo: Arc<Repo> = app.open_repo(&args.hg_repo_args).await?;

    if !app.environment().readonly_storage.0 {
        warn!(logger, "readonly_storage is DISABLED!");
    };

    if !app.environment().remote_derivation_options.derive_remotely {
        warn!(logger, "Remote derivation is DISABLED!");
    };

    let auth_ctx = AuthorizationContext::new_bypass_access_control();
    let repo_ctx: RepoContext<Repo> = RepoContext::new(
        ctx.clone(),
        auth_ctx.into(),
        repo,
        None,
        None,
        Arc::new(MononokeRepos::new()),
    )
    .await?;

    let cs_ctx = get_latest_changeset_context(&repo_ctx, &args).await?;

    info!(
        logger,
        "Using changeset {0:?} as the starting changeset",
        cs_ctx.id()
    );

    if let Some(source_graph_output) = args.print_graph_args.source_graph_output.clone() {
        print_commit_graph(
            &repo_ctx,
            cs_ctx.id(),
            source_graph_output,
            args.print_graph_args.distance_limit,
        )
        .await?;
    };

    // Paths provided directly via args with a single head commit, e.g. "master".
    let export_paths = args
        .export_paths
        .into_iter()
        .map(|p| TryFrom::try_from(p.as_os_str()))
        .collect::<Result<Vec<NonRootMPath>>>()?;

    let export_path_infos = {
        let mut export_path_infos: Vec<(NonRootMPath, ChangesetContext<Repo>)> = export_paths
            .into_iter()
            .map(|p| (p, cs_ctx.clone()))
            .collect();

        // Paths provided with associated head commits through a JSON file
        let export_paths_with_specific_heads: Vec<ExportPathInfo<Repo>> =
            match args.bounded_export_paths_file {
                Some(file) => get_bounded_export_paths(&repo_ctx, file).await?,
                None => vec![],
            };

        export_path_infos.extend(export_paths_with_specific_heads);

        export_path_infos
    };

    info!(
        logger,
        "Export paths and their HEAD commits: {0:#?}", export_path_infos
    );

    let graph_info = build_partial_commit_graph_for_export(
        &ctx,
        &logger,
        export_path_infos.clone(),
        args.oldest_commit_ts,
    )
    .try_timed()
    .await?
    .log_future_stats(
        repo_ctx.ctx().scuba().clone(),
        "Build partial commit graph",
        None,
    );

    trace!(logger, "changesets: {:#?}", &graph_info.changesets);
    trace!(logger, "changeset parents: {:#?}", &graph_info.parents_map);

    let ctx = repo_ctx.ctx().clone();
    let temp_repo_ctx = rewrite_partial_changesets(
        app.fb,
        repo_ctx,
        graph_info,
        export_path_infos,
        args.implicit_delete_prefetch_buffer_size,
    )
    .try_timed()
    .await?
    .log_future_stats(ctx.scuba().clone(), "Rewrite all relevant commits", None);

    let temp_master_csc = temp_repo_ctx
        .resolve_bookmark(
            &BookmarkKey::from_str(MASTER_BOOKMARK)?,
            BookmarkFreshness::MostRecent,
        )
        .await?
        .ok_or(anyhow!("Couldn't find master bookmark in temp repo."))?;

    if let Some(partial_graph_output) = args.print_graph_args.partial_graph_output.clone() {
        print_commit_graph(
            &temp_repo_ctx,
            temp_master_csc.id(),
            partial_graph_output,
            args.print_graph_args.distance_limit,
        )
        .await?;
    };

    create_git_repo_on_disk(
        temp_repo_ctx.ctx(),
        temp_repo_ctx.repo(),
        args.git_repo_path,
    )
    .try_timed()
    .await?
    .log_future_stats(ctx.scuba().clone(), "Create git bundle", None);

    Ok(())
}

async fn print_commit_graph<R: MononokeRepo>(
    repo_ctx: &RepoContext<R>,
    cs_id: ChangesetId,
    output: PathBuf,
    limit: usize,
) -> Result<()> {
    let print_graph_args = PrintGraphOptions {
        limit,
        display_message: true,
        display_id: false,
        display_file_changes: true,
        display_author: false,
        display_author_date: false,
    };
    let changesets = vec![cs_id];

    let output_file = Box::new(File::create(output).unwrap());

    print_graph(
        repo_ctx.ctx(),
        repo_ctx.repo(),
        changesets,
        print_graph_args,
        output_file,
    )
    .await
}

/// Gets the head commit for all export paths provided via the
/// `-p` (or `--export_paths`) argument.
async fn get_latest_changeset_context<R: MononokeRepo>(
    repo_ctx: &RepoContext<R>,
    args: &GitExportArgs,
) -> Result<ChangesetContext<R>> {
    if let Some(changeset_id) = &args.latest_cs_id {
        return get_changeset_context_from_head_arg(
            repo_ctx,
            HeadChangesetArg::ID(changeset_id.clone()),
        )
        .await;
    };

    let bookmark_name = args.latest_cs_bookmark.clone().ok_or(anyhow!(
        "No bookmark or changeset id specified to search history"
    ))?;

    get_changeset_context_from_head_arg(repo_ctx, HeadChangesetArg::Bookmark(bookmark_name)).await
}

async fn get_bounded_export_paths<R: MononokeRepo>(
    repo_ctx: &RepoContext<R>,
    bounded_export_paths_file: String,
) -> Result<Vec<ExportPathInfo<R>>> {
    let export_path_info_args = read_bounded_export_paths_file(bounded_export_paths_file)?;

    stream::iter(export_path_info_args)
        .then(|ep_arg| async move {
            let export_paths: Vec<NonRootMPath> = ep_arg
                .paths
                .into_iter()
                .map(|p| TryFrom::try_from(p.as_os_str()))
                .collect::<Result<Vec<NonRootMPath>>>()?;

            let head_cs: ChangesetContext<R> =
                get_changeset_context_from_head_arg(repo_ctx, ep_arg.head).await?;

            Ok(stream::iter(export_paths)
                .map(move |p| Ok::<ExportPathInfo<R>>((p, head_cs.clone()))))
        })
        .try_flatten()
        .try_collect::<Vec<_>>()
        .await
}

/// Deserialize a JSON file containing export paths and associated head commits
fn read_bounded_export_paths_file(
    bounded_export_paths_file: String,
) -> Result<Vec<ExportPathsInfoArg>> {
    let mut file = File::open(bounded_export_paths_file)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    Ok(serde_json::from_str(&contents)?)
}

async fn get_changeset_context_from_head_arg<R: MononokeRepo>(
    repo_ctx: &RepoContext<R>,
    head_cs: HeadChangesetArg,
) -> Result<ChangesetContext<R>> {
    match head_cs {
        HeadChangesetArg::ID(changeset_id) => {
            let cs_id =
                parse_commit_id(repo_ctx.ctx(), repo_ctx.repo(), changeset_id.as_str()).await?;
            repo_ctx
                .changeset(cs_id)
                .await?
                .ok_or(anyhow!("Provided starting changeset id not found"))
        }
        HeadChangesetArg::Bookmark(bookmark_name) => {
            let bookmark_key = BookmarkKey::from_str(bookmark_name.as_str())?;

            repo_ctx
                .resolve_bookmark(&bookmark_key, BookmarkFreshness::MostRecent)
                .await?
                .ok_or(anyhow!(
                    "Expected the repo to contain the bookmark: {bookmark_key}. It didn't"
                ))
        }
    }
}
