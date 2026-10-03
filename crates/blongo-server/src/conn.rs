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

/// Run the handshake. `None`: refused (already answered) or gone.
async fn handshake(
    reader: &mut Reader,
    writer: &mut Writer,
    transport: Transport,
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
    let result = match request {
        AuthRequest::Local if transport.is_local() => Ok(Authenticated {
            device_id: None,
            issued: None,
            resume: None,
            label: format!("local ({})", transport.name()),
        }),
        AuthRequest::Local => Err("local auth on a network transport".to_owned()),
        AuthRequest::Token {
            device_id,
            token,
            proof,
        } => tokio::task::spawn_blocking(move || {
            auth.verify_token(&device_id, &token, &proof, &nonce)
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r.map_err(|e| e.to_string()))
        .map(|device| Authenticated {
            label: format!("device {:?} ({})", device.name, device.id),
            device_id: Some(device.id),
            issued: None,
            resume: None,
        }),
        AuthRequest::Pair {
            code,
            device_name,
            public_key,
            proof,
        } => tokio::task::spawn_blocking(move || {
            auth.pair(&code, &device_name, &public_key, &proof, &nonce)
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r.map_err(|e| e.to_string()))
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
        Err(reason) => {
            // Never log what was presented; the reason names no secret.
            eprintln!(
                "blongo-serve: refused a {} connection: {reason}",
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

pub async fn serve(
    mut reader: Reader,
    mut writer: Writer,
    transport: Transport,
    shared: Arc<Shared>,
) {
    let conn: ConnId = shared.next_conn.fetch_add(1, Ordering::Relaxed);
    let authed = match tokio::time::timeout(
        shared.limits.handshake_timeout,
        handshake(&mut reader, &mut writer, transport, &shared),
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
    loop {
        let msg = tokio::select! {
            msg = read_msg(&mut reader) => msg,
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
                let _ = shared.hub.send(HubMsg::Cwd(thread_id, tx));
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
            ClientMsg::Hello(_) | ClientMsg::Auth(_) => break,
        };
        if let Some(m) = to_hub
            && shared.hub.send(m).is_err()
        {
            break;
        }
    }
    terminals.close_all();
    let _ = shared.hub.send(HubMsg::Leave(conn));
    outbox.close();
    let _ = tokio::time::timeout(Duration::from_secs(5), writer_task).await;
    eprintln!("blongo-serve: connection {conn} closed");
}
