//! Encrypted Nova Engine transport over iroh.
//!
//! Nova reuses its existing Ed25519 device seed as the iroh endpoint key. Iroh therefore
//! authenticates and encrypts every QUIC connection before Nova's own pairing/trust and
//! method-level authorization run. A small TCP/WebSocket listener remains for explicit LAN
//! discovery only: it emits signed public metadata and closes without accepting credentials
//! or RPC frames.

use std::net::Ipv4Addr;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL;
use futures::SinkExt as _;
use iroh::endpoint::{Connection, RecvStream, SendStream, presets};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMap, RelayMode, SecretKey};
use nova_rpc::{RpcError, RpcReply, RpcService, serve_connection};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::authz::{CallerKind, Decision, authorize};
use crate::identity::{DeviceIdentityRecord, decode_hex_vec, verify_signature};
use crate::trust::{PairingState, Role, Trust, TrustedPeer};

pub type NovaEndpoint = Endpoint;

const ALPN: &[u8] = b"nova-engine/rpc/1";
const TICKET_PREFIX: &str = "nova-iroh:";
const CHALLENGE_TAG: &str = "nova-engine-challenge-v3";
const AUTH_TAG: &str = "nova-engine-auth-v3";
const MAX_HANDSHAKE_FRAME_BYTES: usize = 64 * 1024;
const MAX_PENDING_HANDSHAKES: usize = 64;
// Peer-sync exchanges can contain several base64-encoded 32 MiB Loro updates.
// The sender is already a paired device by the time this limit applies.
const MAX_RPC_FRAME_BYTES: usize = 192 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PeerIdentity {
    pub device_id: String,
    pub name: String,
    pub platform: String,
    pub public_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Challenge {
    pub nova: String,
    #[serde(flatten)]
    pub identity: PeerIdentity,
    pub ticket: String,
    pub nonce: String,
    pub signature: String,
}

impl Challenge {
    pub fn verify(&self) -> bool {
        self.nova == "challenge"
            && identity_is_valid(&self.identity)
            && ticket_matches_public_key(&self.ticket, &self.identity.public_key)
            && verify_signature(
                &self.identity.public_key,
                &challenge_message(&self.identity.device_id, &self.ticket, &self.nonce),
                &self.signature,
            )
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Hello {
    nova: String,
    device_id: String,
    public_key: String,
    signature: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PairRequest {
    nova: String,
    #[serde(flatten)]
    identity: PeerIdentity,
    ticket: String,
    code: String,
    signature: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Welcome {
    nova: String,
    device_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Paired {
    nova: String,
    #[serde(flatten)]
    identity: PeerIdentity,
    ticket: String,
}

#[derive(Debug, Serialize)]
struct Reject<'a> {
    nova: &'a str,
    reason: &'a str,
}

/// Authorization-wrapping service. The iroh listener never grants local trust;
/// localhost UI traffic uses the separate IPC listener.
pub struct AuthorizedService {
    inner: Arc<dyn RpcService>,
    trust: Arc<Trust>,
    device_id: String,
    public_key: String,
}

impl AuthorizedService {
    pub fn new(
        inner: Arc<dyn RpcService>,
        trust: Arc<Trust>,
        device_id: String,
        public_key: String,
    ) -> Self {
        Self {
            inner,
            trust,
            device_id,
            public_key,
        }
    }
}

#[async_trait::async_trait]
impl RpcService for AuthorizedService {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        // Re-read trust for every call rather than freezing the role at handshake
        // time. Revocation and role changes therefore affect already-open streams.
        let role = self
            .trust
            .peer(&self.device_id)
            .filter(|peer| !peer.revoked && peer.public_key == self.public_key)
            .map(|peer| peer.role)
            .ok_or_else(|| RpcError::Failed("device trust changed or was revoked".into()))?;
        match authorize(CallerKind::Remote { role }, method) {
            Decision::Allow => self.inner.handle(method, params).await,
            Decision::Deny(reason) => Err(RpcError::Failed(format!(
                "not permitted for this peer: {reason}"
            ))),
        }
    }
}

pub fn peer_identity(record: &DeviceIdentityRecord) -> Result<PeerIdentity, RpcError> {
    let public_key = record
        .public_key()
        .ok_or_else(|| RpcError::Failed("device identity secret is invalid".into()))?;
    let identity = PeerIdentity {
        device_id: record.device_id.clone(),
        name: record.name.clone(),
        platform: record.platform.clone(),
        public_key,
    };
    identity_is_valid(&identity)
        .then_some(identity)
        .ok_or_else(|| RpcError::Failed("device identity does not match public key".into()))
}

/// Bind Nova's iroh endpoint. Production uses iroh's relay/address-lookup overlay;
/// tests can disable it and use the ticket's direct loopback address only.
pub async fn bind_endpoint(
    identity: &DeviceIdentityRecord,
    port: u16,
    overlay: bool,
) -> Result<NovaEndpoint, RpcError> {
    let secret = identity
        .secret()
        .ok_or_else(|| RpcError::Failed("device identity secret is invalid".into()))?;
    let custom_relay = overlay
        .then(|| std::env::var("NOVA_IROH_RELAY_URL").ok())
        .flatten()
        .filter(|value| !value.trim().is_empty());
    let builder = if let Some(relay_url) = custom_relay.as_deref() {
        let relay_map = RelayMap::try_from_iter([relay_url])
            .map_err(|error| RpcError::BadParams(format!("invalid iroh relay url: {error}")))?;
        Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Custom(relay_map))
    } else if overlay {
        Endpoint::builder(presets::N0)
    } else {
        Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Disabled)
    };
    builder
        .secret_key(SecretKey::from_bytes(&secret.0))
        .alpns(vec![ALPN.to_vec()])
        .bind_addr((Ipv4Addr::UNSPECIFIED, port))
        .map_err(|error| RpcError::Transport(error.to_string()))?
        .bind()
        .await
        .map_err(|error| RpcError::Transport(error.to_string()))
}

pub fn encode_ticket(addr: &EndpointAddr) -> Result<String, RpcError> {
    let bytes = serde_json::to_vec(addr).map_err(|error| RpcError::Transport(error.to_string()))?;
    Ok(format!("{TICKET_PREFIX}{}", BASE64_URL.encode(bytes)))
}

pub fn decode_ticket(ticket: &str) -> Result<EndpointAddr, RpcError> {
    let value = ticket.trim();
    if let Some(encoded) = value.strip_prefix(TICKET_PREFIX) {
        let bytes = BASE64_URL
            .decode(encoded)
            .map_err(|_| RpcError::BadParams("invalid iroh ticket".into()))?;
        return serde_json::from_slice(&bytes)
            .map_err(|_| RpcError::BadParams("invalid iroh ticket".into()));
    }
    let id = value
        .parse::<EndpointId>()
        .map_err(|_| RpcError::BadParams("expected a nova-iroh ticket or endpoint id".into()))?;
    Ok(EndpointAddr::from(id))
}

pub fn endpoint_ticket(endpoint: &NovaEndpoint) -> Result<String, RpcError> {
    encode_ticket(&endpoint.addr())
}

pub fn challenge_from_text(text: &str) -> Option<Challenge> {
    serde_json::from_str::<Challenge>(text)
        .ok()
        .filter(Challenge::verify)
}

/// Public, signed LAN discovery only. This listener never reads a pairing code or
/// serves RPC; communication between paired engines always uses encrypted iroh QUIC.
pub async fn serve_discovery_listener(
    listener: TcpListener,
    identity: DeviceIdentityRecord,
    endpoint: NovaEndpoint,
) {
    let handshakes = Arc::new(Semaphore::new(MAX_PENDING_HANDSHAKES));
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let Ok(permit) = handshakes.clone().acquire_owned().await else {
                    return;
                };
                let identity = identity.clone();
                let endpoint = endpoint.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(error) = serve_discovery_socket(stream, identity, endpoint).await {
                        tracing::debug!(%peer, %error, "nova discovery probe ended");
                    }
                });
            }
            Err(error) => {
                tracing::warn!(%error, "nova discovery listener accept failed");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

async fn serve_discovery_socket(
    stream: TcpStream,
    identity: DeviceIdentityRecord,
    endpoint: NovaEndpoint,
) -> Result<(), RpcError> {
    let mut ws = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio_tungstenite::accept_async(stream),
    )
    .await
    .map_err(|_| RpcError::Transport("nova discovery handshake timed out".into()))?
    .map_err(|error| RpcError::Transport(error.to_string()))?;
    let challenge = make_challenge(&identity, &endpoint)?;
    ws.send(WsMessage::Text(
        serde_json::to_string(&challenge).map_err(|e| RpcError::Transport(e.to_string()))?,
    ))
    .await
    .map_err(|error| RpcError::Transport(error.to_string()))?;
    let _ = ws.send(WsMessage::Close(None)).await;
    Ok(())
}

