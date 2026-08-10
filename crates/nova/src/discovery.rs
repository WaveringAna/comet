//! LAN discovery for Nova peers configured from Nova settings.
//!
//! Scanning is deliberately defensive:
//!   - **Explicit user action only** — nothing runs until the user clicks Scan.
//!   - **No default Internet-wide scans** — ranges come from user-entered CIDRs, each
//!     validated + private-checked by [`crate::cidr`].
//!   - **Bounded concurrency** — at most [`ScanOptions::concurrency`] probes in flight.
//!   - **Short per-host timeout** — [`ScanOptions::per_host_timeout`], default 250ms.
//!   - **Cancellable** — pass a `CancellationToken`; aborting returns partial results.
//!   - **Deduplicated** — overlapping ranges never double-probe an address.
//!   - **Capped** — a scan returns a clear error rather than silently truncating a range
//!     larger than [`ScanOptions::max_candidates`].
//!
//! The probe is a TCP connect to the Nova Engine control port, optionally followed by a
//! short "hello" handshake that identifies a Nova peer. Strangers (a service on the port
//! that is not a Nova engine) are reported as `Unknown` and never auto-trusted.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::cidr::CidrRange;
use crate::transport::challenge_from_text;

/// Tunable knobs for a scan. Defaults are conservative.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub concurrency: usize,
    pub per_host_timeout: Duration,
    /// Hard cap on the enumerated candidate set across all ranges.
    pub max_candidates: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            concurrency: 64,
            per_host_timeout: Duration::from_millis(250),
            max_candidates: 65_536,
        }
    }
}

/// A discovered endpoint. `Nova` means the probe identified a Nova Engine (matched the
/// magic bytes); `Open` means something answered on the port that wasn't Nova;
/// `Unreachable` addresses are omitted from the result stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum FoundPeer {
    /// A Nova engine answered with its public device id and name.
    Nova {
        addr: String,
        device_id: String,
        name: String,
        ticket: String,
        trusted: bool,
    },
    /// A non-Nova service on the port — surfaced so the user can see what's there,
    /// never auto-paired.
    Open { addr: String },
}

impl FoundPeer {
    pub fn addr(&self) -> &str {
        match self {
            FoundPeer::Nova { addr, .. } | FoundPeer::Open { addr } => addr,
        }
    }
}

#[derive(Debug, Clone)]
pub enum ProbeReply {
    Nova {
        device_id: String,
        name: String,
        ticket: String,
    },
    Open,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DiscoveryError {
    #[error(
        "scan contains more than {max} addresses; use /16 or smaller ranges and scan them separately"
    )]
    TooManyCandidates { max: usize },
}

/// Enumerate candidate addresses from a set of validated CIDRs, deduplicated and capped.
pub fn candidate_addresses(
    ranges: &[CidrRange],
    opts: &ScanOptions,
) -> Result<Vec<String>, DiscoveryError> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for range in ranges {
        let range_size = match range.family {
            crate::cidr::AddrFamily::V4 => 1_u128 << (32 - range.prefix),
            crate::cidr::AddrFamily::V6 => 2,
        };
        if range_size > opts.max_candidates as u128 {
            return Err(DiscoveryError::TooManyCandidates {
                max: opts.max_candidates,
            });
        }
        for host in range.hosts() {
            if seen.insert(host.clone()) {
                out.push(host);
                if out.len() > opts.max_candidates {
                    return Err(DiscoveryError::TooManyCandidates {
                        max: opts.max_candidates,
                    });
                }
            }
        }
    }
    Ok(out)
}

/// A transport-injected probe. The real engine passes a TCP-connecting probe; tests
/// pass a fake so the scan logic runs without touching the network.
#[async_trait::async_trait]
pub trait Probe: Send + Sync + 'static {
    async fn probe(&self, addr: &str) -> Result<Option<ProbeReply>, ()>;
}

/// The production probe performs the same WebSocket upgrade as a real peer, then reads
/// and verifies the listener's signed public challenge. It sends no auth or pairing data.
pub struct NovaProbe {
    pub port: u16,
    pub timeout: Duration,
}

