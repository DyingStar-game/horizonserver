//! The few Kubernetes API calls the autoscaler needs (see autoscale.rs), made with
//! the pod's own service account: read the godotserver Deployment and its pods,
//! mark a pod to be deleted first, change the number of replicas.
//!
//! Plain HTTPS through reqwest rather than a full client crate: three endpoints.
//! Needs, in the namespace, `get` on deployments, `patch` on deployments/scale and
//! `list` + `patch` on pods (the horizon chart's Role).

use serde_json::{json, Value};
use std::time::Duration;

const SERVICE_ACCOUNT_DIR: &str = "/var/run/secrets/kubernetes.io/serviceaccount";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// The ReplicaSet deletes the pods with the lowest cost first when it scales down.
pub const DELETION_COST_ANNOTATION: &str = "controller.kubernetes.io/pod-deletion-cost";

#[derive(Clone)]
pub struct K8sClient {
    http: reqwest::Client,
    base: String,
    token: String,
    pub namespace: String,
}

/// The Deployment as the autoscaler sees it.
#[derive(Debug, Clone)]
pub struct DeploymentInfo {
    /// `spec.replicas`.
    pub replicas: u32,
    /// A rollout is going on (`status.updatedReplicas` behind, or an old ReplicaSet
    /// still has pods): no scale-down meanwhile.
    pub rolling: bool,
}

/// One pod of the Deployment.
#[derive(Debug, Clone)]
pub struct PodInfo {
    pub name: String,
    /// Empty until the pod is scheduled and has a network.
    pub ip: String,
    pub node: String,
    pub phase: String,
    /// Being deleted (`metadata.deletionTimestamp`).
    pub terminating: bool,
    /// `PodScheduled=False` with reason `Unschedulable`, since that many seconds.
    pub unschedulable_for: Option<Duration>,
    /// `controller.kubernetes.io/pod-deletion-cost`, when set.
    pub deletion_cost: Option<i32>,
}

impl K8sClient {
    /// The in-cluster client, from the service account mounted in the pod.
    pub fn in_cluster() -> Result<K8sClient, String> {
        let read = |file: &str| {
            std::fs::read_to_string(format!("{}/{}", SERVICE_ACCOUNT_DIR, file)).map_err(|e| format!("{}/{}: {}", SERVICE_ACCOUNT_DIR, file, e))
        };
        let token = read("token")?.trim().to_string();
        let namespace = read("namespace")?.trim().to_string();
        let ca = reqwest::Certificate::from_pem(read("ca.crt")?.as_bytes()).map_err(|e| format!("ca.crt: {}", e))?;
        let host = std::env::var("KUBERNETES_SERVICE_HOST").unwrap_or_else(|_| "kubernetes.default.svc".to_string());
        let port = std::env::var("KUBERNETES_SERVICE_PORT").unwrap_or_else(|_| "443".to_string());
        let host = if host.contains(':') { format!("[{}]", host) } else { host };
        let http = reqwest::Client::builder()
            .add_root_certificate(ca)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(K8sClient { http, base: format!("https://{}:{}", host, port), token, namespace })
    }

    async fn request(&self, method: reqwest::Method, path: &str, patch: Option<Value>) -> Result<Value, String> {
        let url = format!("{}{}", self.base, path);
        let mut request = self.http.request(method.clone(), &url).bearer_auth(&self.token);
        if let Some(body) = patch {
            request = request.header("Content-Type", "application/merge-patch+json").body(body.to_string());
        }
        let response = request.send().await.map_err(|e| format!("{} {}: {}", method, path, e))?;
        let status = response.status();
        let text = response.text().await.map_err(|e| format!("{} {}: {}", method, path, e))?;
        if !status.is_success() {
            return Err(format!("{} {}: {} {}", method, path, status, text.chars().take(300).collect::<String>()));
        }
        serde_json::from_str(&text).map_err(|e| format!("{} {}: {}", method, path, e))
    }

    pub async fn deployment(&self, name: &str) -> Result<DeploymentInfo, String> {
        let path = format!("/apis/apps/v1/namespaces/{}/deployments/{}", self.namespace, name);
        let d = self.request(reqwest::Method::GET, &path, None).await?;
        let int = |v: &Value| v.as_u64().unwrap_or(0) as u32;
        let replicas = int(&d["spec"]["replicas"]);
        let status = &d["status"];
        let rolling = int(&status["updatedReplicas"]) < replicas
            || int(&status["replicas"]) > int(&status["updatedReplicas"])
            || d["metadata"]["generation"].as_u64() != status["observedGeneration"].as_u64();
        Ok(DeploymentInfo { replicas, rolling })
    }