/// Accept encrypted, mutually authenticated iroh connections until the endpoint closes.
pub async fn serve_iroh_endpoint(
    endpoint: NovaEndpoint,
    inner: Arc<dyn RpcService>,
    trust: Arc<Trust>,
    identity: DeviceIdentityRecord,
    pairing: Arc<PairingState>,
) {
    let handshakes = Arc::new(Semaphore::new(MAX_PENDING_HANDSHAKES));
    while let Some(incoming) = endpoint.accept().await {
        let Ok(permit) = handshakes.clone().acquire_owned().await else {
            return;
        };
        let endpoint = endpoint.clone();
        let inner = inner.clone();
        let trust = trust.clone();
        let identity = identity.clone();
        let pairing = pairing.clone();
        tokio::spawn(async move {
            let result = async {
                let connection = incoming
                    .await
                    .map_err(|error| RpcError::Transport(error.to_string()))?;
                serve_iroh_connection(
                    connection, endpoint, inner, trust, identity, pairing, permit,
                )
                .await
            }
            .await;
            if let Err(error) = result {
                tracing::debug!(%error, "nova iroh connection ended");
            }
        });
    }
}

async fn serve_iroh_connection(
    connection: Connection,
    endpoint: NovaEndpoint,
    inner: Arc<dyn RpcService>,
    trust: Arc<Trust>,
    local: DeviceIdentityRecord,
    pairing: Arc<PairingState>,
    handshake_permit: OwnedSemaphorePermit,
) -> Result<(), RpcError> {
    let remote_id = connection.remote_id();
    // The accepting side opens the stream and writes first so QUIC materializes it
    // without deadlocking on the caller waiting for the challenge.
    let (mut send, recv) = connection
        .open_bi()
        .await
        .map_err(|error| RpcError::Transport(error.to_string()))?;
    let mut recv = BufReader::new(recv);
    let local_identity = peer_identity(&local)?;
    let challenge = make_challenge(&local, &endpoint)?;
    send_frame(&mut send, &challenge).await?;

    let first = read_handshake_frame(&mut recv).await?;
    let tag = serde_json::from_str::<serde_json::Value>(&first)
        .ok()
        .and_then(|value| value.get("nova")?.as_str().map(str::to_owned));
    let (device_id, public_key) = match tag.as_deref() {
        Some("hello") => {
            let hello: Hello = serde_json::from_str(&first)
                .map_err(|e| RpcError::BadParams(format!("hello decode: {e}")))?;
            let message = auth_message(
                &local_identity.device_id,
                &hello.device_id,
                &challenge.nonce,
            );
            if hello.nova != "hello"
                || !endpoint_id_matches_public_key(remote_id, &hello.public_key)
                || trust
                    .verify_signature(
                        &hello.device_id,
                        &hello.public_key,
                        &message,
                        &hello.signature,
                    )
                    .is_none()
            {
                reject(&mut send, "device not paired or identity invalid").await;
                return Err(RpcError::Failed("peer rejected".into()));
            }
            send_frame(
                &mut send,
                &Welcome {
                    nova: "welcome".into(),
                    device_id: local_identity.device_id.clone(),
                },
            )
            .await?;
            (hello.device_id, hello.public_key)
        }
        Some("pair") => {
            let request: PairRequest = serde_json::from_str(&first)
                .map_err(|e| RpcError::BadParams(format!("pair decode: {e}")))?;
            let message = auth_message(
                &local_identity.device_id,
                &request.identity.device_id,
                &challenge.nonce,
            );
            if request.nova != "pair"
                || !identity_is_valid(&request.identity)
                || !endpoint_id_matches_public_key(remote_id, &request.identity.public_key)
                || !ticket_matches_endpoint_id(&request.ticket, remote_id)
                || !verify_signature(&request.identity.public_key, &message, &request.signature)
            {
                reject(&mut send, "invalid device identity proof").await;
                return Err(RpcError::Failed("pairing identity rejected".into()));
            }
            if !pairing.consume(&request.code) {
                reject(&mut send, "pairing code invalid or expired").await;
                return Err(RpcError::Failed("pairing code rejected".into()));
            }
            trust
                .pair(TrustedPeer {
                    device_id: request.identity.device_id.clone(),
                    name: request.identity.name.clone(),
                    platform: request.identity.platform.clone(),
                    endpoint: request.ticket.clone(),
                    role: Role::Admin,
                    public_key: request.identity.public_key.clone(),
                    paired_at: chrono::Utc::now(),
                    revoked: false,
                })
                .map_err(|e| RpcError::Failed(e.to_string()))?;
            send_frame(
                &mut send,
                &Paired {
                    nova: "paired".into(),
                    identity: local_identity,
                    ticket: challenge.ticket,
                },
            )
            .await?;
            (request.identity.device_id, request.identity.public_key)
        }
        _ => {
            reject(&mut send, "expected hello or pair").await;
            return Err(RpcError::Failed("expected hello or pair".into()));
        }
    };
    drop(handshake_permit);

    let service = Arc::new(AuthorizedService::new(inner, trust, device_id, public_key));
    let (out_tx, out_rx) = mpsc::channel::<String>(256);
    let (in_tx, in_rx) = mpsc::channel::<String>(256);
    let pump = tokio::spawn(pump_stream(connection, send, recv, out_rx, in_tx));
    serve_connection(service, out_tx, in_rx).await;
    pump.abort();
    Ok(())
}

