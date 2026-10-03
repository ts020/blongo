//! One client connection: handshake, then a reader (client messages → hub,
//! terminals) and a writer (outbox → frames).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use blongo_client::secret::random;
use blongo_client::transport::{Reader, Writer};
use blongo_protocol::wire::{
    AuthRequest, Challenge, ClientMsg, IssuedCredential, MAX_CLIENT_FRAME, MAX_SERVER_FRAME,
    RefuseCode, ServerFrame, ServerMsg, decode, encode, negotiate_caps, negotiate_version,
};
use tokio::sync::oneshot;

use crate::hub::{ConnId, HubMsg};
use crate::outbox::Outbox;
use crate::pty::Terminals;
use crate::{Shared, Transport};

/// Most bytes one frame carries (several queued messages).
const FRAME_BUDGET: usize = 256 << 10;
/// Most terminal input chunks queued for one terminal's writer.
pub(crate) const TERMINAL_INPUT_QUEUE: usize = 256;

async fn send_now(writer: &mut Writer, msgs: Vec<ServerMsg>) -> std::io::Result<()> {
    let bytes = encode(&ServerFrame(msgs), MAX_SERVER_FRAME)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    writer.write_frame(bytes).await
}

async fn read_msg(reader: &mut Reader) -> Result<ClientMsg, String> {
    match reader.read_frame().await {
        Ok(Some(bytes)) => decode(&bytes, MAX_CLIENT_FRAME).map_err(|e| e.to_string()),
        Ok(None) => Err("closed".into()),
        Err(e) => Err(e.to_string()),
    }
}

async fn refuse(writer: &mut Writer, code: RefuseCode, message: &str) {
    let _ = send_now(
        writer,
        vec![ServerMsg::Refused {
            code,
            message: message.into(),
        }],
    )
    .await;
    writer.close().await;
}

struct Authenticated {
    device_id: Option<String>,
    issued: Option<IssuedCredential>,
    resume: Option<blongo_protocol::wire::Resume>,
    label: String,
}

/// Why authentication did not succeed: refused for good, or the server
/// could not check (its state could not be read: retry later).
enum AuthFailure {
    Refused(String),
    Unavailable(String),
}

impl From<crate::auth::AuthError> for AuthFailure {
    fn from(e: crate::auth::AuthError) -> Self {
        match e {
            crate::auth::AuthError::Storage(_) => Self::Unavailable(e.to_string()),
            e => Self::Refused(e.to_string()),
        }
    }
}

