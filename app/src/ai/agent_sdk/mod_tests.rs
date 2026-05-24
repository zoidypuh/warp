use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use ai::api_keys::LocalOpenAIEndpointConfig;
use serde_json::json;
use warp_cli::agent::Harness;
use warp_cli::artifact::{
    ArtifactCommand, DownloadArtifactArgs, GetArtifactArgs, UploadArtifactArgs,
};
use warp_cli::task::{MessageCommand, MessageSendArgs, MessageWatchArgs, TaskCommand};
use warp_cli::CliCommand;
use warp_core::telemetry::TelemetryEvent;
use warp_managed_secrets::ManagedSecretValue;

use super::driver::{AgentDriverOptions, AgentRunPrompt, Task};
use super::{
    apply_local_openai_endpoint_config, command_requires_auth, command_to_telemetry_event,
    reconcile_task_harness,
};
use crate::ai::agent_sdk::driver::harness::harness_kind;
use crate::ai::ambient_agents::task::{AgentConfigSnapshot, HarnessConfig, HarnessModelConfig};

const TASK_ID: &str = "00000000-0000-0000-0000-000000000001";

#[test]
fn logout_does_not_require_auth() {
    assert!(!command_requires_auth(&CliCommand::Logout));
}

#[test]
fn login_does_not_require_auth() {
    assert!(!command_requires_auth(&CliCommand::Login));
}

#[test]
fn artifact_download_requires_auth() {
    assert!(command_requires_auth(&CliCommand::Artifact(
        ArtifactCommand::Download(DownloadArtifactArgs {
            artifact_uid: "artifact-123".to_string(),
            out: None,
        },)
    )));
}

#[test]
fn run_message_send_requires_auth() {
    assert!(command_requires_auth(&CliCommand::Run(
        TaskCommand::Message(MessageCommand::Send(MessageSendArgs {
            to: vec!["run-456".to_string()],
            subject: "subject".to_string(),
            body: "body".to_string(),
            sender_run_id: "run-123".to_string(),
        }),)
    )));
}

#[test]
fn artifact_get_requires_auth() {
    assert!(command_requires_auth(&CliCommand::Artifact(
        ArtifactCommand::Get(GetArtifactArgs {
            artifact_uid: "artifact-123".to_string(),
        },)
    )));
}

#[test]
fn artifact_upload_requires_auth() {
    assert!(command_requires_auth(&CliCommand::Artifact(
        ArtifactCommand::Upload(UploadArtifactArgs {
            path: "artifact.txt".into(),
            run_id: Some("run-123".to_string()),
            conversation_id: None,
            description: None,
        },)
    )));
}

#[test]
#[serial_test::serial]
fn run_message_send_telemetry_uses_canonical_harness_from_env() {
    std::env::set_var("OZ_HARNESS", "  CLAUDE  ");
    let event = command_to_telemetry_event(&CliCommand::Run(TaskCommand::Message(
        MessageCommand::Send(MessageSendArgs {
            to: vec!["run-456".to_string()],
            subject: "subject".to_string(),
            body: "body".to_string(),
            sender_run_id: "run-123".to_string(),
        }),
    )));
    std::env::remove_var("OZ_HARNESS");

    assert_eq!(event.payload(), Some(json!({ "harness": "claude" })));
}

#[test]
#[serial_test::serial]
fn run_message_send_telemetry_supports_claude_code_alias() {
    std::env::set_var("OZ_HARNESS", "CLAUDE_CODE");
    let event = command_to_telemetry_event(&CliCommand::Run(TaskCommand::Message(
        MessageCommand::Send(MessageSendArgs {
            to: vec!["run-456".to_string()],
            subject: "subject".to_string(),
            body: "body".to_string(),
            sender_run_id: "run-123".to_string(),
        }),
    )));
    std::env::remove_var("OZ_HARNESS");

    assert_eq!(event.payload(), Some(json!({ "harness": "claude" })));
}