/// Pair with a device whose iroh ticket and code are visible to the user.
pub async fn pair_nova(
    ticket: &str,
    endpoint: &NovaEndpoint,
    local: &DeviceIdentityRecord,
    code: &str,
) -> Result<TrustedPeer, RpcError> {
    let target = decode_ticket(ticket)?;
    let target_id = target.id;
    let connection = endpoint
        .connect(target, ALPN)
        .await
        .map_err(|error| RpcError::Transport(error.to_string()))?;
    let (mut send, recv) = connection
        .accept_bi()
        .await
        .map_err(|error| RpcError::Transport(error.to_string()))?;
    let mut recv = BufReader::new(recv);
    let challenge: Challenge = read_json_frame(&mut recv, "invalid nova challenge").await?;
    if !challenge.verify() || !ticket_matches_endpoint_id(&challenge.ticket, target_id) {
        return Err(RpcError::Failed(
            "remote identity changed from iroh ticket".into(),
        ));
    }
    let identity = peer_identity(local)?;
    let signature = local
        .sign(&auth_message(
            &challenge.identity.device_id,
            &identity.device_id,
            &challenge.nonce,
        ))
        .ok_or_else(|| RpcError::Failed("device identity cannot sign".into()))?;
    let local_ticket = endpoint_ticket(endpoint)?;
    send_frame(
        &mut send,
        &serde_json::json!({
            "nova": "pair",
            "deviceId": identity.device_id,
            "name": identity.name,
            "platform": identity.platform,
            "publicKey": identity.public_key,
            "ticket": local_ticket,
            "code": code,
            "signature": signature,
        }),
    )
    .await?;
    let response = read_handshake_frame(&mut recv).await?;
    let paired: Paired = serde_json::from_str(&response)
        .map_err(|_| rejection_or("unexpected pairing reply", &response))?;
    if paired.nova != "paired"
        || paired.identity != challenge.identity
        || !ticket_matches_endpoint_id(&paired.ticket, target_id)
    {
        return Err(RpcError::Failed("pairing reply identity changed".into()));
    }
    connection.close(0u8.into(), b"paired");
    Ok(TrustedPeer {
        device_id: paired.identity.device_id,
        name: paired.identity.name,
        platform: paired.identity.platform,
        endpoint: paired.ticket,
        role: Role::Admin,
        public_key: paired.identity.public_key,
        paired_at: chrono::Utc::now(),
        revoked: false,
    })
}