/// Run the handshake. `None`: refused (already answered) or gone.
async fn handshake(
    reader: &mut Reader,
    writer: &mut Writer,
    transport: Transport,
    peer: &str,
    shared: &Shared,
) -> Option<Authenticated> {
    let hello = match read_msg(reader).await {
        Ok(ClientMsg::Hello(hello)) => hello,
        Ok(_) => {
            refuse(writer, RefuseCode::Protocol, "expected hello").await;
            return None;
        }
        Err(_) => return None,
    };
    let Some(version) = negotiate_version(&hello) else {
        refuse(
            writer,
            RefuseCode::Version,
            &format!(
                "no common protocol version (server speaks {}..={})",
                blongo_protocol::wire::MIN_PROTOCOL_VERSION,
                blongo_protocol::wire::PROTOCOL_VERSION
            ),
        )
        .await;
        return None;
    };
    let nonce = random::<32>();
    let challenge = Challenge {
        version,
        capabilities: negotiate_caps(&hello.capabilities),
        server_id: shared.auth.server_id().to_owned(),
        nonce: nonce.to_vec(),
        epoch: shared.epoch,
    };
    send_now(writer, vec![ServerMsg::Challenge(challenge)])
        .await
        .ok()?;
    let request = match read_msg(reader).await {
        Ok(ClientMsg::Auth(request)) => request,
        Ok(_) => {
            refuse(writer, RefuseCode::Protocol, "expected auth").await;
            return None;
        }
        Err(_) => return None,
    };
    let auth = shared.auth.clone();
    let joined = |e: tokio::task::JoinError| AuthFailure::Unavailable(e.to_string());
    let result = match request {
        AuthRequest::Local if transport.is_local() => Ok(Authenticated {
            device_id: None,
            issued: None,
            resume: None,
            label: format!("local ({})", transport.name()),
        }),
        AuthRequest::Local => Err(AuthFailure::Refused(
            "local auth on a network transport".to_owned(),
        )),
        AuthRequest::Token { device_id, proof } => {
            tokio::task::spawn_blocking(move || auth.verify_token(&device_id, &proof, &nonce))
                .await
                .map_err(joined)
                .and_then(|r| r.map_err(AuthFailure::from))
                .map(|device| Authenticated {
                    label: format!("device {:?} ({})", device.name, device.id),
                    device_id: Some(device.id),
                    issued: None,
                    resume: None,
                })
        }
        AuthRequest::Pair {
            device_name,
            public_key,
            proof,
        } => tokio::task::spawn_blocking(move || {
            auth.pair(&device_name, &public_key, &proof, &nonce)
        })
        .await
        .map_err(joined)
        .and_then(|r| r.map_err(AuthFailure::from))
        .map(|(device, token)| {
            eprintln!(
                "blongo-serve: paired device {:?} ({})",
                device.name, device.id
            );
            Authenticated {
                label: format!("device {:?} ({})", device.name, device.id),
                device_id: Some(device.id.clone()),
                issued: Some(IssuedCredential {
                    device_id: device.id,
                    token,
                }),
                resume: None,
            }
        }),
    };
    match result {
        Ok(mut ok) => {
            ok.resume = hello.resume;
            Some(ok)
        }
        Err(AuthFailure::Unavailable(reason)) => {
            eprintln!(
                "blongo-serve: cannot check a {} connection from {peer}: {reason}",
                transport.name()
            );
            refuse(
                writer,
                RefuseCode::Unavailable,
                "the server cannot check credentials right now",
            )
            .await;
            None
        }
        Err(AuthFailure::Refused(reason)) => {
            // Never log what was presented; the reason names no secret.
            eprintln!(
                "blongo-serve: refused a {} connection from {peer}: {reason}",
                transport.name()
            );
            shared.stats.refused.fetch_add(1, Ordering::Relaxed);
            // Slow down guessing.
            tokio::time::sleep(shared.limits.refuse_delay).await;
            refuse(writer, RefuseCode::Unauthorized, "authentication failed").await;
            None
        }
    }
}