#[async_trait::async_trait]
impl Probe for NovaProbe {
    async fn probe(&self, addr: &str) -> Result<Option<ProbeReply>, ()> {
        let url = if addr.contains(':') {
            format!("ws://[{addr}]:{}", self.port)
        } else {
            format!("ws://{addr}:{}", self.port)
        };
        let (mut ws, _) = match tokio::time::timeout(
            self.timeout,
            tokio_tungstenite::connect_async(&url),
        )
        .await
        {
            Ok(Ok(pair)) => pair,
            _ => return Err(()),
        };
        use futures::{SinkExt as _, StreamExt as _};
        let message = match tokio::time::timeout(self.timeout, ws.next()).await {
            Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text)))) => text,
            Ok(Some(Ok(_))) => return Ok(Some(ProbeReply::Open)),
            _ => return Ok(None),
        };
        let reply = challenge_from_text(&message)
            .map(|challenge| ProbeReply::Nova {
                device_id: challenge.identity.device_id,
                name: challenge.identity.name,
                ticket: challenge.ticket,
            })
            .unwrap_or(ProbeReply::Open);
        let _ = ws
            .send(tokio_tungstenite::tungstenite::Message::Close(None))
            .await;
        Ok(Some(reply))
    }
}

/// Run a scan. Streams found peers as the scan progresses (callers render them live).
/// Honors the cancellation token: aborting yields the peers found so far and stops.
pub async fn scan(
    ranges: &[CidrRange],
    probe: Arc<dyn Probe>,
    opts: ScanOptions,
    cancel: CancellationToken,
    mut on_found: impl FnMut(FoundPeer) + Send,
    known_trusted: impl Fn(&str) -> bool + Send + Sync + 'static,
) -> Result<Vec<FoundPeer>, DiscoveryError> {
    let candidates = candidate_addresses(ranges, &opts)?;
    let known_trusted = Arc::new(known_trusted);
    let concurrency = opts.concurrency.max(1);
    let mut candidates = candidates.into_iter();
    let mut tasks = tokio::task::JoinSet::new();
    let mut final_results = Vec::new();
    loop {
        while !cancel.is_cancelled() && tasks.len() < concurrency {
            let Some(addr) = candidates.next() else {
                break;
            };
            let probe = probe.clone();
            let cancel = cancel.clone();
            let trusted_fn = known_trusted.clone();
            let timeout = opts.per_host_timeout;
            tasks.spawn(async move {
                if cancel.is_cancelled() {
                    return None;
                }
                let reply = tokio::time::timeout(timeout, probe.probe(&addr))
                    .await
                    .ok()?
                    .ok()??;
                Some(match reply {
                    ProbeReply::Nova {
                        device_id,
                        name,
                        ticket,
                    } => FoundPeer::Nova {
                        addr,
                        trusted: trusted_fn(&device_id),
                        device_id,
                        name,
                        ticket,
                    },
                    ProbeReply::Open => FoundPeer::Open { addr },
                })
            });
        }

        if cancel.is_cancelled() {
            tasks.abort_all();
            break;
        }
        match tasks.join_next().await {
            Some(Ok(Some(peer))) => {
                on_found(peer.clone());
                final_results.push(peer);
            }
            Some(_) => {}
            None => break,
        }
    }
    final_results.sort_by(|a, b| a.addr().cmp(b.addr()));
    Ok(final_results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cidr::CidrRange;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ScriptedProbe {
        responses: std::sync::Mutex<std::collections::HashMap<String, ProbeReply>>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Probe for ScriptedProbe {
        async fn probe(&self, addr: &str) -> Result<Option<ProbeReply>, ()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let g = self.responses.lock().unwrap();
            if let Some(b) = g.get(addr) {
                Ok(Some(b.clone()))
            } else {
                Err(())
            }
        }
    }

    #[tokio::test]
    async fn scans_small_range_dedups_and_classifies() {
        let range = CidrRange::parse("192.168.1.0/30", false).unwrap(); // 4 hosts
        let mut responses = std::collections::HashMap::new();
        responses.insert(
            "192.168.1.1".to_string(),
            ProbeReply::Nova {
                device_id: "devid1".into(),
                name: "laptop".into(),
                ticket: "ticket-1".into(),
            },
        );
        responses.insert("192.168.1.2".to_string(), ProbeReply::Open);
        // .0 and .3 are closed.
        let probe = Arc::new(ScriptedProbe {
            responses: std::sync::Mutex::new(responses),
            calls: Arc::new(AtomicUsize::new(0)),
        });

        let opts = ScanOptions {
            concurrency: 4,
            per_host_timeout: Duration::from_millis(100),
            max_candidates: 1024,
        };
        let found = scan(
            &[range],
            probe.clone(),
            opts,
            CancellationToken::new(),
            |_| {},
            |_| false,
        )
        .await
        .unwrap();

        // 2 reported (Nova + Open); closed hosts omitted.
        assert_eq!(found.len(), 2);
        assert!(
            found
                .iter()
                .any(|p| matches!(p, FoundPeer::Nova { device_id, .. } if device_id == "devid1"))
        );
        assert!(found.iter().any(|p| matches!(p, FoundPeer::Open { .. })));
        // Dedup: 4 candidates each probed exactly once.
        assert_eq!(probe.calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn cancellation_stops_early() {
        // Build a /28 (16 hosts) all closed except one, then cancel immediately.
        let range = CidrRange::parse("192.168.2.0/28", false).unwrap();
        let probe = Arc::new(ScriptedProbe {
            responses: std::sync::Mutex::new(std::collections::HashMap::new()),
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let cancel = CancellationToken::new();
        cancel.cancel();
        let opts = ScanOptions::default();
        let found = scan(&[range], probe.clone(), opts, cancel, |_| {}, |_| false)
            .await
            .unwrap();
        assert!(found.is_empty());
        // Cancelled before spawning any task.
        assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
    }

    struct DelayedProbe;

    #[async_trait::async_trait]
    impl Probe for DelayedProbe {
        async fn probe(&self, addr: &str) -> Result<Option<ProbeReply>, ()> {
            if addr.ends_with(".1") {
                return Ok(Some(ProbeReply::Nova {
                    device_id: "early".into(),
                    name: "early peer".into(),
                    ticket: "ticket-early".into(),
                }));
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
            Err(())
        }
    }

    #[tokio::test]
    async fn reports_results_before_the_scan_finishes() {
        let range = CidrRange::parse("192.168.3.0/30", false).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            scan(
                &[range],
                Arc::new(DelayedProbe),
                ScanOptions {
                    concurrency: 4,
                    per_host_timeout: Duration::from_secs(1),
                    max_candidates: 16,
                },
                CancellationToken::new(),
                move |peer| {
                    let _ = tx.send(peer);
                },
                |_| false,
            )
            .await
            .unwrap()
        });

        let first = tokio::time::timeout(Duration::from_millis(75), rx.recv())
            .await
            .expect("first result should arrive while slow probes are running")
            .expect("result channel remains open");
        assert!(matches!(
            first,
            FoundPeer::Nova { device_id, .. } if device_id == "early"
        ));
        assert!(!task.is_finished(), "slow probes should still be running");
        assert_eq!(task.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn trusted_peers_are_marked() {
        let range = CidrRange::parse("10.0.0.0/30", false).unwrap();
        let mut responses = std::collections::HashMap::new();
        responses.insert(
            "10.0.0.1".to_string(),
            ProbeReply::Nova {
                device_id: "known".into(),
                name: "box".into(),
                ticket: "ticket-known".into(),
            },
        );
        let probe = Arc::new(ScriptedProbe {
            responses: std::sync::Mutex::new(responses),
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let found = scan(
            &[range],
            probe,
            ScanOptions::default(),
            CancellationToken::new(),
            |_| {},
            |device_id| device_id == "known",
        )
        .await
        .unwrap();
        match found.iter().find(|p| p.addr() == "10.0.0.1") {
            Some(FoundPeer::Nova { trusted, .. }) => assert!(*trusted),
            other => panic!("expected trusted Nova peer, got {other:?}"),
        }
    }

    #[test]
    fn candidate_dedup_across_ranges() {
        let a = CidrRange::parse("192.168.1.0/30", false).unwrap();
        let b = CidrRange::parse("192.168.1.2/31", false).unwrap();
        let opts = ScanOptions::default();
        let cands = candidate_addresses(&[a, b], &opts).unwrap();
        // .2 and .3 appear in both ranges; dedup.
        let mut sorted = cands.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(cands.len(), sorted.len());
        assert!(cands.contains(&"192.168.1.2".to_string()));
    }

    #[test]
    fn candidate_cap_returns_a_clear_error() {
        let r = CidrRange::parse("10.0.0.0/16", false).unwrap();
        let opts = ScanOptions {
            max_candidates: 100,
            ..ScanOptions::default()
        };
        let error = candidate_addresses(&[r], &opts).unwrap_err();
        assert_eq!(error, DiscoveryError::TooManyCandidates { max: 100 });
    }
}