/// Dial a previously paired Nova Engine and return an ordinary RPC client.
pub async fn connect_nova(
    ticket: &str,
    endpoint: &NovaEndpoint,
    local: &DeviceIdentityRecord,
    peer: &TrustedPeer,
) -> Result<nova_rpc::RpcClient, RpcError> {
    let target = decode_ticket(ticket)?;
    let target_id = target.id;
    let connection = endpoint
        .connect(target, ALPN)
        .await
        .map_err(|error| RpcError::Transport(error.to_string()))?;
    if connection.remote_id() != target_id {
        return Err(RpcError::Failed("remote iroh endpoint mismatch".into()));
    }
    let (mut send, recv) = connection
        .accept_bi()
        .await
        .map_err(|error| RpcError::Transport(error.to_string()))?;
    let mut recv = BufReader::new(recv);
    let challenge: Challenge = read_json_frame(&mut recv, "invalid nova challenge").await?;
    if !challenge.verify()
        || challenge.identity.device_id != peer.device_id
        || challenge.identity.public_key != peer.public_key
        || !ticket_matches_endpoint_id(&challenge.ticket, target_id)
    {
        return Err(RpcError::Failed(
            "remote identity does not match paired device".into(),
        ));
    }
    let local_identity = peer_identity(local)?;
    let signature = local
        .sign(&auth_message(
            &challenge.identity.device_id,
            &local_identity.device_id,
            &challenge.nonce,
        ))
        .ok_or_else(|| RpcError::Failed("device identity cannot sign".into()))?;
    send_frame(
        &mut send,
        &serde_json::json!({
            "nova": "hello",
            "deviceId": local_identity.device_id,
            "publicKey": local_identity.public_key,
            "signature": signature,
        }),
    )
    .await?;
    let response = read_handshake_frame(&mut recv).await?;
    let welcome: Welcome = serde_json::from_str(&response)
        .map_err(|_| rejection_or("unexpected authentication reply", &response))?;
    if welcome.nova != "welcome" || welcome.device_id != peer.device_id {
        return Err(RpcError::Failed("remote welcome identity mismatch".into()));
    }

    let (out_tx, out_rx) = mpsc::channel::<String>(256);
    let (in_tx, in_rx) = mpsc::channel::<String>(256);
    tokio::spawn(pump_stream(connection, send, recv, out_rx, in_tx));
    Ok(nova_rpc::RpcClient::new(out_tx, in_rx))
}