/// Serve one connection. `pre_auth` is a slot held only until the
/// handshake ends (bounds connections that have not authenticated yet).
pub async fn serve(
    mut reader: Reader,
    mut writer: Writer,
    transport: Transport,
    peer: String,
    pre_auth: Option<crate::PreAuthSlot>,
    shared: Arc<Shared>,
) {
    let conn: ConnId = shared.next_conn.fetch_add(1, Ordering::Relaxed);
    let authed = match tokio::time::timeout(
        shared.limits.handshake_timeout,
        handshake(&mut reader, &mut writer, transport, &peer, &shared),
    )
    .await
    {
        Ok(Some(a)) => a,
        Ok(None) => return,
        Err(_) => {
            refuse(&mut writer, RefuseCode::Protocol, "handshake timed out").await;
            return;
        }
    };
    drop(pre_auth);
    // Administration needs a transport the OS authenticated, not a device.
    let admin = transport.is_local() && authed.device_id.is_none();
    eprintln!("blongo-serve: connection {conn}: {}", authed.label);
    let outbox = Arc::new(Outbox::new(shared.limits.outbox));
    if shared
        .hub
        .send(HubMsg::Join {
            conn,
            outbox: outbox.clone(),
            resume: authed.resume,
            device_id: authed.device_id,
            issued: authed.issued,
        })
        .await
        .is_err()
    {
        return;
    }
    let write_timeout = shared.limits.write_timeout;
    let writer_outbox = outbox.clone();
    let writer_task = tokio::spawn(async move {
        while let Some(msgs) = writer_outbox.drain(FRAME_BUDGET).await {
            let bytes = match encode(&ServerFrame(msgs), MAX_SERVER_FRAME) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("blongo-serve: connection {conn}: dropped a frame: {e}");
                    continue;
                }
            };
            match tokio::time::timeout(write_timeout, writer.write_frame(bytes)).await {
                Ok(Ok(())) => {}
                _ => break,
            }
        }
        writer_outbox.close();
        writer.close().await;
    });
    let mut terminals = Terminals::default();
    let idle = shared.limits.idle_timeout;
    loop {
        let msg = tokio::select! {
            msg = tokio::time::timeout(idle, read_msg(&mut reader)) => match msg {
                Ok(msg) => msg,
                Err(_) => {
                    eprintln!("blongo-serve: connection {conn}: silent for {}s; closing it", idle.as_secs());
                    break;
                }
            },
            _ = outbox.closed() => break,
        };
        let Ok(msg) = msg else { break };
        let to_hub = match msg {
            ClientMsg::Command(c) => Some(HubMsg::Command(conn, c)),
            ClientMsg::Subscribe { threads } => Some(HubMsg::Subscribe(conn, threads)),
            ClientMsg::Login { provider } => Some(HubMsg::Login(provider)),
            ClientMsg::InstallAntigravity => Some(HubMsg::InstallAntigravity),
            ClientMsg::ImportT3 { path } => Some(HubMsg::ImportT3(path)),
            ClientMsg::Ping { at } => {
                outbox.push(ServerMsg::Pong { at });
                None
            }
            ClientMsg::TerminalOpen {
                id,
                thread_id,
                columns,
                lines,
            } => {
                let (tx, rx) = oneshot::channel();
                let _ = shared.hub.send(HubMsg::Cwd(thread_id, tx)).await;
                let result = match rx.await.ok().flatten() {
                    Some(cwd) => terminals
                        .open(id, &PathBuf::from(cwd), columns, lines, outbox.clone())
                        .map_err(|e| format!("{e:#}")),
                    None => Err("unknown thread".into()),
                };
                if let Err(message) = result {
                    outbox.push(ServerMsg::TerminalFailed { id, message });
                }
                None
            }
            ClientMsg::TerminalInput { id, data } => {
                terminals.input(id, data);
                None
            }
            ClientMsg::TerminalResize { id, columns, lines } => {
                terminals.resize(id, columns, lines);
                None
            }
            ClientMsg::TerminalClose { id } => {
                terminals.close(id);
                None
            }
            ClientMsg::Revoke { device } if admin => {
                let auth = shared.auth.clone();
                let result = tokio::task::spawn_blocking(move || auth.revoke(&device))
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|r| r.map_err(|e| e.to_string()));
                if let Ok(ids) = &result
                    && !ids.is_empty()
                {
                    eprintln!("blongo-serve: revoked {} device(s)", ids.len());
                    let _ = shared.hub.send(HubMsg::Revoke(ids.clone())).await;
                }
                outbox.push(ServerMsg::Revoked(result.map(|ids| ids.len())));
                None
            }
            ClientMsg::Revoke { .. } => {
                outbox.push(ServerMsg::Revoked(Err(
                    "administration needs the server's local socket".into(),
                )));
                None
            }
            ClientMsg::Hello(_) | ClientMsg::Auth(_) => break,
        };
        if let Some(m) = to_hub
            && shared.hub.send(m).await.is_err()
        {
            break;
        }
    }
    terminals.close_all();
    let _ = shared.hub.send(HubMsg::Leave(conn)).await;
    outbox.close();
    let _ = tokio::time::timeout(Duration::from_secs(5), writer_task).await;
    eprintln!("blongo-serve: connection {conn} closed");
}
