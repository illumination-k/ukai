use crate::config::KubernetesAgent;
use rstest::rstest;
use serde_json::{Value, json};

use super::*;

/// A Kubernetes agent with `extra` keys, and `workspace` keys after `git`.
fn agent_with(extra: &str, workspace: &str) -> KubernetesAgent {
    let src = format!(
        r#"
        [agents.k]
        image = "registry.example.com/agent:latest"
        command = "claude-agent-acp"
        args = ["--verbose"]
        home = "/home/agent"
        workspace = {{ git = "https://github.com/org/repo", github_token = "gh/token"{workspace} }}
        {extra}
        "#
    );
    match crate::config::parse(&src).map(|mut config| config.agents.remove("k")) {
        Ok(Some(agent)) => agent,
        other => panic!("expected agent `k`, got {other:?}"),
    }
}

fn agent(extra: &str) -> KubernetesAgent {
    agent_with(extra, "")
}

fn plan(extra: &str) -> Plan {
    Plan::new(agent(extra), "alice", "team", None)
}

fn json(plan: &Plan) -> Value {
    let runtime = Runtime {
        relay_image: "registry.example.com/ukai-runtime:1".into(),
        gateway_url: Some("http://ukai-gateway".into()),
    };
    serde_json::to_value(job(plan, &runtime, "s3cret")).unwrap_or_default()
}

fn container<'a>(job: &'a Value, path: &str, name: &str) -> &'a Value {
    job.pointer(path)
        .and_then(Value::as_array)
        .and_then(|containers| containers.iter().find(|c| c["name"] == name))
        .unwrap_or(&Value::Null)
}

fn env_names(container: &Value) -> Vec<&str> {
    container["env"]
        .as_array()
        .map(|env| env.iter().filter_map(|e| e["name"].as_str()).collect())
        .unwrap_or_default()
}

#[test]
fn workspace_id_is_stable_and_distinguishes_inputs() {
    let a = plan("");
    assert_eq!(a.workspace_id, plan("").workspace_id);
    assert_eq!(a.workspace_id.len(), 12);
    assert_eq!(a.job_name(), format!("ukai-{}", a.workspace_id));
    assert_ne!(
        a.workspace_id,
        Plan::new(agent(""), "bob", "team", None).workspace_id
    );
    assert_ne!(
        a.workspace_id,
        Plan::new(agent(""), "alice", "other", None).workspace_id
    );
    // A pane of its own gets its own workspace, the same one each time.
    let pane = |key| Plan::new(agent(""), "alice", "team", Some(key)).workspace_id;
    assert_ne!(pane("k1"), a.workspace_id);
    assert_ne!(pane("k1"), pane("k2"));
    assert_eq!(pane("k1"), pane("k1"));
    // Image and resources are not part of the identity: changing them keeps
    // the workspace and its snapshots.
    assert_eq!(
        a.workspace_id,
        plan("resources = { cpu = \"1\" }").workspace_id
    );
}

#[test]
fn the_github_token_reaches_the_sidecar_and_the_agent() {
    let job = json(&plan("secret_env = { API_KEY = \"llm/key\" }"));
    let sidecar = container(&job, "/spec/template/spec/initContainers", "workspace");
    let agent = container(&job, "/spec/template/spec/containers", "agent");
    let token = json!({
        "name": "GH_TOKEN",
        "valueFrom": { "secretKeyRef": { "name": "gh", "key": "token" } }
    });
    for container in [sidecar, agent] {
        let found = container["env"]
            .as_array()
            .and_then(|env| env.iter().find(|e| e["name"] == "GH_TOKEN"));
        assert_eq!(found, Some(&token));
    }
    // The agent pushes through gh; its own secrets stay its own.
    assert!(env_names(agent).contains(&"GIT_CONFIG_VALUE_0"));
    assert!(env_names(agent).contains(&"API_KEY"));
    assert!(!env_names(sidecar).contains(&"API_KEY"));
}

#[test]
fn author_sets_the_agents_commit_identity() {
    let job = json(&Plan::new(
        agent_with(
            "",
            r#", author = { name = "ukai", email = "ukai@example.com" }"#,
        ),
        "alice",
        "team",
        None,
    ));
    let agent = container(&job, "/spec/template/spec/containers", "agent");
    for name in ["GIT_AUTHOR_NAME", "GIT_COMMITTER_EMAIL"] {
        assert!(env_names(agent).contains(&name), "{name}");
    }
}