#[test]
#[serial_test::serial]
fn run_message_send_telemetry_supports_opencode_harness() {
    std::env::set_var("OZ_HARNESS", "opencode");
    let event = command_to_telemetry_event(&CliCommand::Run(TaskCommand::Message(
        MessageCommand::Send(MessageSendArgs {
            to: vec!["run-456".to_string()],
            subject: "subject".to_string(),
            body: "body".to_string(),
            sender_run_id: "run-123".to_string(),
        }),
    )));
    std::env::remove_var("OZ_HARNESS");

    assert_eq!(event.payload(), Some(json!({ "harness": "opencode" })));
}

#[test]
#[serial_test::serial]
fn run_message_send_telemetry_defaults_to_unknown_harness() {
    std::env::remove_var("OZ_HARNESS");
    let event = command_to_telemetry_event(&CliCommand::Run(TaskCommand::Message(
        MessageCommand::Send(MessageSendArgs {
            to: vec!["run-456".to_string()],
            subject: "subject".to_string(),
            body: "body".to_string(),
            sender_run_id: "run-123".to_string(),
        }),
    )));

    assert_eq!(event.payload(), Some(json!({ "harness": "unknown" })));
}

#[test]
fn reconcile_task_harness_adopts_task_harness_when_cli_uses_default() {
    let mut selected_harness = Harness::Oz;
    let harness = reconcile_task_harness(TASK_ID, &mut selected_harness, Harness::Claude)
        .expect("default harness should adopt task harness");

    assert_eq!(selected_harness, Harness::Claude);
    assert_eq!(harness.harness(), Harness::Claude);
}

#[test]
fn reconcile_task_harness_allows_matching_explicit_harness() {
    let mut selected_harness = Harness::Claude;
    let harness = reconcile_task_harness(TASK_ID, &mut selected_harness, Harness::Claude)
        .expect("matching harness should succeed");

    assert_eq!(selected_harness, Harness::Claude);
    assert_eq!(harness.harness(), Harness::Claude);
}

#[test]
fn reconcile_task_harness_rejects_explicit_mismatch() {
    let mut selected_harness = Harness::Gemini;
    let err = reconcile_task_harness(TASK_ID, &mut selected_harness, Harness::Claude)
        .expect_err("mismatched harness should fail");

    assert_eq!(selected_harness, Harness::Gemini);
    assert!(err.to_string().contains("Task"));
    assert!(err.to_string().contains("--harness gemini"));
    assert!(err.to_string().contains("claude"));
}

fn local_openai_endpoint_config() -> LocalOpenAIEndpointConfig {
    LocalOpenAIEndpointConfig {
        enabled: true,
        base_url: "http://127.0.0.1:8317/v1".to_string(),
        api_key: "local-key".to_string(),
        model_id: "gpt-local".to_string(),
    }
}

fn driver_options_for_harness(
    selected_harness: Harness,
    model_config: Option<HarnessModelConfig>,
) -> AgentDriverOptions {
    AgentDriverOptions {
        working_dir: PathBuf::from("/tmp"),
        secrets: HashMap::new(),
        task_id: None,
        parent_run_id: None,
        should_share: false,
        idle_on_complete: None,
        resume: None,
        cloud_providers: Vec::new(),
        environment: None,
        selected_harness,
        third_party_harness_model_config: model_config,
        snapshot_disabled: None,
        snapshot_upload_timeout: Some(Duration::from_secs(1)),
        snapshot_script_timeout: Some(Duration::from_secs(1)),
    }
}

fn task_for_harness(harness: Harness) -> Task {
    Task {
        prompt: AgentRunPrompt::Local("hello".to_string()),
        model: None,
        profile: None,
        mcp_specs: Vec::new(),
        harness: harness_kind(harness).expect("test harness should be valid"),
    }
}

