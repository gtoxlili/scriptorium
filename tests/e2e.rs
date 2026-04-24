//! End-to-end integration tests for scriptorium.
//!
//! These tests spin up the gRPC service in-process against a local Docker
//! daemon (OrbStack on the author's Mac), make real exec calls, and assert
//! the resulting behaviour. They are gated behind `#[ignore]` because they
//! require:
//!   1. A reachable Docker daemon.
//!   2. The `scriptorium-sandbox:debian13-v1` image (built via
//!      `docker build -f docker/sandbox.Dockerfile -t scriptorium-sandbox:debian13-v1 .`).
//!
//! Tests that also need TOS credentials use the `SCRIPTORIUM_TEST_TOS=1`
//! env guard on top of that — see individual test docs.
//!
//! Run with `cargo test --test e2e -- --ignored --nocapture`.
#![allow(clippy::doc_markdown)]

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use scriptorium::{
    config::{
        ConcurrencyConfig, DockerConfig, FetchConfig, SandboxConfig, TosConfig, WorkspaceConfig,
    },
    oss::OssClient,
    pb::{
        CallToolRequest, DeleteWorkspaceRequest, ExecRequest, HealthRequest, ListFilesRequest,
        ListToolsRequest, exec_event, sandbox_client::SandboxClient, sandbox_server::SandboxServer,
    },
    runtime::DockerRuntime,
    service::SandboxService,
    workspace::WorkspaceManager,
};
use tempfile::TempDir;
use tokio_stream::{StreamExt, wrappers::TcpListenerStream};
use tonic::transport::{Channel, Server};

