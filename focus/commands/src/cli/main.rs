#![allow(clippy::too_many_arguments)]

use std::{
    convert::TryFrom,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use anyhow::{bail, Context, Result};
use chrono::NaiveDate;
use clap::Parser;
use focus_migrations::production::perform_pending_migrations;
use git2::Repository;

use focus_util::{
    app::{App, ExitCode},
    git_helper,
    lock_file::LockFile,
    paths, sandbox,
    time::FocusTime,
};

use focus_internals::{
    operation,
    operation::maintenance::{self, ScheduleOpts},
    tracker::Tracker,
};
use strum::VariantNames;
use tracing::{debug, debug_span, info};

#[derive(Parser, Debug)]
enum Subcommand {
    /// Create a sparse clone from named layers or ad-hoc build targets
    Clone {
        /// Path to the repository to clone.
        #[clap(long, default_value = "~/workspace/source")]
        dense_repo: String,

        /// Path where the new sparse repository should be created.
        #[clap(parse(from_os_str))]
        sparse_repo: PathBuf,

        /// The name of the branch to clone.
        #[clap(long, default_value = "master")]
        branch: String,

        /// Days of history to maintain in the sparse repo.
        #[clap(long, default_value = "90")]
        days_of_history: u64,

        /// Copy only the specified branch rather than all local branches.
        #[clap(long, parse(try_from_str), default_value = "true")]
        copy_branches: bool,

        /// The repository to fetch the index from.
        #[clap(long, default_value = operation::index::INDEX_DEFAULT_REMOTE)]
        index_remote: String,

        /// Whether to fetch the index.
        #[clap(long, parse(try_from_str), default_value = "true")]
        fetch_index: bool,

        /// Initial projects and targets to add to the repo.
        projects_and_targets: Vec<String>,
    },

    /// Update the sparse checkout to reflect changes to the build graph.
    Sync {
        /// Path to the sparse repository.
        #[clap(parse(from_os_str), default_value = ".")]
        sparse_repo: PathBuf,

        /// Try to fetch a remote index before syncing.
        #[clap(long, parse(try_from_str), default_value = "true")]
        fetch_index: bool,
    },

    /// Interact with repos configured on this system. Run `focus repo help` for more information.
    Repo {
        #[clap(subcommand)]
        subcommand: RepoSubcommand,
    },

    /// Add projects and targets to the selection.
    Add {
        /// Try to fetch a remote index before syncing.
        #[clap(long, parse(try_from_str), default_value = "true")]
        fetch_index: bool,

        /// Project and targets to add to the selection.
        projects_and_targets: Vec<String>,
    },

    /// Remove projects and targets from the selection.
    #[clap(visible_alias("rm"))]
    Remove {
        /// Try to fetch a remote index before syncing.
        #[clap(long, parse(try_from_str), default_value = "true")]
        fetch_index: bool,

        /// Project and targets to remove from the selection
        projects_and_targets: Vec<String>,
    },

    /// Display which projects and targets are selected.
    Status {},

    /// List available projects.
    Projects {},

    /// Detect whether there are changes to the build graph (used internally)
    DetectBuildGraphChanges {
        /// Path to the repository.
        #[clap(long, parse(from_os_str), default_value = ".")]
        repo: PathBuf,

        /// Extra arguments.
        args: Vec<String>,
    },

    /// Utility methods for listing and expiring outdated refs. Used to maintain a time windowed
    /// repository.
    Refs {
        #[clap(long, parse(from_os_str), default_value = ".")]
        repo: PathBuf,

        #[clap(subcommand)]
        subcommand: RefsSubcommand,
    },

    /// Set up an initial clone of the repo from the remote
    Init {
        /// By default we take 90 days of history, pass a date with this option
        /// if you want a different amount of history
        #[clap(long, parse(try_from_str = operation::init::parse_shallow_since_date))]
        shallow_since: Option<NaiveDate>,

        /// This command will only ever clone a single ref, by default this is
        /// "master". If you wish to clone a different branch, then use this option
        #[clap(long, default_value = "master")]
        branch_name: String,

        #[clap(long)]
        no_checkout: bool,

        /// The default is to pass --no-tags to clone, this option, if given,
        /// will cause git to do its normal default tag following behavior
        #[clap(long)]
        follow_tags: bool,

        /// If not given, we use --filter=blob:none. To use a different filter
        /// argument, use this option. To disable filtering, use --no-filter.
        #[clap(long, default_value = "blob:none")]
        filter: String,

        /// Do not pass a filter flag to git-clone. If both --no-filter and --filter
        /// options are given, --no-filter wins
        #[clap(long)]
        no_filter: bool,

        #[clap(long)]
        bare: bool,

        #[clap(long)]
        sparse: bool,

        #[clap(long)]
        progress: bool,

        #[clap(long)]
        push_url: Option<String>,

        #[clap(long, default_value=operation::init::SOURCE_RO_URL)]
        fetch_url: String,

        #[clap()]
        target_path: String,
    },

    #[clap(hide = true)]
    Maintenance {
        /// The git config key to look for paths of repos to run maintenance in. Defaults to
        /// 'maintenance.repo'
        #[clap(long, default_value=operation::maintenance::DEFAULT_CONFIG_KEY, global = true)]
        git_config_key: String,

        #[clap(subcommand)]
        subcommand: MaintenanceSubcommand,
    },

    /// git-trace allows one to transform the output of GIT_TRACE2_EVENT data into a format
    /// that the chrome://tracing viewer can understand and display. This is a convenient way
    /// to analyze the timing and call tree of a git command.
    ///
    /// For example, to analyze git gc:
    /// ```
    /// $ GIT_TRACE2_EVENT=/tmp/gc.json git gc
    /// $ focus git-trace /tmp/gc.json /tmp/chrome-trace.json
    /// ````
    /// Then open chrome://tracing in your browser and load the /tmp/chrome-trace.json flie.
    GitTrace { input: PathBuf, output: PathBuf },

    /// Upgrade the repository by running outstanding migration steps.
    Upgrade {
        #[clap(long, parse(from_os_str), default_value = ".")]
        repo: PathBuf,
    },

    /// Interact with the on-disk focus index.
    Index {
        #[clap(subcommand)]
        subcommand: IndexSubcommand,
    },
    /// Called by a git hook to trigger certain actions after a git event such as
    /// merge completion or checkout
    Event {
        #[clap(subcommand)]
        subcommand: EventSubcommand,
    },
}

/// Helper method to extract subcommand name. Tool insights client uses this to set
/// feature name.
fn feature_name_for(subcommand: &Subcommand) -> String {
    let subcommand_name = match subcommand {
        Subcommand::Clone { .. } => "clone",
        Subcommand::Sync { .. } => "sync",
        Subcommand::Repo { subcommand } => match subcommand {
            RepoSubcommand::List { .. } => "repo-list",
            RepoSubcommand::Repair { .. } => "repo-repair",
        },
        Subcommand::Add { .. } => "add",
        Subcommand::Remove { .. } => "remove",
        Subcommand::Status { .. } => "status",
        Subcommand::Projects { .. } => "projects",
        Subcommand::DetectBuildGraphChanges { .. } => "detect-build-graph-changes",
        Subcommand::Refs { subcommand, .. } => match subcommand {
            RefsSubcommand::Delete { .. } => "refs-delete",
            RefsSubcommand::ListExpired { .. } => "refs-list-expired",
            RefsSubcommand::ListCurrent { .. } => "refs-list-current",
        },
        Subcommand::Init { .. } => "init",
        Subcommand::Maintenance { subcommand, .. } => match subcommand {
            MaintenanceSubcommand::Run { .. } => "maintenance-run",
            MaintenanceSubcommand::Register { .. } => "maintenance-register",
            MaintenanceSubcommand::SetDefaultConfig { .. } => "maintenance-set-default-config",
            MaintenanceSubcommand::SandboxCleanup { .. } => "maintenance-sandbox-cleanup",
            MaintenanceSubcommand::Schedule { subcommand } => match subcommand {
                MaintenanceScheduleSubcommand::Enable { .. } => "maintenance-schedule-enable",
                MaintenanceScheduleSubcommand::Disable { .. } => "maintenance-schedule-disable",
            },
        },
        Subcommand::GitTrace { .. } => "git-trace",
        Subcommand::Upgrade { .. } => "upgrade",
        Subcommand::Index { subcommand } => match subcommand {
            IndexSubcommand::Clear { .. } => "index-clear",
            IndexSubcommand::Fetch { .. } => "index-fetch",
            IndexSubcommand::Get { .. } => "index-get",
            IndexSubcommand::Generate { .. } => "index-generate",
            IndexSubcommand::Hash { .. } => "index-hash",
            IndexSubcommand::Push { .. } => "index-push",
            IndexSubcommand::Resolve { .. } => "index-resolve",
        },
        Subcommand::Event { subcommand } => match subcommand {
            EventSubcommand::PostCheckout => "event-post-checkout",
            EventSubcommand::PostMerge => "event-post-merge",
        },
    };
    subcommand_name.into()
}

#[derive(Parser, Debug)]
enum MaintenanceSubcommand {
    /// Runs global (i.e. system-wide) git maintenance tasks on repositories listed in
    /// the $HOME/.gitconfig's `maintenance.repo` multi-value config key. This command
    /// is usually run by a system-specific scheduler (eg. launchd) so it's unlikely that
    /// end users would need to invoke this command directly.
    Run {
        /// The absolute path to the git binary to use. If not given, the default MDE path
        /// will be used.
        #[clap(long, default_value = maintenance::DEFAULT_GIT_BINARY_PATH_FOR_SCHEDULED_JOBS, env = "FOCUS_GIT_BINARY_PATH")]
        git_binary_path: PathBuf,

        /// The git config file to use to read the list of repos to run maintenance in. If not
        /// given, then use the default 'global' config which is usually $HOME/.gitconfig.
        #[clap(long, env = "FOCUS_GIT_CONFIG_PATH")]
        git_config_path: Option<PathBuf>,

        /// run maintenance on repos tracked by focus rather than reading from the
        /// git global config file
        #[clap(long, conflicts_with = "git-config-path", env = "FOCUS_TRACKED")]
        tracked: bool,

        /// The time period of job to run
        #[clap(
            long,
            possible_values=operation::maintenance::TimePeriod::VARIANTS,
            default_value="hourly",
            env = "FOCUS_TIME_PERIOD"
        )]
        time_period: operation::maintenance::TimePeriod,
    },

    SetDefaultConfig {},

    Register {
        #[clap(long, parse(from_os_str))]
        repo_path: Option<PathBuf>,

        #[clap(long, parse(from_os_str))]
        git_config_path: Option<PathBuf>,
    },

    Schedule {
        #[clap(subcommand)]
        subcommand: MaintenanceScheduleSubcommand,
    },

    SandboxCleanup {
        /// Sandboxes older than this many hours will be deleted automatically.
        /// if 0 then time based cleanup is not performed and we just go by
        /// max_num_sandboxes.
        #[clap(long)]
        preserve_hours: Option<u32>,

        /// The maximum number of sandboxes we'll allow to exist on disk.
        /// this is computed after we clean up sandboxes that are older
        /// than preserve_hours
        #[clap(long)]
        max_num_sandboxes: Option<u32>,
    },
}

