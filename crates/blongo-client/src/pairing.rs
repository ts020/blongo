//! Adding an environment: trade a one-time pairing code for a credential
//! (or, for SSH stdio, just check that the server answers).

use blongo_protocol::wire::Hello;

use crate::environments::{Credential, Environment};
use crate::handshake::{ClientAuth, handshake};
use crate::secret::{b64, new_device_key};
use crate::target::{Target, open};

/// Connect to `target` and pair this device with `code`. The returned
/// environment holds the credential; the caller saves it.
pub async fn pair(
    name: &str,
    target: &str,
    code: Option<&str>,
    device_name: &str,
) -> Result<Environment, String> {
    let parsed = Target::parse(target)?;
    let mut link = open(&parsed)
        .await
        .map_err(|e| format!("cannot connect: {e}"))?;
    let hello = Hello::new(
        format!("blongo {} (pairing)", env!("CARGO_PKG_VERSION")),
        None,
    );
    if link.local_auth {
        handshake(&mut link.reader, &mut link.writer, hello, ClientAuth::Local)
            .await
            .map_err(|e| e.to_string())?;
        link.writer.close().await;
        return Ok(Environment {
            name: name.into(),
            target: parsed.to_string(),
            credential: None,
        });
    }
    let code =
        code.ok_or("this target needs a pairing code (run `blongo-serve pair` on the server)")?;
    let key = new_device_key();
    let session = handshake(
        &mut link.reader,
        &mut link.writer,
        hello,
        ClientAuth::Pair {
            code,
            device_name,
            key: &key,
        },
    )
    .await
    .map_err(|e| e.to_string())?;
    link.writer.close().await;
    let issued = session
        .welcome
        .issued
        .ok_or("the server accepted the code but issued no credential")?;
    Ok(Environment {
        name: name.into(),
        target: parsed.to_string(),
        credential: Some(Credential {
            server_id: session.challenge.server_id,
            device_id: issued.device_id,
            token: issued.token,
            device_key: b64(&key.to_bytes()),
        }),
    })
}

/// This machine's name, for the server's device list.
pub fn device_name() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "blongo client".into())
}
