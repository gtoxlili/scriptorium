//! End-to-end integration tests for scriptorium.
//!
//! These tests spin up the gRPC service in-process against a local Docker
//! daemon (OrbStack on the author's Mac), make real exec calls, and assert
//! the resulting behaviour. They are gated behind `#[ignore]` because they
//! require:
//!   1. A reachable Docker daemon.
//!   2. The `scriptorium-sandbox:debian12-v1` image (built via
//!      `docker build -f docker/sandbox.Dockerfile -t scriptorium-sandbox:debian12-v1 .`).
//!
//! Run with `cargo test --test e2e -- --ignored --nocapture`.
#![allow(clippy::doc_markdown)]

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use scriptorium::{
    config::{DockerConfig, SandboxConfig, WorkspaceConfig},
    pb::{
        DeleteWorkspaceRequest, ExecRequest, GetFileRequest, HealthRequest, ListFilesRequest,
        PutFileHeader, PutFileRequest, exec_event, get_file_chunk, put_file_request,
        sandbox_client::SandboxClient, sandbox_server::SandboxServer,
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
            .unwrap_or_else(|_| "scriptorium-sandbox:debian12-v1".to_string()),
        default_timeout_seconds: 30,
        default_cpu_millis: 2000,
        default_memory_bytes: 2 * 1024 * 1024 * 1024,
        default_pids: 256,
        default_tmpfs_bytes: 256 * 1024 * 1024,
        agent_uid: 1000,
        agent_gid: 1000,
        workspace_mount: PathBuf::from("/home/agent"),
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
    let svc = SandboxService::new(runtime, workspaces);

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
    println!("docker_version = {}", resp.docker_version);
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
    assert_eq!(resp.stdout, b"hello\n");
    assert_eq!(resp.stderr, b"world\n");
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
    assert_eq!(resp.stdout, b"persisted content\n");
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
async fn put_then_get_file_roundtrips_contents() {
    let (addr, _tmp) = spawn_service().await;
    let mut client = client(addr).await;

    let payload: Vec<u8> = (0u8..=250).cycle().take(200_000).collect();
    let ws = "io";
    let path = "inputs/a.bin";

    // PutFile
    let header_msg = PutFileRequest {
        payload: Some(put_file_request::Payload::Header(PutFileHeader {
            workspace_id: ws.into(),
            path: path.into(),
            mode: 0o644,
        })),
    };
    let chunk_msgs: Vec<PutFileRequest> = payload
        .chunks(8192)
        .map(|c| PutFileRequest {
            payload: Some(put_file_request::Payload::Chunk(c.to_vec())),
        })
        .collect();
    let mut messages = Vec::with_capacity(chunk_msgs.len() + 1);
    messages.push(header_msg);
    messages.extend(chunk_msgs);
    let stream = tokio_stream::iter(messages);
    let resp = client.put_file(stream).await.unwrap().into_inner();
    assert_eq!(resp.bytes_written, payload.len() as u64);

    // GetFile
    let mut stream = client
        .get_file(GetFileRequest {
            workspace_id: ws.into(),
            path: path.into(),
        })
        .await
        .unwrap()
        .into_inner();

    let mut saw_header = false;
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        match chunk.payload {
            Some(get_file_chunk::Payload::Header(h)) => {
                saw_header = true;
                assert_eq!(h.size_bytes, payload.len() as u64);
                assert_eq!(h.mode & 0o777, 0o644);
            }
            Some(get_file_chunk::Payload::Chunk(data)) => buf.extend_from_slice(&data),
            None => {}
        }
    }
    assert!(saw_header, "header must arrive");
    assert_eq!(buf, payload);
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