async fn pump_stream(
    connection: Connection,
    mut send: SendStream,
    mut recv: BufReader<RecvStream>,
    mut out_rx: mpsc::Receiver<String>,
    in_tx: mpsc::Sender<String>,
) {
    let writer = async {
        while let Some(frame) = out_rx.recv().await {
            if write_text_frame(&mut send, &frame).await.is_err() {
                break;
            }
        }
        let _ = send.finish();
    };
    let reader = async {
        while let Ok(frame) = read_rpc_frame(&mut recv).await {
            if in_tx.send(frame).await.is_err() {
                break;
            }
        }
    };
    tokio::select! {
        _ = writer => {},
        _ = reader => {},
    }
    connection.close(0u8.into(), b"rpc stream closed");
}

async fn send_frame<T: Serialize>(send: &mut SendStream, value: &T) -> Result<(), RpcError> {
    let text = serde_json::to_string(value).map_err(|e| RpcError::Transport(e.to_string()))?;
    write_text_frame_with_limit(send, &text, MAX_HANDSHAKE_FRAME_BYTES).await
}

async fn write_text_frame(send: &mut SendStream, text: &str) -> Result<(), RpcError> {
    write_text_frame_with_limit(send, text, MAX_RPC_FRAME_BYTES).await
}

async fn write_text_frame_with_limit(
    send: &mut SendStream,
    text: &str,
    limit: usize,
) -> Result<(), RpcError> {
    if text.len() > limit {
        return Err(RpcError::Transport("nova frame too large".into()));
    }
    send.write_all(text.as_bytes())
        .await
        .map_err(|e| RpcError::Transport(e.to_string()))?;
    send.write_all(b"\n")
        .await
        .map_err(|e| RpcError::Transport(e.to_string()))
}