#[derive(Parser, Debug)]
enum MaintenanceScheduleSubcommand {
    /// Set up a system-appropriate periodic job (launchctl, systemd, etc.) for running
    /// maintenance tasks on hourly, daily, and weekly bases
    Enable {
        /// The time period of job to schedule
        #[clap(
            long,
            possible_values=operation::maintenance::TimePeriod::VARIANTS,
            default_value="hourly",
            env = "FOCUS_TIME_PERIOD"
        )]
        time_period: operation::maintenance::TimePeriod,

        /// register jobs for all time periods
        #[clap(long, conflicts_with = "time-period", env = "FOCUS_ALL")]
        all: bool,

        /// path to the focus binary, defaults to the current running focus binary
        #[clap(long)]
        focus_path: Option<PathBuf>,

        /// path to git
        #[clap(long, default_value = operation::maintenance::DEFAULT_GIT_BINARY_PATH_FOR_SCHEDULED_JOBS, env = "FOCUS_GIT_BINARY_PATH")]
        git_binary_path: PathBuf,

        /// Normally, we check to see if the scheduled job is already defined and if it is
        /// we do nothing. IF this flag is given, stop the existing job, remove its definition,
        /// rewrite the job manifest (eg. plist) and reload it.
        #[clap(long, env = "FOCUS_FORCE_RELOAD")]
        force_reload: bool,

        /// Add a flag to the maintenance cmdline that will run the tasks against all focus tracked repos
        #[clap(long, env = "FOCUS_TRACKED")]
        tracked: bool,
    },

    /// Unload all the scheduled jobs from the system scheduler (if loaded).
    Disable {
        /// Delete the plist after unloading
        #[clap(long)]
        delete: bool,
    },
}

