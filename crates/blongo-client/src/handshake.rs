//! The client side of the handshake: `Hello` → `Challenge` → `Auth` →
//! `Welcome` / `Refused`.

use std::time::Duration;

use blongo_protocol::wire::{
    AuthRequest, Challenge, ClientMsg, Hello, MAX_CLIENT_FRAME, MAX_SERVER_FRAME, ProofPurpose,
    RefuseCode, ServerFrame, ServerMsg, Welcome, decode, encode,
};
use ed25519_dalek::SigningKey;

use crate::environments::Credential;
use crate::secret::{device_key_from_b64, make_proof};
use crate::transport::{Reader, Writer};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

pub enum ClientAuth<'a> {
    Credential(&'a Credential),
    Pair {
        code: &'a str,
        device_name: &'a str,
        key: &'a SigningKey,
    },
    /// The transport authenticated us (SSH stdio).
    Local,
}

#[derive(Debug)]
pub enum HandshakeError {
    /// The server said no. Permanent for bad credentials and versions.
    Refused {
        code: RefuseCode,
        message: String,
    },
    /// Not the server this credential was issued by.
    WrongServer,
    Io(String),
    Protocol(String),
}

impl HandshakeError {
    pub fn is_permanent(&self) -> bool {
        match self {
            Self::Refused { code, .. } => code.is_permanent(),
            Self::WrongServer => true,
            Self::Io(_) | Self::Protocol(_) => false,
        }
    }
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused { code, message } => write!(f, "refused ({code:?}): {message}"),
            Self::WrongServer => write!(
                f,
                "the server's identity changed: it is not the server this credential was paired with"
            ),
            Self::Io(e) => write!(f, "{e}"),
            Self::Protocol(e) => write!(f, "protocol error: {e}"),
        }
    }
}

pub struct Session {
    pub challenge: Challenge,
    pub welcome: Welcome,
    /// Messages that arrived in the same frame after `Welcome`.
    pub rest: Vec<ServerMsg>,
}

pub async fn send(writer: &mut Writer, msg: &ClientMsg) -> Result<(), HandshakeError> {
    let bytes =
        encode(msg, MAX_CLIENT_FRAME).map_err(|e| HandshakeError::Protocol(e.to_string()))?;
    writer
        .write_frame(bytes)
        .await
        .map_err(|e| HandshakeError::Io(e.to_string()))
}

pub async fn recv(reader: &mut Reader) -> Result<Vec<ServerMsg>, HandshakeError> {
    match reader.read_frame().await {
        Ok(Some(bytes)) => decode::<ServerFrame>(&bytes, MAX_SERVER_FRAME)
            .map(|f| f.0)
            .map_err(|e| HandshakeError::Protocol(e.to_string())),
        Ok(None) => Err(HandshakeError::Io(
            "the server closed the connection".into(),
        )),
        Err(e) => Err(HandshakeError::Io(e.to_string())),
    }
}

pub async fn handshake(
    reader: &mut Reader,
    writer: &mut Writer,
    hello: Hello,
    auth: ClientAuth<'_>,
) -> Result<Session, HandshakeError> {
    tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        handshake_inner(reader, writer, hello, auth),
    )
    .await
    .map_err(|_| HandshakeError::Io("the handshake timed out".into()))?
}

async fn handshake_inner(
    reader: &mut Reader,
    writer: &mut Writer,
    hello: Hello,
    auth: ClientAuth<'_>,
) -> Result<Session, HandshakeError> {
    send(writer, &ClientMsg::Hello(hello)).await?;
    let mut msgs = recv(reader).await?.into_iter();
    let challenge = match msgs.next() {
        Some(ServerMsg::Challenge(c)) => c,
        Some(ServerMsg::Refused { code, message }) => {
            return Err(HandshakeError::Refused { code, message });
        }
        other => {
            return Err(HandshakeError::Protocol(format!(
                "expected a challenge, got {other:?}"
            )));
        }
    };
    let request = match auth {
        ClientAuth::Local => AuthRequest::Local,
        ClientAuth::Credential(cred) => {
            if cred.server_id != challenge.server_id {
                return Err(HandshakeError::WrongServer);
            }
            let key = device_key_from_b64(&cred.device_key).ok_or_else(|| {
                HandshakeError::Protocol("the stored device key is unreadable".into())
            })?;
            AuthRequest::Token {
                device_id: cred.device_id.clone(),
                proof: make_proof(
                    &key,
                    ProofPurpose::Token,
                    &challenge.server_id,
                    &challenge.nonce,
                    &cred.token,
                ),
            }
        }
        ClientAuth::Pair {
            code,
            device_name,
            key,
        } => AuthRequest::Pair {
            device_name: device_name.to_owned(),
            public_key: key.verifying_key().to_bytes().to_vec(),
            proof: make_proof(
                key,
                ProofPurpose::Pair,
                &challenge.server_id,
                &challenge.nonce,
                &normalize_code(code),
            ),
        },
    };
    send(writer, &ClientMsg::Auth(request)).await?;
    let mut msgs = recv(reader).await?.into_iter();
    match msgs.next() {
        Some(ServerMsg::Welcome(welcome)) => Ok(Session {
            challenge,
            welcome,
            rest: msgs.collect(),
        }),
        Some(ServerMsg::Refused { code, message }) => {
            Err(HandshakeError::Refused { code, message })
        }
        other => Err(HandshakeError::Protocol(format!(
            "expected welcome, got {other:?}"
        ))),
    }
}

/// Pairing codes are case-insensitive and ignore separators.
pub fn normalize_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}