async fn read_handshake_frame<R>(recv: &mut R) -> Result<String, RpcError>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    read_frame_with_limit(recv, MAX_HANDSHAKE_FRAME_BYTES).await
}

async fn read_rpc_frame<R>(recv: &mut R) -> Result<String, RpcError>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    read_frame_with_limit(recv, MAX_RPC_FRAME_BYTES).await
}

async fn read_frame_with_limit<R>(recv: &mut R, limit: usize) -> Result<String, RpcError>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let read = async {
        let mut bytes = Vec::new();
        loop {
            let available = recv
                .fill_buf()
                .await
                .map_err(|error| RpcError::Transport(error.to_string()))?;
            if available.is_empty() {
                return Err(RpcError::Closed);
            }
            if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
                let encoded_len = bytes.len().saturating_add(newline);
                let has_cr = if newline > 0 {
                    available[newline - 1] == b'\r'
                } else {
                    bytes.last() == Some(&b'\r')
                };
                let content_len = encoded_len.saturating_sub(usize::from(has_cr));
                if content_len > limit {
                    return Err(RpcError::Transport("nova frame too large".into()));
                }
                bytes.extend_from_slice(&available[..newline]);
                recv.consume(newline + 1);
                if bytes.last() == Some(&b'\r') {
                    bytes.pop();
                }
                return String::from_utf8(bytes)
                    .map_err(|_| RpcError::Transport("nova frame is not utf-8".into()));
            }
            let buffered_len = bytes.len().saturating_add(available.len());
            if buffered_len > limit
                && !(buffered_len == limit.saturating_add(1) && available.last() == Some(&b'\r'))
            {
                return Err(RpcError::Transport("nova frame too large".into()));
            }
            bytes.extend_from_slice(available);
            let consumed = available.len();
            recv.consume(consumed);
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(30), read)
        .await
        .map_err(|_| RpcError::Transport("nova frame timed out".into()))?
}

async fn read_json_frame<T: for<'de> Deserialize<'de>>(
    recv: &mut BufReader<RecvStream>,
    fallback: &str,
) -> Result<T, RpcError> {
    let text = read_handshake_frame(recv).await?;
    serde_json::from_str(&text).map_err(|_| rejection_or(fallback, &text))
}

async fn reject(send: &mut SendStream, reason: &str) {
    let _ = send_frame(
        send,
        &Reject {
            nova: "reject",
            reason,
        },
    )
    .await;
    let _ = send.finish();
}

fn make_challenge(
    identity: &DeviceIdentityRecord,
    endpoint: &NovaEndpoint,
) -> Result<Challenge, RpcError> {
    let identity_view = peer_identity(identity)?;
    let ticket = endpoint_ticket(endpoint)?;
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let signature = identity
        .sign(&challenge_message(
            &identity_view.device_id,
            &ticket,
            &nonce,
        ))
        .ok_or_else(|| RpcError::Failed("device identity cannot sign".into()))?;
    Ok(Challenge {
        nova: "challenge".into(),
        identity: identity_view,
        ticket,
        nonce,
        signature,
    })
}

fn rejection_or(fallback: &str, text: &str) -> RpcError {
    let reason = serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .filter(|value| value.get("nova").and_then(|v| v.as_str()) == Some("reject"))
        .and_then(|value| value.get("reason")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| fallback.to_string());
    RpcError::Failed(reason)
}

fn identity_is_valid(identity: &PeerIdentity) -> bool {
    !identity.device_id.is_empty()
        && decode_hex_vec(&identity.public_key).is_some_and(|key| key.len() == 32)
}