#[derive(Parser, Debug)]
enum RepoSubcommand {
    /// List registered repositories
    List {},

    /// Attempt to repair the registry of repositories
    Repair {},
}

#[derive(Parser, Debug)]
enum ProjectSubcommand {
    /// List all available layers
    Available {},

    /// List currently selected layers
    List {},

    /// Push a project onto the top of the stack of currently selected layers
    Push {
        /// Names of layers to push.
        names: Vec<String>,
    },

    /// Pop one or more project(s) from the top of the stack of current selected layers
    Pop {
        /// The number of layers to pop.
        #[clap(long, default_value = "1")]
        count: usize,
    },

    /// Filter out one or more project(s) from the stack of currently selected layers
    Remove {
        /// Names of the layers to be removed.
        names: Vec<String>,
    },
}

#[derive(Parser, Debug)]
enum AdhocSubcommand {
    /// List the contents of the ad-hoc target stack
    List {},

    /// Push one or more target(s) onto the top of the ad-hoc target stack
    Push {
        /// Names of targets to push.
        names: Vec<String>,
    },

    /// Pop one or more targets(s) from the top of the ad-hoc target stack
    Pop {
        /// The number of targets to pop.
        #[clap(long, default_value = "1")]
        count: usize,
    },

    /// Filter out one or more target(s) from the ad-hoc target stack
    Remove {
        /// Names of the targets to be removed.
        names: Vec<String>,
    },
}

