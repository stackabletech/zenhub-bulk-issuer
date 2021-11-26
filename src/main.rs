use std::{io::Read, path::Path};

use anyhow::Context;
use graphql_client::GraphQLQuery;
use reqwest::header::{HeaderMap, HeaderValue};
use structopt::StructOpt;
use tempfile::NamedTempFile;
use tokio::{fs::File, io::AsyncReadExt, process::Command};

const ZENHUB_API: &str = "https://api.zenhub.com/v1/graphql";

#[derive(StructOpt)]
struct Opts {
    /// The target workspace ID
    ///
    /// For the URL https://app.zenhub.com/workspaces/test-ws-61a0be55e9e28d000fed8bde/board, this will be `61a0be55e9e28d000fed8bde`
    #[structopt(long)]
    workspace: String,
    /// The title of the created issues
    #[structopt(long)]
    title: String,
    /// The body of the created issues
    ///
    /// If this is not specified then your default editor (as set in $VISUAL or $EDITOR) will be opened
    #[structopt(long)]
    body: Option<String>,
    /// The labels to be added to the created issues
    #[structopt(long = "label")]
    labels: Vec<String>,
    /// The names of the sprints that should be added to the created issues
    ///
    /// For example: `Sprint: Nov 26 - Nov 29`
    #[structopt(long = "sprint")]
    sprints: Vec<String>,
    /// The epics that should be added to the created issues, formatted as owner/repo#123
    #[structopt(long = "epic")]
    epics: Vec<String>,

    /// The authentication token to be used for authenticating to the ZenHub API
    ///
    /// This is *not* your API token. To find out yours, go to https://app.zenhub.com/ and look at the header `x-authentication-token`
    /// sent to https://api.zenhub.com/v1/graphql?query=trackEvent.
    #[structopt(long, env = "ZENHUB_TOKEN")]
    zenhub_token: String,
}

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "zenhub-schema-fetcher/schema.graphql",
    query_path = "zenhub-queries.graphql",
    response_derives = "Debug"
)]
struct ZenhubStateQuery;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "zenhub-schema-fetcher/schema.graphql",
    query_path = "zenhub-queries.graphql",
    response_derives = "Debug"
)]
struct ZenhubCreateIssue;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "zenhub-schema-fetcher/schema.graphql",
    query_path = "zenhub-queries.graphql",
    response_derives = "Debug"
)]
struct ZenhubAddIssuesToEpics;

#[derive(GraphQLQuery)]
#[graphql(
    schema_path = "zenhub-schema-fetcher/schema.graphql",
    query_path = "zenhub-queries.graphql",
    response_derives = "Debug"
)]
struct ZenhubAddIssuesToSprints;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().init();
    let opts = Opts::from_args();
    let body = match opts.body {
        Some(body) => match safe_split_str_at(&body, 1) {
            ("@", path) => read_file_to_str(path)
                .await
                .with_context(|| format!("failed to load body from {}", path))?,
            _ => body,
        },
        None => open_in_editor()
            .await
            .context("failed to set issue body in editor")?,
    };
    let zenhub_headers = {
        let mut map = HeaderMap::new();
        map.insert(
            "x-authentication-token",
            HeaderValue::from_str(&opts.zenhub_token).context("invalid zenhub token")?,
        );
        map
    };
    let reqwest = reqwest::Client::builder()
        .default_headers(zenhub_headers)
        .build()?;
    let workspace = graphql_client::reqwest::post_graphql::<ZenhubStateQuery, _>(
        &reqwest,
        ZENHUB_API,
        zenhub_state_query::Variables {
            workspace: opts.workspace,
        },
    )
    .await
    .context("failed to retrieve current zenhub state")?
    .data
    .context("no response for zenhub state query (is your zenhub token valid?)")?
    .workspace
    .context("no workspace found")?;
    let sprint_ids = resolve_sprint_ids(&opts.sprints, &workspace)?;
    let epic_ids = resolve_epic_ids(&opts.epics, &workspace)?;
    let mut issue_ids = Vec::new();
    for repo in workspace
        .repositories_connection
        .into_iter()
        .flat_map(|repo| repo.nodes)
    {
        match create_issue(&reqwest, &repo, &opts.title, &body, &opts.labels).await {
            Ok(issue) => {
                tracing::info!(
                    issue = format_args!("{}/{}#{}", repo.owner_name, repo.name, issue.number),
                    "Created issue in repo"
                );
                issue_ids.push(issue.id);
            }
            Err(err) => {
                tracing::warn!(
                    repository = format_args!("{}/{}", repo.owner_name, repo.name),
                    error = err.as_ref() as &(dyn std::error::Error + 'static),
                    "Failed to create issue in repo, skipping"
                )
            }
        }
    }
    tag_issues(&reqwest, issue_ids, epic_ids, sprint_ids).await?;
    Ok(())
}