#[test]
fn agent_never_mounts_the_pristine_clone() {
    let job = json(&plan(""));
    let agent = container(&job, "/spec/template/spec/containers", "agent");
    let paths: Vec<&str> = agent["volumeMounts"]
        .as_array()
        .map(|m| m.iter().filter_map(|m| m["mountPath"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(paths, ["/workspace", "/home/agent", "/ukai/bin"]);
}

#[test]
fn sidecar_is_native_and_gates_the_agent() {
    let job = json(&plan(""));
    let sidecar = container(&job, "/spec/template/spec/initContainers", "workspace");
    assert_eq!(sidecar["restartPolicy"], "Always");
    assert_eq!(
        sidecar["startupProbe"]["exec"]["command"],
        json!(["test", "-e", "/workspace/.ukai/ready"])
    );
}

#[test]
fn mounts_share_one_volume_per_claim_and_expand_home() {
    let job = json(&plan(
        r#"mounts = [
            { claim = "home", path = "~/.claude", sub_path = "claude-config" },
            { claim = "home", path = "/home/agent/.codex", sub_path = "codex-config" },
            { claim = "cache", path = "/cache", read_only = true },
        ]"#,
    ));
    let volumes = job.pointer("/spec/template/spec/volumes");
    let claims: Vec<(&str, &str)> = volumes
        .and_then(Value::as_array)
        .map(|v| {
            v.iter()
                .filter_map(|v| {
                    Some((
                        v["name"].as_str()?,
                        v["persistentVolumeClaim"]["claimName"].as_str()?,
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(claims, [("claim-0", "cache"), ("claim-1", "home")]);

    let agent = container(&job, "/spec/template/spec/containers", "agent");
    // After the workspace, home, and relay mounts.
    let mounts = &agent["volumeMounts"];
    assert_eq!(
        mounts[3],
        json!({ "name": "claim-1", "mountPath": "/home/agent/.claude", "subPath": "claude-config" })
    );
    assert_eq!(mounts[4]["mountPath"], "/home/agent/.codex");
    assert_eq!(
        mounts[5],
        json!({ "name": "claim-0", "mountPath": "/cache", "readOnly": true })
    );
}

#[test]
fn tokens_project_one_volume_per_directory() {
    let job = json(&plan(
        r#"tokens = [
            { audience = "https://infer.example/wif/a", path = "/var/run/secrets/inference/subject-token" },
            { audience = "https://other.example", path = "/var/run/secrets/inference/other-token" },
            { audience = "https://vault.example", path = "~/.vault/token" },
        ]"#,
    ));
    let volumes = job
        .pointer("/spec/template/spec/volumes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let projected: Vec<&Value> = volumes
        .iter()
        .filter(|v| v["projected"].is_object())
        .collect();
    assert_eq!(
        projected,
        [
            &json!({ "name": "token-0", "projected": { "sources": [
                { "serviceAccountToken": { "audience": "https://infer.example/wif/a", "path": "subject-token" } },
                { "serviceAccountToken": { "audience": "https://other.example", "path": "other-token" } },
            ] } }),
            &json!({ "name": "token-1", "projected": { "sources": [
                { "serviceAccountToken": { "audience": "https://vault.example", "path": "token" } },
            ] } }),
        ]
    );

    // Whole directories, never a `subPath`, so refreshed tokens show up.
    let agent = container(&job, "/spec/template/spec/containers", "agent");
    let mounts = &agent["volumeMounts"];
    assert_eq!(
        mounts[3],
        json!({ "name": "token-0", "mountPath": "/var/run/secrets/inference", "readOnly": true })
    );
    assert_eq!(
        mounts[4],
        json!({ "name": "token-1", "mountPath": "/home/agent/.vault", "readOnly": true })
    );
    let sidecar = container(&job, "/spec/template/spec/initContainers", "workspace");
    assert!(
        !sidecar.to_string().contains("token-"),
        "the sidecar never sees the tokens"
    );
}

#[rstest]
#[case::defaults("", "", "1800", "300", "refs/ukai/wip/")]
#[case::configured(
    "idle_timeout = \"45s\"",
    ", snapshot_interval = \"1m\", wip_ref = \"refs/heads/wip\"",
    "45",
    "60",
    "refs/heads/wip"
)]
fn durations_and_wip_ref_reach_the_scripts(
    #[case] extra: &str,
    #[case] workspace: &str,
    #[case] idle: &str,
    #[case] interval: &str,
    #[case] wip_prefix: &str,
) {
    let job = json(&Plan::new(
        agent_with(extra, workspace),
        "alice",
        "team",
        None,
    ));
    let value = |path: &str, name: &str, var: &str| {
        container(&job, path, name)["env"]
            .as_array()
            .and_then(|env| env.iter().find(|e| e["name"] == var))
            .and_then(|e| e["value"].as_str())
            .map(str::to_owned)
    };
    let containers = "/spec/template/spec/containers";
    let init = "/spec/template/spec/initContainers";
    assert_eq!(
        value(containers, "agent", "UKAI_IDLE_TIMEOUT").as_deref(),
        Some(idle)
    );
    assert_eq!(
        value(init, "workspace", "UKAI_SNAPSHOT_INTERVAL").as_deref(),
        Some(interval)
    );
    assert!(value(init, "workspace", "UKAI_WIP_REF").is_some_and(|r| r.starts_with(wip_prefix)));
}

#[test]
fn pod_meets_the_restricted_profile() {
    let job = json(&plan(""));
    let pod = &job["spec"]["template"]["spec"];
    assert_eq!(pod["securityContext"]["runAsNonRoot"], true);
    assert_eq!(
        pod["securityContext"]["seccompProfile"]["type"],
        "RuntimeDefault"
    );
    for path in [
        "/spec/template/spec/containers",
        "/spec/template/spec/initContainers",
    ] {
        for container in job
            .pointer(path)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            assert_eq!(
                container["securityContext"]["allowPrivilegeEscalation"],
                false
            );
            assert_eq!(
                container["securityContext"]["capabilities"]["drop"],
                json!(["ALL"])
            );
        }
    }
}

#[test]
fn every_mount_names_a_volume() {
    let job = json(&plan(
        r#"mounts = [{ claim = "home", path = "~/.claude", sub_path = "claude" }]"#,
    ));
    let spec = &job["spec"]["template"]["spec"];
    let volumes: Vec<&str> = spec["volumes"]
        .as_array()
        .map(|v| v.iter().filter_map(|v| v["name"].as_str()).collect())
        .unwrap_or_default();
    for containers in ["containers", "initContainers"] {
        for container in spec[containers].as_array().into_iter().flatten() {
            for mount in container["volumeMounts"].as_array().into_iter().flatten() {
                let name = mount["name"].as_str().unwrap_or_default();
                assert!(
                    volumes.contains(&name),
                    "{} mounts undeclared volume `{name}`",
                    container["name"]
                );
            }
        }
    }
}

#[test]
fn the_relay_serves_the_agent_from_the_checkout() {
    let job = json(&plan(""));
    let install = container(&job, "/spec/template/spec/initContainers", "relay-install");
    assert_eq!(install["image"], "registry.example.com/ukai-runtime:1");
    let agent = container(&job, "/spec/template/spec/containers", "agent");
    assert_eq!(
        agent["command"],
        json!([
            "/ukai/bin/ukai-relay",
            "--listen",
            "0.0.0.0:7070",
            "--",
            "claude-agent-acp",
            "--verbose"
        ])
    );
    assert_eq!(agent["workingDir"], REPO_DIR);
    assert_eq!(
        agent["readinessProbe"]["httpGet"],
        json!({ "path": "/health", "port": 7070 })
    );
    let token = agent["env"]
        .as_array()
        .and_then(|env| env.iter().find(|e| e["name"] == "UKAI_RELAY_TOKEN"));
    assert_eq!(token.map(|e| &e["value"]), Some(&json!("s3cret")));
    // The relay installer runs before the sidecar, which gates the agent.
    let order: Vec<&str> = job
        .pointer("/spec/template/spec/initContainers")
        .and_then(Value::as_array)
        .map(|c| c.iter().filter_map(|c| c["name"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(order, ["relay-install", "workspace"]);
}

#[test]
fn the_pod_can_reach_the_knowledge_of_its_repository() {
    let job = json(&plan(""));
    let agent = container(&job, "/spec/template/spec/containers", "agent");
    let url = agent["env"]
        .as_array()
        .and_then(|env| env.iter().find(|e| e["name"] == GATEWAY_URL_ENV));
    assert_eq!(
        url.map(|e| &e["value"]),
        Some(&json!("http://ukai-gateway"))
    );
    // The gateway tells a pod's knowledge scope from the pod alone.
    assert_eq!(
        job.pointer("/spec/template/metadata/annotations/ukai.dev~1knowledge"),
        Some(&json!("github.com/org/repo"))
    );
    let shared = json(&plan("knowledge = \"plekti\""));
    assert_eq!(
        shared.pointer("/spec/template/metadata/annotations/ukai.dev~1knowledge"),
        Some(&json!("plekti"))
    );
}

#[test]
fn the_agents_labels_join_the_gateways() {
    let job = json(&plan(r#"labels = { "example.com/inject-config" = "" }"#));
    // On the pod, which is what admission webhooks see.
    let labels = &job["spec"]["template"]["metadata"]["labels"];
    assert_eq!(labels["example.com/inject-config"], "");
    assert_eq!(labels[MANAGED_BY], "ukai");
    assert!(labels[WORKSPACE_LABEL].is_string());
    assert_eq!(job["metadata"]["labels"], *labels);
}

#[test]
fn the_agents_env_can_refer_to_its_namespace() {
    let job = json(&plan(
        r#"env = { CACHE = "http://cache/$(POD_NAMESPACE)" }"#,
    ));
    let agent = container(&job, "/spec/template/spec/containers", "agent");
    assert_eq!(
        agent["env"][0],
        json!({
            "name": "POD_NAMESPACE",
            "valueFrom": { "fieldRef": { "fieldPath": "metadata.namespace" } }
        })
    );
    assert!(env_names(agent).contains(&"CACHE"));
}

#[rstest]
#[case::limit_is_the_request("resources = { cpu = \"4\", memory = \"8Gi\" }", "8Gi")]
#[case::limit_of_its_own(
    "resources = { cpu = \"4\", memory = \"8Gi\", memory_limit = \"16Gi\" }",
    "16Gi"
)]
fn the_agent_gets_its_resources(#[case] extra: &str, #[case] limit: &str) {
    let job = json(&plan(extra));
    let agent = container(&job, "/spec/template/spec/containers", "agent");
    assert_eq!(
        agent["resources"],
        json!({
            "requests": { "cpu": "4", "memory": "8Gi" },
            "limits": { "memory": limit }
        })
    );
}