#[derive(Parser, Debug)]
enum RefsSubcommand {
    /// Expires refs that are outside the window of "current refs"
    Delete {
        #[clap(long, default_value = "2021-01-01")]
        cutoff_date: String,

        #[clap(long)]
        use_transaction: bool,

        /// If true, then ensure the merge base falls after the cutoff date.
        /// this avoids the problem of refs that refer to commits that are not
        /// included in master
        #[clap(short = 'm', long = "check-merge-base")]
        check_merge_base: bool,
    },

    ListExpired {
        #[clap(long, default_value = "2021-01-01")]
        cutoff_date: String,

        /// If true, then ensure the merge base falls after the cutoff date.
        /// this avoids the problem of refs that refer to commits that are not
        /// included in master
        #[clap(short = 'm', long = "check-merge-base")]
        check_merge_base: bool,
    },

    /// Output a list of still current (I.e. non-expired) refs
    ListCurrent {
        #[clap(long, default_value = "2021-01-01")]
        cutoff_date: String,

        /// If true, then ensure the merge base falls after the cutoff date.
        /// this avoids the problem of refs that refer to commits that are not
        /// included in master
        #[clap(short = 'm', long = "check-merge-base")]
        check_merge_base: bool,
    },
}

#[derive(Parser, Debug)]
enum IndexSubcommand {
    /// Clear the on-disk cache.
    Clear {
        /// Path to the sparse repository.
        #[clap(parse(from_os_str), default_value = ".")]
        sparse_repo: PathBuf,
    },

    /// Fetch the pre-computed index for the repository.
    Fetch {
        /// Path to the sparse repository.
        #[clap(parse(from_os_str), default_value = ".")]
        sparse_repo: PathBuf,

        /// The Git remote to fetch from.
        #[clap(long, default_value = operation::index::INDEX_DEFAULT_REMOTE)]
        remote: String,
    },

    Get {
        target: String,
    },

    /// Populate the index with entries for all projects.
    Generate {
        /// Path to the sparse repository.
        #[clap(parse(from_os_str), default_value = ".")]
        sparse_repo: PathBuf,

        /// If index keys are found to be missing, pause for debugging.
        #[clap(long)]
        break_on_missing_keys: bool,
    },

    /// Calculate and print the content hashes of the provided targets.
    Hash {
        /// The targets to hash.
        targets: Vec<String>,
    },

    /// Generate and push the pre-computed index to the remote store for others
    /// to fetch.
    Push {
        /// Path to the sparse repository.
        #[clap(parse(from_os_str), default_value = ".")]
        sparse_repo: PathBuf,

        /// The Git remote to push to.
        #[clap(long, default_value = operation::index::INDEX_DEFAULT_REMOTE)]
        remote: String,

        /// If index keys are found to be missing, pause for debugging.
        #[clap(long)]
        break_on_missing_keys: bool,
    },