    pub async fn pods(&self, label_selector: &str) -> Result<Vec<PodInfo>, String> {
        let path = format!("/api/v1/namespaces/{}/pods?labelSelector={}", self.namespace, url_encode(label_selector));
        let list = self.request(reqwest::Method::GET, &path, None).await?;
        let now = chrono::Utc::now();
        let pods = list["items"]
            .as_array()
            .map(|items| items.iter().map(|p| parse_pod(p, now)).collect())
            .unwrap_or_default();
        Ok(pods)
    }

    /// The ReplicaSet will delete this pod before the others on the next scale-down.
    pub async fn set_deletion_cost(&self, pod: &str, cost: i32) -> Result<(), String> {
        let path = format!("/api/v1/namespaces/{}/pods/{}", self.namespace, pod);
        let patch = json!({ "metadata": { "annotations": { DELETION_COST_ANNOTATION: cost.to_string() } } });
        self.request(reqwest::Method::PATCH, &path, Some(patch)).await.map(|_| ())
    }

    pub async fn scale(&self, deployment: &str, replicas: u32) -> Result<(), String> {
        let path = format!("/apis/apps/v1/namespaces/{}/deployments/{}/scale", self.namespace, deployment);
        self.request(reqwest::Method::PATCH, &path, Some(json!({ "spec": { "replicas": replicas } }))).await.map(|_| ())
    }
}

fn parse_pod(p: &Value, now: chrono::DateTime<chrono::Utc>) -> PodInfo {
    let text = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let unschedulable_for = p["status"]["conditions"].as_array().and_then(|conditions| {
        conditions
            .iter()
            .find(|c| c["type"] == "PodScheduled" && c["status"] == "False" && c["reason"] == "Unschedulable")
            .map(|c| {
                c["lastTransitionTime"]
                    .as_str()
                    .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                    .and_then(|since| (now - since.with_timezone(&chrono::Utc)).to_std().ok())
                    .unwrap_or_default()
            })
    });
    PodInfo {
        name: text(&p["metadata"]["name"]),
        ip: text(&p["status"]["podIP"]),
        node: text(&p["spec"]["nodeName"]),
        phase: text(&p["status"]["phase"]),
        terminating: !p["metadata"]["deletionTimestamp"].is_null(),
        unschedulable_for,
        deletion_cost: p["metadata"]["annotations"][DELETION_COST_ANNOTATION].as_str().and_then(|c| c.parse().ok()),
    }
}

/// Enough of percent-encoding for a label selector (`a=b,c in (d)`).
fn url_encode(raw: &str) -> String {
    raw.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => (b as char).to_string(),
            _ => format!("%{:02X}", b),
        })
        .collect()
}

/// The IP of a pool address (`ws://10.244.1.23:8980` → `10.244.1.23`).
pub fn address_ip(address: &str) -> &str {
    let rest = address.split("://").nth(1).unwrap_or(address);
    let host = rest.split('/').next().unwrap_or(rest);
    match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(v6),
        None => host.rsplit_once(':').map_or(host, |(ip, _)| ip),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_ip_strips_scheme_and_port() {
        assert_eq!(address_ip("ws://10.244.1.23:8980"), "10.244.1.23");
        assert_eq!(address_ip("ws://[fd00::5]:8980"), "fd00::5");
        assert_eq!(address_ip("10.0.0.1"), "10.0.0.1");
    }

    #[test]
    fn selector_is_encoded() {
        assert_eq!(url_encode("app.kubernetes.io/name=godotserver"), "app.kubernetes.io/name%3Dgodotserver");
    }

    #[test]
    fn unschedulable_pod_is_parsed() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-26T12:05:00Z").unwrap().with_timezone(&chrono::Utc);
        let pod = json!({
            "metadata": { "name": "godotserver-abc" },
            "spec": {},
            "status": { "phase": "Pending", "conditions": [
                { "type": "PodScheduled", "status": "False", "reason": "Unschedulable", "lastTransitionTime": "2026-10-26T12:00:00Z" }
            ] }
        });
        let info = parse_pod(&pod, now);
        assert_eq!(info.unschedulable_for, Some(Duration::from_secs(300)));
        assert!(!info.terminating);
        assert!(info.ip.is_empty());
    }
}