fn endpoint_id_matches_public_key(endpoint_id: EndpointId, public_key: &str) -> bool {
    decode_hex_vec(public_key).is_some_and(|bytes| bytes.as_slice() == endpoint_id.as_bytes())
}

fn ticket_matches_public_key(ticket: &str, public_key: &str) -> bool {
    decode_ticket(ticket)
        .ok()
        .is_some_and(|addr| endpoint_id_matches_public_key(addr.id, public_key))
}

fn ticket_matches_endpoint_id(ticket: &str, endpoint_id: EndpointId) -> bool {
    decode_ticket(ticket)
        .ok()
        .is_some_and(|addr| addr.id == endpoint_id)
}

fn challenge_message(device_id: &str, ticket: &str, nonce: &str) -> Vec<u8> {
    format!("{CHALLENGE_TAG}\0{device_id}\0{ticket}\0{nonce}").into_bytes()
}

fn auth_message(server_id: &str, client_id: &str, nonce: &str) -> Vec<u8> {
    format!("{AUTH_TAG}\0{server_id}\0{client_id}\0{nonce}").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Svc;

    #[async_trait::async_trait]
    impl RpcService for Svc {
        async fn handle(&self, method: &str, _p: serde_json::Value) -> Result<RpcReply, RpcError> {
            match method {
                nova_rpc::methods::LIST_MODELS => {
                    Ok(RpcReply::Value(serde_json::json!({"ok": true})))
                }
                nova_rpc::methods::SET_PI_CREDENTIAL => {
                    Ok(RpcReply::Value(serde_json::json!({"ok": true})))
                }
                _ => Err(RpcError::UnknownMethod(method.into())),
            }
        }
    }

    fn identity(dir: &tempfile::TempDir, name: &str) -> DeviceIdentityRecord {
        let path = dir.path().join(format!("{name}.json"));
        let mut identity = DeviceIdentityRecord::load_or_create(&path, "test").unwrap();
        identity.name = name.into();
        identity
    }

    async fn local_endpoint(identity: &DeviceIdentityRecord) -> NovaEndpoint {
        bind_endpoint(identity, 0, false).await.unwrap()
    }

    #[tokio::test]
    async fn ticket_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let identity = identity(&dir, "ticket");
        let endpoint = local_endpoint(&identity).await;
        let ticket = endpoint_ticket(&endpoint).unwrap();
        let decoded = decode_ticket(&ticket).unwrap();
        assert_eq!(decoded.id, endpoint.id());
        assert!(ticket_matches_public_key(
            &ticket,
            &identity.public_key().unwrap()
        ));
        endpoint.close().await;
    }

    #[tokio::test]
    async fn discovery_websocket_only_sends_a_signed_challenge_then_closes() {
        use futures::StreamExt as _;

        let dir = tempfile::tempdir().unwrap();
        let identity = identity(&dir, "discovery");
        let endpoint = local_endpoint(&identity).await;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(serve_discovery_listener(
            listener,
            identity,
            endpoint.clone(),
        ));

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}"))
            .await
            .unwrap();
        let first = ws.next().await.unwrap().unwrap();
        let WsMessage::Text(text) = first else {
            panic!("expected signed discovery challenge");
        };
        assert!(challenge_from_text(&text).is_some());
        assert!(matches!(
            ws.next().await,
            Some(Ok(WsMessage::Close(_))) | None
        ));

        task.abort();
        endpoint.close().await;
    }

    #[tokio::test]
    async fn real_pairing_then_authenticated_rpc() {
        let server_dir = tempfile::tempdir().unwrap();
        let client_dir = tempfile::tempdir().unwrap();
        let server_identity = identity(&server_dir, "server");
        let client_identity = identity(&client_dir, "client");
        let server_endpoint = local_endpoint(&server_identity).await;
        let client_endpoint = local_endpoint(&client_identity).await;
        let server_ticket = endpoint_ticket(&server_endpoint).unwrap();
        let server_trust = Arc::new(Trust::load(server_dir.path().join("trust.json")));
        let client_trust = Arc::new(Trust::load(client_dir.path().join("trust.json")));
        let pairing = Arc::new(PairingState::default());
        let code = pairing.begin().code;
        tokio::spawn(serve_iroh_endpoint(
            server_endpoint.clone(),
            Arc::new(Svc),
            server_trust.clone(),
            server_identity.clone(),
            pairing,
        ));
        let server_peer = pair_nova(&server_ticket, &client_endpoint, &client_identity, &code)
            .await
            .unwrap();
        client_trust.pair(server_peer.clone()).unwrap();
        assert!(server_trust.is_trusted(&client_identity.device_id));
        assert!(client_trust.is_trusted(&server_identity.device_id));

        let client = connect_nova(
            &server_peer.endpoint,
            &client_endpoint,
            &client_identity,
            &server_peer,
        )
        .await
        .unwrap();
        assert!(
            client
                .call(nova_rpc::methods::LIST_MODELS, serde_json::Value::Null)
                .await
                .is_ok()
        );
        assert!(
            client
                .call(
                    nova_rpc::methods::SET_PI_CREDENTIAL,
                    serde_json::Value::Null,
                )
                .await
                .is_ok()
        );

        server_trust.revoke(&client_identity.device_id).unwrap();
        let error = client
            .call(nova_rpc::methods::LIST_MODELS, serde_json::Value::Null)
            .await
            .expect_err("revocation must affect an already-open connection");
        assert!(error.to_string().contains("trust changed or was revoked"));
        server_endpoint.close().await;
        client_endpoint.close().await;
    }

    #[tokio::test]
    async fn bad_pairing_code_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let client_dir = tempfile::tempdir().unwrap();
        let server = identity(&dir, "server");
        let client = identity(&client_dir, "client");
        let server_endpoint = local_endpoint(&server).await;
        let client_endpoint = local_endpoint(&client).await;
        let ticket = endpoint_ticket(&server_endpoint).unwrap();
        tokio::spawn(serve_iroh_endpoint(
            server_endpoint.clone(),
            Arc::new(Svc),
            Arc::new(Trust::load(dir.path().join("trust.json"))),
            server,
            Arc::new(PairingState::default()),
        ));
        assert!(
            pair_nova(&ticket, &client_endpoint, &client, "000000")
                .await
                .is_err()
        );
        server_endpoint.close().await;
        client_endpoint.close().await;
    }

    #[tokio::test]
    async fn frame_reader_rejects_before_buffering_past_limit() {
        use tokio::io::AsyncWriteExt as _;

        let (mut writer, reader) = tokio::io::duplex(256);
        tokio::spawn(async move {
            writer.write_all(&[b'x'; 65]).await.unwrap();
            writer.write_all(b"\n").await.unwrap();
        });
        let mut reader = BufReader::new(reader);
        let error = read_frame_with_limit(&mut reader, 64)
            .await
            .expect_err("oversized frame must fail");
        assert!(error.to_string().contains("frame too large"));
    }

    #[tokio::test]
    async fn frame_reader_accepts_exact_limit_and_crlf() {
        use tokio::io::AsyncWriteExt as _;

        let (mut writer, reader) = tokio::io::duplex(256);
        tokio::spawn(async move {
            writer.write_all(&[b'x'; 64]).await.unwrap();
            writer.write_all(b"\r\n").await.unwrap();
        });
        let mut reader = BufReader::with_capacity(1, reader);
        assert_eq!(
            read_frame_with_limit(&mut reader, 64).await.unwrap().len(),
            64
        );
    }

    #[test]
    fn rpc_frame_limit_fits_max_encoded_peer_sync_exchange() {
        let max_binary_exchange: usize = 128 * 1024 * 1024;
        let max_base64_len = max_binary_exchange.div_ceil(3) * 4;
        assert!(MAX_RPC_FRAME_BYTES > max_base64_len);
    }

    #[test]
    fn public_key_text_is_same_key_as_iroh_endpoint() {
        let secret = crate::identity::DeviceSecret::generate();
        let endpoint_id = SecretKey::from_bytes(&secret.0).public();
        assert_eq!(
            crate::identity::hex(endpoint_id.as_bytes()),
            secret.public_key().unwrap()
        );
    }
}