/// OrbStack default socket on macOS. Override with `DOCKER_HOST=unix:///path`.
fn discover_docker_socket() -> PathBuf {
    if let Ok(host) = std::env::var("DOCKER_HOST") {
        if let Some(path) = host.strip_prefix("unix://") {
            return PathBuf::from(path);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    PathBuf::from(format!("{home}/.orbstack/run/docker.sock"))
}

fn sandbox_cfg_for_tests() -> SandboxConfig {
    SandboxConfig {
        default_image: std::env::var("SCRIPTORIUM_TEST_IMAGE")
            .unwrap_or_else(|_| "scriptorium-sandbox:debian13-v1".to_string()),
        default_timeout_seconds: 30,
        default_cpu_millis: 2000,
        default_memory_bytes: 2 * 1024 * 1024 * 1024,
        default_pids: 256,
        default_tmpfs_bytes: 256 * 1024 * 1024,
        heavy_cpu_millis: 4000,
        heavy_memory_bytes: 4 * 1024 * 1024 * 1024,
        heavy_pids: 512,
        heavy_tmpfs_bytes: 512 * 1024 * 1024,
        agent_uid: 1000,
        agent_gid: 1000,
        workspace_mount: PathBuf::from("/home/agent"),
    }
}

fn concurrency_cfg_for_tests() -> ConcurrencyConfig {
    ConcurrencyConfig {
        max_concurrent_execs: 4,
        exec_queue_timeout_seconds: 10,
    }
}

/// Placeholder TOS config — non-OSS tests never hit it, so dummy creds are
/// fine. Tests that do exercise `upload_to_oss` override fields from env.
fn tos_cfg_for_tests() -> TosConfig {
    TosConfig {
        endpoint: std::env::var("SCRIPTORIUM_TEST_TOS_ENDPOINT")
            .unwrap_or_else(|_| "https://tos-s3-cn-shanghai.volces.com".to_string()),
        region: std::env::var("SCRIPTORIUM_TEST_TOS_REGION")
            .unwrap_or_else(|_| "cn-shanghai".to_string()),
        bucket: std::env::var("SCRIPTORIUM_TEST_TOS_BUCKET")
            .unwrap_or_else(|_| "dummy-bucket".to_string()),
        access_key: std::env::var("SCRIPTORIUM_TEST_TOS_ACCESS_KEY")
            .unwrap_or_else(|_| "dummy-access-key".to_string()),
        secret_key: std::env::var("SCRIPTORIUM_TEST_TOS_SECRET_KEY")
            .unwrap_or_else(|_| "dummy-secret-key".to_string()),
        key_prefix: "scriptorium-test/".to_string(),
        signed_url_expires_seconds: 600,
        signed_url_max_seconds: 3600,
        part_size_bytes: 8 * 1024 * 1024,
        multipart_threshold_bytes: 64 * 1024 * 1024,
        upload_timeout_seconds: 120,
    }
}

/// Boot an in-process SandboxService listening on a random port. Returns
/// (`bound_addr`, `tmp_root`); dropping `tmp_root` wipes the workspace state.
async fn spawn_service() -> (SocketAddr, TempDir) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let docker_cfg = DockerConfig {
        socket: discover_docker_socket(),
        control_timeout_seconds: 30,
    };
    let sandbox_cfg = sandbox_cfg_for_tests();
    let workspace_cfg = WorkspaceConfig {
        root: tmp.path().to_path_buf(),
    };

    let runtime = DockerRuntime::connect(docker_cfg, sandbox_cfg)
        .await
        .expect("docker connect");
    let workspaces = WorkspaceManager::new(workspace_cfg);
    workspaces.ensure_root().await.expect("ensure_root");
    let oss = OssClient::connect(&tos_cfg_for_tests()).expect("oss connect");
    let svc = SandboxService::new(
        runtime,
        workspaces,
        oss,
        FetchConfig::default(),
        &concurrency_cfg_for_tests(),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = TcpListenerStream::new(listener);

    tokio::spawn(async move {
        Server::builder()
            .add_service(SandboxServer::new(svc))
            .serve_with_incoming(incoming)
            .await
            .expect("server exited");
    });

    // Give the server a tick to settle before the client dials.
    tokio::time::sleep(Duration::from_millis(100)).await;
    (addr, tmp)
}

async fn client(addr: SocketAddr) -> SandboxClient<Channel> {
    let endpoint = format!("http://{addr}");
    SandboxClient::connect(endpoint)
        .await
        .expect("client connect")
}

// ─── Tests ────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local docker + scriptorium-sandbox image"]
async fn health_reports_docker_reachable() {
    let (addr, _tmp) = spawn_service().await;
    let mut client = client(addr).await;
    let resp = client.health(HealthRequest {}).await.unwrap().into_inner();
    assert!(resp.docker_reachable, "docker must be reachable");
    assert!(!resp.version.is_empty());
    assert!(resp.exec_permits_available > 0);
    println!(
        "docker_version = {}, permits = {}",
        resp.docker_version, resp.exec_permits_available
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local docker + scriptorium-sandbox image"]
async fn exec_echoes_to_stdout_and_stderr() {
    let (addr, _tmp) = spawn_service().await;
    let mut client = client(addr).await;
    let resp = client
        .exec(ExecRequest {
            workspace_id: "echo".into(),
            tenant_id: "t".into(),
            command: "echo hello && echo world >&2".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.exit_code, 0);
    assert_eq!(resp.stdout.as_ref(), b"hello\n");
    assert_eq!(resp.stderr.as_ref(), b"world\n");
    assert!(!resp.timed_out);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local docker + scriptorium-sandbox image"]
async fn exec_propagates_non_zero_exit() {
    let (addr, _tmp) = spawn_service().await;
    let mut client = client(addr).await;
    let resp = client
        .exec(ExecRequest {
            workspace_id: "exit".into(),
            tenant_id: "t".into(),
            command: "exit 42".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.exit_code, 42);
    assert!(!resp.timed_out);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local docker + scriptorium-sandbox image"]
async fn exec_enforces_wall_clock_timeout() {
    let (addr, _tmp) = spawn_service().await;
    let mut client = client(addr).await;
    let resp = client
        .exec(ExecRequest {
            workspace_id: "timeout".into(),
            tenant_id: "t".into(),
            command: "sleep 20".into(),
            timeout_seconds: 2,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert!(resp.timed_out, "expected timeout");
    assert!(resp.duration_ms < 10_000, "should not take the full 20s");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local docker + scriptorium-sandbox image"]
async fn exec_persists_workspace_state_across_calls() {
    let (addr, _tmp) = spawn_service().await;
    let mut client = client(addr).await;

    let ws = "persistent";

    // First exec writes a file into $HOME.
    let _ = client
        .exec(ExecRequest {
            workspace_id: ws.into(),
            tenant_id: "t".into(),
            command: "echo 'persisted content' > ~/hello.txt".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    // Second exec reads it back — proving the home bind-mount survives.
    let resp = client
        .exec(ExecRequest {
            workspace_id: ws.into(),
            tenant_id: "t".into(),
            command: "cat ~/hello.txt".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.exit_code, 0);
    assert_eq!(resp.stdout.as_ref(), b"persisted content\n");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local docker + scriptorium-sandbox image"]
async fn exec_stream_emits_started_chunks_finished() {
    let (addr, _tmp) = spawn_service().await;
    let mut client = client(addr).await;
    let stream = client
        .exec_stream(ExecRequest {
            workspace_id: "stream".into(),
            tenant_id: "t".into(),
            command: "for i in a b c; do echo $i; sleep 0.05; done".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();

    let events: Vec<_> = stream.collect::<Vec<_>>().await;
    assert!(!events.is_empty());
    let events: Vec<_> = events.into_iter().map(Result::unwrap).collect();

    // The first event must be Started.
    assert!(matches!(
        events.first().and_then(|e| e.event.as_ref()),
        Some(exec_event::Event::Started(_))
    ));
    // The last event must be Finished with exit_code=0.
    let last = events.last().and_then(|e| e.event.as_ref()).unwrap();
    match last {
        exec_event::Event::Finished(f) => {
            assert_eq!(f.exit_code, 0);
            assert!(!f.timed_out);
        }
        other => panic!("expected Finished, got {other:?}"),
    }

    // Middle events must contain three stdout payloads matching "a\n", "b\n", "c\n".
    let mut concatenated = Vec::new();
    for ev in &events[1..events.len() - 1] {
        if let Some(exec_event::Event::Stdout(s)) = ev.event.as_ref() {
            concatenated.extend_from_slice(&s.data);
        }
    }
    assert_eq!(concatenated, b"a\nb\nc\n");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local docker + scriptorium-sandbox image"]
async fn list_files_reflects_workspace_contents() {
    let (addr, _tmp) = spawn_service().await;
    let mut client = client(addr).await;

    // Populate workspace via exec.
    let _ = client
        .exec(ExecRequest {
            workspace_id: "list".into(),
            tenant_id: "t".into(),
            command: "mkdir -p ~/dir && echo one > ~/a.txt && echo two > ~/dir/b.txt".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    let resp = client
        .list_files(ListFilesRequest {
            workspace_id: "list".into(),
            path: String::new(),
            recursive: true,
        })
        .await
        .unwrap()
        .into_inner();

    let paths: Vec<String> = resp
        .files
        .iter()
        .map(|f| {
            f.path
                .rsplit_once('/')
                .map_or_else(|| f.path.clone(), |(_, leaf)| leaf.to_string())
        })
        .collect();
    assert!(
        paths.iter().any(|p| p == "a.txt"),
        "a.txt missing: {paths:?}"
    );
    assert!(
        paths.iter().any(|p| p == "b.txt"),
        "b.txt missing: {paths:?}"
    );
    assert!(paths.iter().any(|p| p == "dir"), "dir missing: {paths:?}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local docker + scriptorium-sandbox image"]
async fn delete_workspace_removes_host_directory() {
    let (addr, tmp) = spawn_service().await;
    let mut client = client(addr).await;
    let ws = "deleteme";

    let _ = client
        .exec(ExecRequest {
            workspace_id: ws.into(),
            tenant_id: "t".into(),
            command: "touch ~/marker".into(),
            ..Default::default()
        })
        .await
        .unwrap();

    let host_dir = tmp.path().join(ws);
    assert!(host_dir.exists());

    let resp = client
        .delete_workspace(DeleteWorkspaceRequest {
            workspace_id: ws.into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(resp.existed);
    assert!(!host_dir.exists());

    // Second delete is a no-op.
    let resp = client
        .delete_workspace(DeleteWorkspaceRequest {
            workspace_id: ws.into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(!resp.existed);
}

/// End-to-end against real TOS credentials. Writes an artifact in the
/// sandbox via `Exec`, uploads via `UploadToOSS`, fetches the signed URL
/// back, and asserts the body matches. Requires:
///
///   SCRIPTORIUM_TEST_TOS_ACCESS_KEY
///   SCRIPTORIUM_TEST_TOS_SECRET_KEY
///   SCRIPTORIUM_TEST_TOS_BUCKET
///   SCRIPTORIUM_TEST_TOS_ENDPOINT   (optional, has cn-shanghai default)
///   SCRIPTORIUM_TEST_TOS_REGION     (optional, has cn-shanghai default)
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires docker + scriptorium-sandbox image + real TOS creds"]
async fn upload_to_oss_roundtrips_through_signed_url() {
    if std::env::var("SCRIPTORIUM_TEST_TOS_ACCESS_KEY").is_err()
        || std::env::var("SCRIPTORIUM_TEST_TOS_SECRET_KEY").is_err()
        || std::env::var("SCRIPTORIUM_TEST_TOS_BUCKET").is_err()
    {
        eprintln!(
            "skipping: set SCRIPTORIUM_TEST_TOS_ACCESS_KEY / _SECRET_KEY / _BUCKET to enable"
        );
        return;
    }

    let (addr, _tmp) = spawn_service().await;
    let mut client = client(addr).await;

    let ws = "oss-roundtrip";
    let expected = b"hello from scriptorium e2e\n";

    // 1. Produce an artifact inside the sandbox.
    let exec_resp = client
        .exec(ExecRequest {
            workspace_id: ws.into(),
            tenant_id: "e2e-tenant".into(),
            command:
                "printf 'hello from scriptorium e2e\\n' > ~/artifact.txt && ls -la ~/artifact.txt"
                    .to_string(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(exec_resp.exit_code, 0, "produce artifact failed");

    // 2. Deliver (scriptorium's job). Response contains only the permanent
    //    object_key + metadata — no TTL URL.
    let resp = client
        .upload_to_oss(scriptorium::pb::UploadRequest {
            workspace_id: ws.into(),
            tenant_id: "e2e-tenant".into(),
            source_path: "artifact.txt".into(),
            compress: false,
            label: "e2e-test".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.size_bytes, expected.len() as u64);
    assert!(resp.object_key.contains("e2e-tenant"));
    assert!(resp.object_key.contains(ws));
    assert_eq!(resp.content_type, "text/plain; charset=utf-8");
    assert!(!resp.sha256_hex.is_empty());
    assert_eq!(resp.basename, "artifact.txt");

    println!("object_key: {}", resp.object_key);
    println!("basename:   {}", resp.basename);
    println!("sha256:     {}", resp.sha256_hex);

    // 3. Simulate what a host attachment layer does at download time: re-sign
    //    with the configured OSS client and fetch. Proves the object landed
    //    correctly and is retrievable from a private-read bucket.
    let oss = OssClient::connect(&tos_cfg_for_tests()).expect("oss connect for signing");
    let signed_url = oss
        .signed_url(&resp.object_key, std::time::Duration::from_secs(120))
        .await
        .expect("signed url");
    let body = reqwest::Client::new()
        .get(&signed_url)
        .send()
        .await
        .expect("signed url fetch")
        .error_for_status()
        .expect("signed url returned non-2xx")
        .bytes()
        .await
        .expect("signed url body")
        .to_vec();
    assert_eq!(body.as_slice(), expected, "downloaded body does not match");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local docker + scriptorium-sandbox image"]
async fn list_tools_advertises_four_tools() {
    let (addr, _tmp) = spawn_service().await;
    let mut client = client(addr).await;
    let resp = client
        .list_tools(ListToolsRequest {})
        .await
        .unwrap()
        .into_inner();
    let names: Vec<String> = resp.tools.iter().map(|t| t.name.clone()).collect();
    assert_eq!(names.len(), 4, "expected 4 tools, got {names:?}");
    assert!(names.contains(&"execute_shell".to_string()));
    assert!(names.contains(&"deliver".to_string()));
    assert!(names.contains(&"copy_workspace_sandbox_to_execution_sandbox".to_string()));
    assert!(names.contains(&"copy_execution_sandbox_to_workspace_sandbox".to_string()));
    // Every schema must be valid JSON.
    for t in &resp.tools {
        serde_json::from_str::<serde_json::Value>(&t.parameters_schema)
            .unwrap_or_else(|e| panic!("tool {} schema invalid: {e}", t.name));
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local docker + scriptorium-sandbox image"]
async fn call_tool_execute_shell_routes_to_exec() {
    let (addr, _tmp) = spawn_service().await;
    let mut client = client(addr).await;
    let args = serde_json::json!({
        "command": "echo ok-from-tool",
    });
    let resp = client
        .call_tool(CallToolRequest {
            workspace_id: "tool-exec".into(),
            tenant_id: "t".into(),
            tool_name: "execute_shell".into(),
            arguments_json: args.to_string(),
            timeout_seconds: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert!(!resp.is_error, "got error: {}", resp.error_message);
    let parsed: serde_json::Value = serde_json::from_str(&resp.result_json).unwrap();
    assert_eq!(parsed["exit_code"], 0);
    assert_eq!(parsed["stdout"], "ok-from-tool\n");
    assert_eq!(parsed["timed_out"], false);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local docker + scriptorium-sandbox image"]
async fn call_tool_rejects_unknown_tool() {
    let (addr, _tmp) = spawn_service().await;
    let mut client = client(addr).await;
    let resp = client
        .call_tool(CallToolRequest {
            workspace_id: "tool-unknown".into(),
            tenant_id: "t".into(),
            tool_name: "does_not_exist".into(),
            arguments_json: "{}".into(),
            timeout_seconds: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert!(resp.is_error);
    assert!(resp.error_message.contains("unknown tool"));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local docker + scriptorium-sandbox image"]
async fn call_tool_fetch_is_no_longer_advertised_or_routable() {
    let (addr, _tmp) = spawn_service().await;
    let mut client = client(addr).await;
    let args = serde_json::json!({
        "url": "https://example.com/report.pdf",
        "target_path": "inputs/report.pdf",
    });
    let resp = client
        .call_tool(CallToolRequest {
            workspace_id: "tool-fetch".into(),
            tenant_id: "t".into(),
            tool_name: "fetch".into(),
            arguments_json: args.to_string(),
            timeout_seconds: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert!(resp.is_error);
    assert!(resp.error_message.contains("unknown tool"));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local docker + scriptorium-sandbox image"]
async fn call_tool_workspace_exchange_requires_host_bridge() {
    let (addr, _tmp) = spawn_service().await;
    let mut client = client(addr).await;
    let args = serde_json::json!({
        "source_path": "sandbox/input.txt",
        "target_path": "inputs/input.txt",
    });
    let resp = client
        .call_tool(CallToolRequest {
            workspace_id: "tool-bridge".into(),
            tenant_id: "t".into(),
            tool_name: "copy_workspace_sandbox_to_execution_sandbox".into(),
            arguments_json: args.to_string(),
            timeout_seconds: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert!(resp.is_error);
    assert!(resp.error_message.contains("host bridge"));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local docker + scriptorium-sandbox image"]
async fn fetch_rejects_private_address() {
    let (addr, _tmp) = spawn_service().await;
    let mut client = client(addr).await;
    let err = client
        .fetch_into_workspace(scriptorium::pb::FetchRequest {
            workspace_id: "ssrf".into(),
            tenant_id: "t".into(),
            url: "http://127.0.0.1:1/".into(),
            target_path: "evil".into(),
            headers: std::collections::HashMap::new(),
            timeout_seconds: 5,
        })
        .await
        .unwrap_err();
    assert!(
        err.message().contains("disallowed"),
        "unexpected error: {}",
        err.message()
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires local docker + scriptorium-sandbox image"]
async fn invalid_workspace_id_is_rejected() {
    let (addr, _tmp) = spawn_service().await;
    let mut client = client(addr).await;
    let err = client
        .exec(ExecRequest {
            workspace_id: "../escape".into(),
            tenant_id: "t".into(),
            command: "true".into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}