    /// Resolve the targets to their resulting pattern sets.
    Resolve {
        targets: Vec<String>,

        /// If index keys are found to be missing, pause for debugging.
        #[clap(long)]
        break_on_missing_keys: bool,
    },
}

#[derive(Parser, Debug)]
enum EventSubcommand {
    PostCheckout,
    PostMerge,
}

#[derive(Parser, Debug)]
#[clap(about = "Focused Development Tools")]
struct FocusOpts {
    /// Number of threads to use when performing parallel resolution (where possible).
    #[clap(
        long,
        default_value = "0",
        global = true,
        env = "FOCUS_RESOLUTION_THREADS"
    )]
    resolution_threads: usize,

    /// Change to the provided directory before doing anything else.
    #[clap(
        short = 'C',
        long = "work-dir",
        global = true,
        env = "FOCUS_WORKING_DIRECTORY"
    )]
    working_directory: Option<PathBuf>,

    /// Disables use of ANSI color escape sequences
    #[clap(long, global = true, env = "NO_COLOR")]
    no_color: bool,

    #[clap(subcommand)]
    cmd: Subcommand,
}

fn ensure_directories_exist() -> Result<()> {
    Tracker::default()
        .ensure_directories_exist()
        .context("creating directories for the tracker")?;

    Ok(())
}

fn hold_lock_file(repo: &Path) -> Result<LockFile> {
    let path = repo.join(".focus").join("focus.lock");
    LockFile::new(&path)
}

