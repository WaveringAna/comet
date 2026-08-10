//! Nova Engine direct-peer host.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use futures::stream::BoxStream;
use futures::{StreamExt, stream};
use nova_network::cidr::CidrRange;
use nova_network::discovery::{NovaProbe, Probe, ScanOptions, candidate_addresses, scan};
use nova_network::identity::DeviceIdentityRecord;
use nova_network::trust::{PairingState, Role, Trust};
use nova_rpc::{RpcClient, RpcError};
use tokio::sync::{Mutex, watch};

#[derive(Clone)]
pub struct NovaHost {
    inner: Arc<Inner>,
}

struct Inner {
    identity: DeviceIdentityRecord,
    trust: Arc<Trust>,
    pairing: Arc<PairingState>,
    clients: Mutex<HashMap<String, Arc<RpcClient>>>,
    endpoint: OnceLock<nova_network::transport::NovaEndpoint>,
    listener_port: u16,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerView {
    pub device_id: String,
    pub name: String,
    pub platform: String,
    pub endpoint: String,
    pub role: Role,
    pub revoked: bool,
    pub paired_at: chrono::DateTime<chrono::Utc>,
}

impl NovaHost {
    pub fn load(
        data_dir: &Path,
        platform: &str,
        device_id: &str,
        listener_port: u16,
    ) -> std::io::Result<Self> {
        let dir = data_dir.join("nova");
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            inner: Arc::new(Inner {
                identity: DeviceIdentityRecord::load_or_create_for_device(
                    &dir.join("identity.json"),
                    platform,
                    device_id,
                )?,
                trust: Arc::new(Trust::load(dir.join("trust.json"))),
                pairing: Arc::new(PairingState::default()),
                clients: Mutex::new(HashMap::new()),
                endpoint: OnceLock::new(),
                listener_port,
            }),
        })
    }

    pub fn device_id(&self) -> &str {
        &self.inner.identity.device_id
    }

    pub fn device_name(&self) -> &str {
        &self.inner.identity.name
    }

    pub fn identity(&self) -> DeviceIdentityRecord {
        self.inner.identity.clone()
    }

    pub fn trust(&self) -> Arc<Trust> {
        self.inner.trust.clone()
    }

    pub fn pairing(&self) -> Arc<PairingState> {
        self.inner.pairing.clone()
    }

    pub fn listener_port(&self) -> u16 {
        self.inner.listener_port
    }

    pub async fn bind_endpoint(
        &self,
        overlay: bool,
    ) -> Result<nova_network::transport::NovaEndpoint, RpcError> {
        if let Some(endpoint) = self.inner.endpoint.get() {
            return Ok(endpoint.clone());
        }
        let endpoint = nova_network::transport::bind_endpoint(
            &self.inner.identity,
            self.inner.listener_port,
            overlay,
        )
        .await?;
        let _ = self.inner.endpoint.set(endpoint.clone());
        Ok(self.inner.endpoint.get().cloned().unwrap_or(endpoint))
    }

    pub fn ticket(&self) -> Result<String, RpcError> {
        let endpoint = self
            .inner
            .endpoint
            .get()
            .ok_or_else(|| RpcError::Failed("iroh endpoint is not ready".into()))?;
        nova_network::transport::endpoint_ticket(endpoint)
    }

    fn endpoint(&self) -> Result<&nova_network::transport::NovaEndpoint, RpcError> {
        self.inner
            .endpoint
            .get()
            .ok_or_else(|| RpcError::Failed("iroh endpoint is not ready".into()))
    }

    pub fn begin_pairing(&self) -> serde_json::Value {
        let pairing = self.inner.pairing.begin();
        serde_json::json!({
            "code": pairing.code,
            "expiresAt": pairing.created_at + chrono::Duration::seconds(300),
        })
    }

    pub fn cancel_pairing(&self) -> serde_json::Value {
        self.inner.pairing.cancel();
        serde_json::json!({"ok": true})
    }

    pub async fn pair_peer(
        &self,
        endpoint: &str,
        code: &str,
    ) -> Result<serde_json::Value, RpcError> {
        let peer = nova_network::transport::pair_nova(
            endpoint,
            self.endpoint()?,
            &self.inner.identity,
            code,
        )
        .await?;
        let device_id = peer.device_id.clone();
        self.inner
            .trust
            .pair(peer)
            .map_err(|e| RpcError::Failed(e.to_string()))?;
        self.inner.clients.lock().await.remove(&device_id);
        Ok(serde_json::json!({"ok": true, "deviceId": device_id}))
    }

    pub fn update_peer(
        &self,
        device_id: &str,
        name: String,
        endpoint: &str,
        role: Role,
    ) -> Result<serde_json::Value, RpcError> {
        let endpoint = normalize_endpoint(endpoint)?;
        let changed = self
            .inner
            .trust
            .update(device_id, name, endpoint, role)
            .map_err(|e| RpcError::Failed(e.to_string()))?;
        changed
            .then_some(serde_json::json!({"ok": true}))
            .ok_or_else(|| RpcError::BadParams("unknown peer".into()))
    }

    pub fn revoke_peer(&self, device_id: &str) -> Result<serde_json::Value, RpcError> {
        let changed = self
            .inner
            .trust
            .revoke(device_id)
            .map_err(|e| RpcError::Failed(e.to_string()))?;
        changed
            .then_some(serde_json::json!({"ok": true}))
            .ok_or_else(|| RpcError::BadParams("unknown peer".into()))
    }

    pub fn forget_peer(&self, device_id: &str) -> Result<serde_json::Value, RpcError> {
        let changed = self
            .inner
            .trust
            .forget(device_id)
            .map_err(|e| RpcError::Failed(e.to_string()))?;
        changed
            .then_some(serde_json::json!({"ok": true}))
            .ok_or_else(|| RpcError::BadParams("unknown peer".into()))
    }

    pub fn list_peers(&self) -> Vec<PeerView> {
        self.inner
            .trust
            .snapshot()
            .all_peers()
            .into_iter()
            .map(|peer| PeerView {
                device_id: peer.device_id.clone(),
                name: peer.name.clone(),
                platform: peer.platform.clone(),
                endpoint: peer.endpoint.clone(),
                role: peer.role,
                revoked: peer.revoked,
                paired_at: peer.paired_at,
            })
            .collect()
    }

    pub fn live_peer_ids(&self) -> Vec<String> {
        self.inner
            .trust
            .snapshot()
            .live_peers()
            .into_iter()
            .map(|peer| peer.device_id.clone())
            .collect()
    }

    pub fn watch_trust(&self) -> watch::Receiver<u64> {
        self.inner.trust.watch_changes()
    }

    pub async fn dial(&self, device_id: &str) -> Result<Arc<RpcClient>, RpcError> {
        if let Some(client) = self.inner.clients.lock().await.get(device_id).cloned() {
            return Ok(client);
        }
        let peer = self
            .inner
            .trust
            .peer(device_id)
            .filter(|peer| !peer.revoked)
            .ok_or_else(|| RpcError::Failed(format!("nova device {device_id} is not paired")))?;
        let client = Arc::new(
            nova_network::transport::connect_nova(
                &peer.endpoint,
                self.endpoint()?,
                &self.inner.identity,
                &peer,
            )
            .await?,
        );
        self.inner
            .clients
            .lock()
            .await
            .insert(device_id.to_string(), client.clone());
        Ok(client)
    }

    pub async fn invalidate(&self, device_id: &str) {
        self.inner.clients.lock().await.remove(device_id);
    }

    pub fn scan(
        &self,
        ranges: Vec<String>,
        port: u16,
        allow_public: bool,
    ) -> Result<BoxStream<'static, serde_json::Value>, RpcError> {
        let parsed = ranges
            .into_iter()
            .filter(|range| !range.trim().is_empty())
            .map(|range| {
                CidrRange::parse(range.trim(), allow_public)
                    .map_err(|e| RpcError::BadParams(format!("range '{range}': {e}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if parsed.is_empty() {
            return Err(RpcError::BadParams("no scan ranges provided".into()));
        }

        let trust = self.inner.trust.clone();
        let options = ScanOptions::default();
        candidate_addresses(&parsed, &options)
            .map_err(|error| RpcError::BadParams(error.to_string()))?;
        let probe: Arc<dyn Probe> = Arc::new(NovaProbe {
            port,
            timeout: options.per_host_timeout,
        });
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_guard = cancel.clone().drop_guard();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let callback_tx = tx.clone();
            let result = scan(
                &parsed,
                probe,
                options,
                cancel,
                move |peer| {
                    if let Ok(value) = serde_json::to_value(peer) {
                        let _ = callback_tx.send(value);
                    }
                },
                move |device_id| trust.is_trusted(device_id),
            )
            .await;
            let done = match result {
                Ok(_) => serde_json::json!({"done": true}),
                Err(error) => serde_json::json!({"done": true, "error": error.to_string()}),
            };
            let _ = tx.send(done);
        });
        Ok(
            stream::unfold((rx, cancel_guard), |(mut rx, cancel_guard)| async move {
                rx.recv().await.map(|value| (value, (rx, cancel_guard)))
            })
            .boxed(),
        )
    }
}

pub fn normalize_endpoint(input: &str) -> Result<String, RpcError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(RpcError::BadParams("iroh ticket is empty".into()));
    }
    nova_network::transport::decode_ticket(input)?;
    Ok(input.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_old_socket_addresses() {
        assert!(normalize_endpoint("10.0.0.8:27655").is_err());
    }
}
