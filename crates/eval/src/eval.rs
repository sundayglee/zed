mod example;

use assistant_settings::AssistantSettings;
use client::{Client, UserStore};
pub(crate) use example::*;

use ::fs::RealFs;
use anyhow::{Result, anyhow};
use clap::Parser;
use extension::ExtensionHostProxy;
use futures::future;
use gpui::{App, AppContext, Application, AsyncApp, Entity, SemanticVersion, Task};
use language::LanguageRegistry;
use language_model::{
    AuthenticateError, LanguageModel, LanguageModelProviderId, LanguageModelRegistry,
};
use node_runtime::NodeRuntime;
use project::Project;
use prompt_store::PromptBuilder;
use reqwest_client::ReqwestClient;
use settings::{Settings, SettingsStore};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(name = "eval", disable_version_flag = true)]
struct Args {
    /// Runs all examples that contain these substrings. If unspecified, all examples are run.
    #[arg(value_name = "EXAMPLE_SUBSTRING")]
    examples: Vec<String>,
    /// Model to use (default: "claude-3-7-sonnet-latest")
    #[arg(long, default_value = "claude-3-7-sonnet-latest")]
    model: String,
}

fn main() {
    env_logger::init();

    let args = Args::parse();
    let all_available_examples = list_all_examples().unwrap();
    let example_paths = all_available_examples
        .iter()
        .filter_map(|example_path| {
            let name = example_path.file_name()?.to_string_lossy();
            if args.examples.is_empty()
                || args
                    .examples
                    .iter()
                    .any(|name_substring| name.contains(name_substring))
            {
                Some(example_path.clone())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    let http_client = Arc::new(ReqwestClient::new());
    let app = Application::headless().with_http_client(http_client.clone());

    app.run(move |cx| {
        let app_state = init(cx);

        let model = find_model("claude-3-7-sonnet-latest", cx).unwrap();

        LanguageModelRegistry::global(cx).update(cx, |registry, cx| {
            registry.set_default_model(Some(model.clone()), cx);
        });

        let model_provider_id = model.provider_id();

        let authenticate = authenticate_model_provider(model_provider_id.clone(), cx);

        cx.spawn(async move |cx| {
            authenticate.await.unwrap();

            let mut examples = Vec::new();
            for example_path in example_paths {
                let example = Example::load_from_directory(&example_path)?;
                examples.push((example_path, example));
            }

            std::fs::create_dir_all(REPOS_DIR)?;
            std::fs::create_dir_all(WORKTREES_DIR)?;

            let mut repo_urls = HashSet::new();

            let mut clone_tasks = Vec::new();

            for (_, example) in examples.iter() {
                let repo_url = example.base.url.clone();
                if repo_urls.insert(repo_url.clone()) {
                    let repo_path = repo_path_for_url(&repo_url);
                    if !repo_path.join(".git").is_dir() {
                        let git_task = cx.spawn(async move |_cx| {
                            std::fs::create_dir_all(&repo_path)?;
                            run_git(&repo_path, &["init"]).await?;
                            run_git(&repo_path, &["remote", "add", "origin", &repo_url]).await
                        });

                        clone_tasks.push(git_task);
                    } else {
                        let actual_origin =
                            run_git(&repo_path, &["remote", "get-url", "origin"]).await?;
                        if actual_origin != repo_url {
                            return Err(anyhow!(
                                "remote origin {} does not match expected origin {}",
                                actual_origin,
                                repo_url,
                            ));
                        }
                    }
                }
            }

            future::join_all(clone_tasks).await;

            let tasks = examples
                .into_iter()
                .map(|(example_path, example)| {
                    let app_state = app_state.clone();
                    let model = model.clone();
                    cx.spawn(async move |cx| {
                        (
                            example_path,
                            run_example(example, model, app_state, cx).await,
                        )
                    })
                })
                .collect::<Vec<_>>();

            let results: Vec<(PathBuf, Result<JudgeOutput>)> = future::join_all(tasks).await;

            println!("\n\n");
            println!("========================================");
            println!("              EVAL RESULTS              ");
            println!("========================================");
            println!("");

            let mut judge_scores = Vec::new();

            for (example_path, result) in results {
                let example_name = example_path.file_name().unwrap().to_string_lossy();
                match result {
                    Err(err) => {
                        println!("💥 {:<30}: {:?}", example_name, err);
                    }
                    Ok(judge_output) => {
                        const SCORES: [&str; 6] = ["💀", "😭", "😔", "😐", "🙂", "🤩"];

                        println!(
                            "{} {:<30}: {}",
                            SCORES[judge_output.score.min(5) as usize],
                            example_name,
                            judge_output.score,
                        );
                        judge_scores.push(judge_output.score);
                    }
                }
            }

            let score_count = judge_scores.len();
            let average_score = judge_scores
                .into_iter()
                .map(|score| score as f32)
                .sum::<f32>()
                / (score_count as f32);
            println!("\nAverage score: {average_score}");

            cx.update(|cx| cx.quit())
        })
        .detach_and_log_err(cx);
    });
}

async fn run_example(
    example: Example,
    model: Arc<dyn LanguageModel>,
    app_state: Arc<AgentAppState>,
    cx: &mut AsyncApp,
) -> Result<JudgeOutput> {
    example.setup().await?;
    cx.update(|cx| example.run(model.clone(), app_state, cx))?
        .await?;
    let diff = example.repository_diff().await?;
    example.judge(model, diff, cx).await
}

fn list_all_examples() -> Result<Vec<PathBuf>> {
    let path = std::fs::canonicalize(EXAMPLES_DIR).unwrap();
    let entries = std::fs::read_dir(path).unwrap();
    let mut result_paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            result_paths.push(path);
        }
    }
    Ok(result_paths)
}

/// Subset of `workspace::AppState` needed by `HeadlessAssistant`, with additional fields.
pub struct AgentAppState {
    pub languages: Arc<LanguageRegistry>,
    pub client: Arc<Client>,
    pub user_store: Entity<UserStore>,
    pub fs: Arc<dyn fs::Fs>,
    pub node_runtime: NodeRuntime,

    // Additional fields not present in `workspace::AppState`.
    pub prompt_builder: Arc<PromptBuilder>,
}

pub fn init(cx: &mut App) -> Arc<AgentAppState> {
    release_channel::init(SemanticVersion::default(), cx);
    gpui_tokio::init(cx);

    let mut settings_store = SettingsStore::new(cx);
    settings_store
        .set_default_settings(settings::default_settings().as_ref(), cx)
        .unwrap();
    cx.set_global(settings_store);
    client::init_settings(cx);
    Project::init_settings(cx);

    let client = Client::production(cx);
    cx.set_http_client(client.http_client().clone());

    let git_binary_path = None;
    let fs = Arc::new(RealFs::new(
        git_binary_path,
        cx.background_executor().clone(),
    ));

    let languages = Arc::new(LanguageRegistry::new(cx.background_executor().clone()));

    let user_store = cx.new(|cx| UserStore::new(client.clone(), cx));

    extension::init(cx);

    // todo! actually configure node runtime?
    let node_runtime = NodeRuntime::unavailable();
    let extension_host_proxy = ExtensionHostProxy::global(cx);

    language::init(cx);
    language_extension::init(extension_host_proxy.clone(), languages.clone());
    language_model::init(client.clone(), cx);
    language_models::init(user_store.clone(), client.clone(), fs.clone(), cx);
    languages::init(languages.clone(), node_runtime.clone(), cx);
    assistant_tools::init(client.http_client().clone(), cx);
    context_server::init(cx);
    let stdout_is_a_pty = false;
    let prompt_builder = PromptBuilder::load(fs.clone(), stdout_is_a_pty, cx);
    agent::init(fs.clone(), client.clone(), prompt_builder.clone(), cx);

    AssistantSettings::override_global(
        AssistantSettings {
            always_allow_tool_actions: true,
            ..AssistantSettings::get_global(cx).clone()
        },
        cx,
    );

    Arc::new(AgentAppState {
        languages,
        client,
        user_store,
        fs,
        node_runtime,
        prompt_builder,
    })
}

pub fn find_model(model_name: &str, cx: &App) -> anyhow::Result<Arc<dyn LanguageModel>> {
    let model_registry = LanguageModelRegistry::read_global(cx);
    let model = model_registry
        .available_models(cx)
        .find(|model| model.id().0 == model_name);

    let Some(model) = model else {
        return Err(anyhow!(
            "No language model named {} was available. Available models: {}",
            model_name,
            model_registry
                .available_models(cx)
                .map(|model| model.id().0.clone())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    };

    Ok(model)
}

pub fn authenticate_model_provider(
    provider_id: LanguageModelProviderId,
    cx: &mut App,
) -> Task<std::result::Result<(), AuthenticateError>> {
    let model_registry = LanguageModelRegistry::read_global(cx);
    let model_provider = model_registry.provider(&provider_id).unwrap();
    model_provider.authenticate(cx)
}