fn run_subcommand(app: Arc<App>, options: FocusOpts) -> Result<ExitCode> {
    let cloned_app = app.clone();
    let ti_client = cloned_app.tool_insights_client();
    let feature_name = feature_name_for(&options.cmd);
    ti_client.get_context().set_tool_feature_name(&feature_name);
    let span = debug_span!("Running subcommand", ?feature_name);
    let _guard = span.enter();

    match options.cmd {
        Subcommand::Clone {
            dense_repo,
            sparse_repo,
            branch,
            days_of_history,
            copy_branches,
            fetch_index,
            index_remote,
            projects_and_targets,
        } => {
            let origin = operation::clone::Origin::try_from(dense_repo.as_str())?;
            let sparse_repo = {
                let current_dir =
                    std::env::current_dir().context("Failed to obtain current directory")?;
                let expanded = paths::expand_tilde(sparse_repo)
                    .context("Failed to expand sparse repo path")?;
                current_dir.join(expanded)
            };

            info!("Cloning {:?} into {}", dense_repo, sparse_repo.display());

            // Add targets length to TI custom map.
            ti_client.get_context().add_to_custom_map(
                "projects_and_targets_count",
                projects_and_targets.len().to_string(),
            );

            operation::clone::run(
                origin,
                sparse_repo.clone(),
                branch,
                projects_and_targets,
                copy_branches,
                days_of_history,
                if fetch_index {
                    Some(index_remote)
                } else {
                    None
                },
                app,
            )?;

            perform_pending_migrations(&sparse_repo)
                .context("Performing initial migrations after clone")?;

            Ok(ExitCode(0))
        }

        Subcommand::Sync {
            sparse_repo,
            fetch_index,
        } => {
            // TODO: Add total number of paths in repo to TI.
            let sparse_repo = paths::expand_tilde(sparse_repo)?;
            ensure_repo_compatibility(&sparse_repo)?;

            let _lock_file = hold_lock_file(&sparse_repo)?;
            operation::sync::run(&sparse_repo, app, fetch_index)?;
            Ok(ExitCode(0))
        }

        Subcommand::Refs {
            repo: repo_path,
            subcommand,
        } => {
            let repo = Repository::open(repo_path).context("opening the repo")?;
            match subcommand {
                RefsSubcommand::Delete {
                    cutoff_date,
                    use_transaction,
                    check_merge_base,
                } => {
                    let cutoff = FocusTime::parse_date(cutoff_date)?;
                    operation::refs::expire_old_refs(
                        &repo,
                        cutoff,
                        check_merge_base,
                        use_transaction,
                        app,
                    )?;
                    Ok(ExitCode(0))
                }

                RefsSubcommand::ListExpired {
                    cutoff_date,
                    check_merge_base,
                } => {
                    let cutoff = FocusTime::parse_date(cutoff_date)?;
                    let operation::refs::PartitionedRefNames {
                        current: _,
                        expired,
                    } = operation::refs::PartitionedRefNames::for_repo(
                        &repo,
                        cutoff,
                        check_merge_base,
                    )?;

                    println!("{}", expired.join("\n"));

                    Ok(ExitCode(0))
                }

                RefsSubcommand::ListCurrent {
                    cutoff_date,
                    check_merge_base,
                } => {
                    let cutoff = FocusTime::parse_date(cutoff_date)?;
                    let operation::refs::PartitionedRefNames {
                        current,
                        expired: _,
                    } = operation::refs::PartitionedRefNames::for_repo(
                        &repo,
                        cutoff,
                        check_merge_base,
                    )?;

                    println!("{}", current.join("\n"));

                    Ok(ExitCode(0))
                }
            }
        }

        Subcommand::Repo { subcommand } => match subcommand {
            RepoSubcommand::List {} => {
                operation::repo::list()?;
                Ok(ExitCode(0))
            }
            RepoSubcommand::Repair {} => {
                operation::repo::repair(app)?;
                Ok(ExitCode(0))
            }
        },

        Subcommand::DetectBuildGraphChanges { repo, args } => {
            let repo = paths::expand_tilde(repo)?;
            let repo = git_helper::find_top_level(app.clone(), &repo)
                .context("Failed to canonicalize repo path")?;
            operation::detect_build_graph_changes::run(&repo, args, app)
        }

        Subcommand::Add {
            fetch_index,
            projects_and_targets,
        } => {
            let sparse_repo = std::env::current_dir()?;
            paths::assert_focused_repo(&sparse_repo)?;
            let _lock_file = hold_lock_file(&sparse_repo)?;
            operation::ensure_clean::run(&sparse_repo, app.clone())
                .context("Ensuring working trees are clean failed")?;
            operation::selection::add(&sparse_repo, true, projects_and_targets, fetch_index, app)?;
            Ok(ExitCode(0))
        }

        Subcommand::Remove {
            fetch_index,
            projects_and_targets,
        } => {
            let sparse_repo = std::env::current_dir()?;
            paths::assert_focused_repo(&sparse_repo)?;
            let _lock_file = hold_lock_file(&sparse_repo)?;
            operation::ensure_clean::run(&sparse_repo, app.clone())
                .context("Ensuring working trees are clean failed")?;
            operation::selection::remove(
                &sparse_repo,
                true,
                projects_and_targets,
                fetch_index,
                app,
            )?;
            Ok(ExitCode(0))
        }

        Subcommand::Status {} => {
            let sparse_repo = std::env::current_dir()?;
            paths::assert_focused_repo(&sparse_repo)?;
            operation::selection::status(&sparse_repo, app)?;
            Ok(ExitCode(0))
        }

        Subcommand::Projects {} => {
            let repo = std::env::current_dir()?;
            paths::assert_focused_repo(&repo)?;
            operation::selection::list_projects(&repo, app)?;
            Ok(ExitCode(0))
        }

        Subcommand::Init {
            shallow_since,
            branch_name,
            no_checkout,
            follow_tags,
            filter,
            no_filter,
            bare,
            sparse,
            progress,
            fetch_url,
            push_url,
            target_path,
        } => {
            let expanded = paths::expand_tilde(target_path)
                .context("expanding tilde on target_path argument")?;

            let target = expanded.as_path();

            let mut init_opts: Vec<operation::init::InitOpt> = Vec::new();

            let mut add_if_true = |n: bool, opt: operation::init::InitOpt| {
                if n {
                    init_opts.push(opt)
                };
            };

            add_if_true(no_checkout, operation::init::InitOpt::NoCheckout);
            add_if_true(bare, operation::init::InitOpt::Bare);
            add_if_true(sparse, operation::init::InitOpt::Sparse);
            add_if_true(follow_tags, operation::init::InitOpt::FollowTags);
            add_if_true(progress, operation::init::InitOpt::Progress);

            info!("Setting up a copy of the repo in {:?}", target);

            operation::init::run(
                shallow_since,
                Some(branch_name),
                if no_filter { None } else { Some(filter) },
                fetch_url,
                push_url,
                target.to_owned(),
                init_opts,
                app,
            )?;

            Ok(ExitCode(0))
        }

        Subcommand::Maintenance {
            subcommand,
            git_config_key,
        } => match subcommand {
            MaintenanceSubcommand::Run {
                git_binary_path,
                tracked,
                git_config_path,
                time_period,
            } => {
                operation::maintenance::run(
                    operation::maintenance::RunOptions {
                        git_binary_path,
                        git_config_key,
                        git_config_path,
                        tracked,
                    },
                    time_period,
                    app,
                )?;

                sandbox::cleanup::run_with_default()?;

                Ok(ExitCode(0))
            }

            MaintenanceSubcommand::Register {
                repo_path,
                git_config_path,
            } => {
                operation::maintenance::register(operation::maintenance::RegisterOpts {
                    repo_path,
                    git_config_key,
                    global_config_path: git_config_path,
                })?;
                Ok(ExitCode(0))
            }

            MaintenanceSubcommand::SetDefaultConfig { .. } => {
                operation::maintenance::set_default_git_maintenance_config(
                    &std::env::current_dir()?,
                )?;
                Ok(ExitCode(0))
            }

            MaintenanceSubcommand::Schedule { subcommand } => match subcommand {
                MaintenanceScheduleSubcommand::Enable {
                    time_period,
                    all,
                    focus_path,
                    git_binary_path,
                    force_reload,
                    tracked,
                } => {
                    maintenance::schedule_enable(maintenance::ScheduleOpts {
                        time_period: if all { None } else { Some(time_period) },
                        git_path: git_binary_path,
                        focus_path: match focus_path {
                            Some(fp) => fp,
                            None => std::env::current_exe()
                                .context("could not determine current executable path")?,
                        },
                        skip_if_already_scheduled: !force_reload,
                        tracked,
                    })?;
                    Ok(ExitCode(0))
                }

                MaintenanceScheduleSubcommand::Disable { delete } => {
                    maintenance::schedule_disable(delete)?;
                    Ok(ExitCode(0))
                }
            },

            MaintenanceSubcommand::SandboxCleanup {
                preserve_hours,
                max_num_sandboxes,
            } => {
                let config = sandbox::cleanup::Config {
                    preserve_hours: preserve_hours
                        .unwrap_or(sandbox::cleanup::Config::DEFAULT_HOURS),
                    max_num_sandboxes: max_num_sandboxes
                        .unwrap_or(sandbox::cleanup::Config::DEFAULT_MAX_NUM_SANDBOXES),
                    ..sandbox::cleanup::Config::try_from_git_default()?
                };

                sandbox::cleanup::run(&config)?;

                Ok(ExitCode(0))
            }
        },

        Subcommand::GitTrace { input, output } => {
            focus_tracing::Trace::git_trace_from(input)?.write_trace_json_to(output)?;
            Ok(ExitCode(0))
        }

        Subcommand::Upgrade { repo } => {
            focus_migrations::production::perform_pending_migrations(&repo)
                .context("Failed to upgrade repo")?;

            Ok(ExitCode(0))
        }

        Subcommand::Index { subcommand } => match subcommand {
            IndexSubcommand::Clear { sparse_repo } => {
                operation::index::clear(sparse_repo)?;
                Ok(ExitCode(0))
            }

            IndexSubcommand::Fetch {
                sparse_repo,
                remote,
            } => {
                let exit_code = operation::index::fetch(app, sparse_repo, remote)?;
                Ok(exit_code)
            }

            IndexSubcommand::Generate {
                sparse_repo,
                break_on_missing_keys,
            } => {
                let exit_code =
                    operation::index::generate(app, sparse_repo, break_on_missing_keys)?;
                Ok(exit_code)
            }

            IndexSubcommand::Get { target } => {
                let exit_code = operation::index::get(app, Path::new("."), &target)?;
                Ok(exit_code)
            }

            IndexSubcommand::Hash { targets } => {
                let exit_code = operation::index::hash(app, Path::new("."), &targets)?;
                Ok(exit_code)
            }

            IndexSubcommand::Push {
                sparse_repo,
                remote,
                break_on_missing_keys,
            } => {
                let exit_code =
                    operation::index::push(app, sparse_repo, remote, break_on_missing_keys)?;
                Ok(exit_code)
            }

            IndexSubcommand::Resolve {
                targets,
                break_on_missing_keys,
            } => {
                let exit_code =
                    operation::index::resolve(app, Path::new("."), targets, break_on_missing_keys)?;
                Ok(exit_code)
            }
        },

        Subcommand::Event { subcommand } => match subcommand {
            EventSubcommand::PostCheckout => Ok(ExitCode(0)),
            EventSubcommand::PostMerge => Ok(ExitCode(0)),
        },
    }
}