fn resolve_sprint_ids(
    sprint_names: &[String],
    workspace: &zenhub_state_query::ZenhubStateQueryWorkspace,
) -> Result<Vec<String>, anyhow::Error> {
    sprint_names
        .iter()
        .map(|sprint_name| {
            workspace
                .sprints
                .nodes
                .iter()
                .find(|sprint_candidate| sprint_candidate.name.as_deref() == Some(sprint_name))
                .with_context(|| format!("could not find sprint named {:?}", sprint_name))
                .map(|sprint| sprint.id.clone())
        })
        .collect::<anyhow::Result<Vec<_>>>()
}

fn resolve_epic_ids(
    epic_refs: &[String],
    workspace: &zenhub_state_query::ZenhubStateQueryWorkspace,
) -> Result<Vec<String>, anyhow::Error> {
    epic_refs
        .iter()
        .map(|epic_ref| {
            workspace
                .epics
                .iter()
                .flat_map(|epics| &epics.nodes)
                .find(|epic_candidate| {
                    &format!(
                        "{}/{}#{}",
                        epic_candidate.issue.repository.owner_name,
                        epic_candidate.issue.repository.name,
                        epic_candidate.issue.number
                    ) == epic_ref
                })
                .with_context(|| {
                    format!(
                        "could not find epic with ref {:?} (should be of format owner/repo#123)",
                        epic_ref
                    )
                })
                .map(|epic| epic.id.clone())
        })
        .collect()
}

async fn create_issue(
    reqwest: &reqwest::Client,
    repo: &zenhub_state_query::ZenhubStateQueryWorkspaceRepositoriesConnectionNodes,
    title: &str,
    body: &str,
    labels: &[String],
) -> anyhow::Result<zenhub_create_issue::ZenhubCreateIssueCreateIssueIssue> {
    Ok(
        graphql_client::reqwest::post_graphql::<ZenhubCreateIssue, _>(
            reqwest,
            ZENHUB_API,
            zenhub_create_issue::Variables {
                input: zenhub_create_issue::CreateIssueInput {
                    title: title.to_string(),
                    body: Some(body.to_string()),
                    assignees: None,
                    clientMutationId: None,
                    labels: Some(labels.to_vec()),
                    milestone: None,
                    repositoryId: repo.id.clone(),
                },
            },
        )
        .await?
        .data
        .context("no response creating issue")?
        .create_issue
        .context("no metadata for created issue")?
        .issue,
    )
}

async fn tag_issues(
    reqwest: &reqwest::Client,
    issue_ids: Vec<String>,
    epic_ids: Vec<String>,
    sprint_ids: Vec<String>,
) -> Result<(), anyhow::Error> {
    if !epic_ids.is_empty() {
        graphql_client::reqwest::post_graphql::<ZenhubAddIssuesToEpics, _>(
            reqwest,
            ZENHUB_API,
            zenhub_add_issues_to_epics::Variables {
                issue_ids: issue_ids.clone(),
                epic_ids,
            },
        )
        .await?
        .data
        .context("no response adding issues to epics")?;
    }
    if !sprint_ids.is_empty() {
        graphql_client::reqwest::post_graphql::<ZenhubAddIssuesToSprints, _>(
            reqwest,
            ZENHUB_API,
            zenhub_add_issues_to_sprints::Variables {
                issue_ids,
                sprint_ids,
            },
        )
        .await?
        .data
        .context("no response adding issues to sprints")?;
    }
    Ok(())
}

async fn open_in_editor() -> Result<String, anyhow::Error> {
    let editor = std::env::var_os("VISUAL")
        .or_else(|| std::env::var_os("EDITOR"))
        .context("no default editor could be found, please set $VISUAL or $EDITOR to your preferred text editor")?;
    let mut file = NamedTempFile::new().context("failed to create temp file for editor")?;
    tracing::info!(
        ?editor,
        path = ?file.path(),
        "Launching editor, waiting for it to finish..."
    );
    Command::new(editor)
        .arg(file.path())
        .spawn()
        .context("failed to launch editor")?
        .wait()
        .await
        .context("editor failed")?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)
        .context("failed to read editor tempfile")?;
    Ok(buf)
}

async fn read_file_to_str(path: impl AsRef<Path>) -> Result<String, anyhow::Error> {
    let mut buf = String::new();
    File::open(path).await?.read_to_string(&mut buf).await?;
    Ok(buf)
}

fn safe_split_str_at(full: &str, mut mid: usize) -> (&str, &str) {
    // Clamp to length
    mid = mid.min(full.len());
    // Round down if not a char boundary
    while mid != 0 && !full.is_char_boundary(mid) {
        mid -= 1;
    }
    full.split_at(mid.min(full.len()))
}