#[test]
fn local_openai_endpoint_promotes_new_default_run_to_codex() {
    let local_endpoint = local_openai_endpoint_config();
    let mut driver_options = driver_options_for_harness(Harness::Oz, None);
    let mut task = task_for_harness(Harness::Oz);
    let mut merged_config = AgentConfigSnapshot {
        model_id: Some("gpt-5".to_string()),
        ..Default::default()
    };

    apply_local_openai_endpoint_config(
        &mut driver_options,
        &mut task,
        Some(&mut merged_config),
        &local_endpoint,
    )
    .expect("local endpoint config should apply");

    assert_eq!(driver_options.selected_harness, Harness::Codex);
    assert_eq!(task.harness.harness(), Harness::Codex);
    assert_eq!(
        driver_options.third_party_harness_model_config,
        Some(HarnessModelConfig {
            model_id: "gpt-local".to_string(),
            reasoning_level: None,
        })
    );
    assert_eq!(
        merged_config.harness,
        Some(HarnessConfig {
            harness_type: Harness::Codex,
            model_id: Some("gpt-local".to_string()),
            reasoning_level: None,
        })
    );
    assert_eq!(merged_config.model_id, None);
    match driver_options.secrets.get("local-openai-endpoint") {
        Some(ManagedSecretValue::OpenaiApiKey { api_key, base_url }) => {
            assert_eq!(api_key, "local-key");
            assert_eq!(base_url.as_deref(), Some("http://127.0.0.1:8317/v1"));
        }
        other => panic!("unexpected local endpoint secret: {other:?}"),
    }
}

#[test]
fn local_openai_endpoint_keeps_explicit_codex_model() {
    let local_endpoint = local_openai_endpoint_config();
    let explicit_model = HarnessModelConfig {
        model_id: "gpt-explicit".to_string(),
        reasoning_level: Some("high".to_string()),
    };
    let mut driver_options =
        driver_options_for_harness(Harness::Codex, Some(explicit_model.clone()));
    let mut task = task_for_harness(Harness::Codex);
    let mut merged_config = AgentConfigSnapshot {
        harness: Some(HarnessConfig {
            harness_type: Harness::Codex,
            model_id: Some("gpt-explicit".to_string()),
            reasoning_level: Some("high".to_string()),
        }),
        ..Default::default()
    };

    apply_local_openai_endpoint_config(
        &mut driver_options,
        &mut task,
        Some(&mut merged_config),
        &local_endpoint,
    )
    .expect("local endpoint config should apply");

    assert_eq!(driver_options.selected_harness, Harness::Codex);
    assert_eq!(
        driver_options.third_party_harness_model_config,
        Some(explicit_model)
    );
    assert_eq!(
        merged_config
            .harness
            .as_ref()
            .and_then(|h| h.model_id.clone()),
        Some("gpt-explicit".to_string())
    );
    assert!(driver_options.secrets.contains_key("local-openai-endpoint"));
}

#[test]
fn local_openai_endpoint_ignores_explicit_non_codex_harness() {
    let local_endpoint = local_openai_endpoint_config();
    let mut driver_options = driver_options_for_harness(Harness::Claude, None);
    let mut task = task_for_harness(Harness::Claude);
    let mut merged_config = AgentConfigSnapshot {
        harness: Some(HarnessConfig::from_harness_type(Harness::Claude)),
        ..Default::default()
    };

    apply_local_openai_endpoint_config(
        &mut driver_options,
        &mut task,
        Some(&mut merged_config),
        &local_endpoint,
    )
    .expect("non-Codex harness should ignore local endpoint");

    assert_eq!(driver_options.selected_harness, Harness::Claude);
    assert_eq!(task.harness.harness(), Harness::Claude);
    assert!(driver_options.secrets.is_empty());
    assert_eq!(
        merged_config.harness,
        Some(HarnessConfig::from_harness_type(Harness::Claude))
    );
}

#[test]
#[serial_test::serial]
fn run_message_watch_telemetry_defaults_to_unknown_harness() {
    std::env::remove_var("OZ_HARNESS");
    let event = command_to_telemetry_event(&CliCommand::Run(TaskCommand::Message(
        MessageCommand::Watch(MessageWatchArgs {
            run_id: "run-123".to_string(),
            since_sequence: 0,
        }),
    )));

    assert_eq!(event.payload(), Some(json!({ "harness": "unknown" })));
}