fn ensure_repo_compatibility(sparse_repo: &Path) -> Result<()> {
    if focus_migrations::production::is_upgrade_required(sparse_repo)
        .context("Failed to determine whether an upgrade is required")?
    {
        bail!(
            "Repo '{}' needs to be upgraded. Please run `focus upgrade`",
            sparse_repo.display()
        );
    }

    Ok(())
}

fn setup_thread_pool(resolution_threads: usize) -> Result<()> {
    if resolution_threads > 0 {
        rayon::ThreadPoolBuilder::new()
            .num_threads(resolution_threads)
            .build_global()
            .context("Failed to create the task thread pool")?;
    }

    Ok(())
}

// TODO: there needs to be a way to know if we should re-load the plists, (eg. on a version change)
fn setup_maintenance_scheduler(opts: &FocusOpts) -> Result<()> {
    if std::env::var("FOCUS_NO_SCHEDULE").is_ok() {
        return Ok(());
    }

    match opts.cmd {
        Subcommand::Clone { .. }
        | Subcommand::Sync { .. }
        | Subcommand::Add { .. }
        | Subcommand::Remove { .. }
        | Subcommand::Init { .. } => {
            operation::maintenance::schedule_enable(ScheduleOpts::default())
        }
        _ => Ok(()),
    }
}

// Returns a cmd name for a sandbox.
// Returns None if cmd hasn't been determined to need a sandbox prefix.
fn sandbox_name_for_cmd(opts: &FocusOpts) -> Option<&str> {
    match &opts.cmd {
        Subcommand::Maintenance {
            subcommand: MaintenanceSubcommand::Run { .. },
            ..
        } => Some("maintenance_"),
        _ => None,
    }
}

/// Run the main and any destructors. Local variables are not guaranteed to be
/// dropped if `std::process::exit` is called, so make sure to bubble up the
/// return code to the top level, which is the only place in the code that's
/// allowed to call `std::process::exit`.
fn main_and_drop_locals() -> Result<ExitCode> {
    let started_at = Instant::now();
    let options = FocusOpts::parse();

    let FocusOpts {
        resolution_threads,
        working_directory,
        no_color,
        cmd: _,
    } = &options;

    if let Some(working_directory) = working_directory {
        std::env::set_current_dir(working_directory).context("Switching working directory")?;
    }

    let preserve_sandbox = true;

    let app = Arc::from(App::new(preserve_sandbox, sandbox_name_for_cmd(&options))?);
    let ti_context = app.tool_insights_client();

    setup_thread_pool(*resolution_threads)?;

    let is_tty = termion::is_tty(&std::io::stdout());

    let sandbox_dir = app.sandbox().path().to_owned();

    let _guard = focus_tracing::init_tracing(focus_tracing::TracingOpts {
        is_tty,
        no_color: *no_color,
        log_dir: Some(sandbox_dir.to_owned()),
    })?;

    info!(path = ?sandbox_dir, "Created sandbox");

    ensure_directories_exist().context("Failed to create necessary directories")?;
    setup_maintenance_scheduler(&options).context("Failed to setup maintenance scheduler")?;

    let exit_code = match run_subcommand(app.clone(), options) {
        Ok(exit_code) => {
            ti_context
                .get_inner()
                .write_invocation_message(Some(0), None);
            exit_code
        }
        Err(e) => {
            ti_context
                .get_inner()
                .write_invocation_message(Some(1), None);
            return Err(e);
        }
    };

    sandbox::cleanup::run_with_default()?;

    let total_runtime = started_at.elapsed();
    debug!(
        total_runtime_secs = total_runtime.as_secs_f32(),
        "Finished normally"
    );

    Ok(exit_code)
}

fn main() -> Result<()> {
    let ExitCode(exit_code) = main_and_drop_locals()?;
    std::process::exit(exit_code);
}
