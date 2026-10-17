//! An RSA key in the agent signs with SHA-2 whenever the server can take
//! it, and with SHA-1 `ssh-rsa` only when the server offers nothing else.
//!
//! A real server in this process, with the signature algorithms it names in
//! `server-sig-algs` set per case, and an agent of the test's own that signs
//! as asked and records which hash each request asked for. Its own process,
//! as it points `BERTH_KNOWN_HOSTS` at a file of its own.

use std::sync::{Arc, Mutex};

use russh::keys::ssh_encoding::{Decode, Encode};
use russh::keys::ssh_key::private::KeypairData;
use russh::keys::{Algorithm, HashAlg, PrivateKey};
use russh::server::{Auth, Handler};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use berth_core::known_hosts::TrustPolicy;
use berth_core::transport::Connection;

/// The agent protocol's flags on a sign request (draft-miller-ssh-agent).
const SHA2_256: u32 = 2;
const SHA2_512: u32 = 4;

#[derive(Clone)]
struct Server {
    user_key: russh::keys::PublicKey,
}

impl Handler for Server {
    type Error = russh::Error;

    async fn auth_publickey_offered(
        &mut self,
        _user: &str,
        key: &russh::keys::PublicKey,
    ) -> Result<Auth, Self::Error> {
        Ok(if key.key_data() == self.user_key.key_data() {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn auth_publickey(
        &mut self,
        _user: &str,
        key: &russh::keys::PublicKey,
    ) -> Result<Auth, Self::Error> {
        self.auth_publickey_offered(_user, key).await
    }
}

/// An agent with one key, which answers the two requests a client makes and
/// notes the flags of every sign request.
async fn agent(
    listener: tokio::net::UnixListener,
    key: Arc<PrivateKey>,
    flags: Arc<Mutex<Vec<u32>>>,
) {
    while let Ok((mut stream, _)) = listener.accept().await {
        let key = key.clone();
        let flags = flags.clone();
        tokio::spawn(async move {
            loop {
                let mut len = [0u8; 4];
                if stream.read_exact(&mut len).await.is_err() {
                    return;
                }
                let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
                if stream.read_exact(&mut body).await.is_err() {
                    return;
                }
                let mut reply = Vec::new();
                match body.first() {
                    // REQUEST_IDENTITIES: the one key.
                    Some(11) => {
                        reply.push(12);
                        1u32.encode(&mut reply).unwrap();
                        key.public_key()
                            .key_data()
                            .encode_prefixed(&mut reply)
                            .unwrap();
                        "test".encode(&mut reply).unwrap();
                    }
                    // SIGN_REQUEST: key, data, flags.
                    Some(13) => {
                        let mut r = &body[1..];
                        let _blob = Vec::<u8>::decode(&mut r).unwrap();
                        let data = Vec::<u8>::decode(&mut r).unwrap();
                        let flag = u32::decode(&mut r).unwrap();
                        flags.lock().unwrap().push(flag);
                        let hash = if flag & SHA2_512 != 0 {
                            Some(HashAlg::Sha512)
                        } else if flag & SHA2_256 != 0 {
                            Some(HashAlg::Sha256)
                        } else {
                            None
                        };
                        let KeypairData::Rsa(rsa) = key.key_data() else {
                            panic!("an RSA key");
                        };
                        let signature: russh::keys::ssh_key::Signature =
                            russh::keys::signature::Signer::try_sign(&(rsa, hash), &data).unwrap();
                        let mut encoded = Vec::new();
                        signature.encode(&mut encoded).unwrap();
                        reply.push(14);
                        encoded.encode(&mut reply).unwrap();
                    }
                    _ => reply.push(5),
                }
                let mut framed = (reply.len() as u32).to_be_bytes().to_vec();
                framed.extend_from_slice(&reply);
                if stream.write_all(&framed).await.is_err() {
                    return;
                }
            }
        });
    }
}

/// Connect to a server naming `offered` as its signature algorithms, with
/// the RSA key in the agent, and say which hashes the agent was asked for.
async fn connect_offering(
    dir: &std::path::Path,
    user_key: &Arc<PrivateKey>,
    offered: Vec<Algorithm>,
) -> (bool, Vec<u32>) {
    let host_key =
        PrivateKey::random(&mut russh::keys::key::safe_rng(), Algorithm::Ed25519).unwrap();
    let mut preferred = russh::Preferred::default();
    // The host key's algorithm leads: the list is also what the host key is
    // agreed from.
    let mut keys = vec![Algorithm::Ed25519];
    keys.extend(offered);
    preferred.key = keys.into();
    let config = Arc::new(russh::server::Config {
        keys: vec![host_key.clone()],
        preferred,
        ..Default::default()
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = Server {
        user_key: user_key.public_key().clone(),
    };
    tokio::spawn(async move {
        if let Ok((socket, _)) = listener.accept().await
            && let Ok(session) = russh::server::run_stream(config, socket, server).await
        {
            let _ = session.await;
        }
    });

    let known = dir.join(format!("known_hosts-{port}"));
    std::fs::write(
        &known,
        format!(
            "[127.0.0.1]:{port} {}\n",
            host_key.public_key().to_openssh().unwrap()
        ),
    )
    .unwrap();
    // This file's only test sets it, before each connect, and nothing else
    // in this process reads it meanwhile.
    unsafe { std::env::set_var(berth_core::env::KNOWN_HOSTS, &known) };

    let socket = dir.join(format!("agent-{port}.sock"));
    let flags = Arc::new(Mutex::new(Vec::new()));
    tokio::spawn(agent(
        tokio::net::UnixListener::bind(&socket).unwrap(),
        user_key.clone(),
        flags.clone(),
    ));

    let mut host = berth_core::Host::new("rsa-test", "127.0.0.1");
    host.port = port;
    host.user = Some("tester".into());
    host.identity_agent = Some(socket.to_string_lossy().to_string());
    let connected = Connection::connect(&host, TrustPolicy::KnownHostsOnly)
        .await
        .is_ok();
    let asked = flags.lock().unwrap().clone();
    (connected, asked)
}

#[tokio::test]
async fn an_rsa_key_signs_with_sha2_unless_the_server_takes_only_sha1() {
    let dir = tempfile::tempdir().unwrap();
    let user_key = Arc::new(
        PrivateKey::random(
            &mut russh::keys::key::safe_rng(),
            Algorithm::Rsa { hash: None },
        )
        .unwrap(),
    );
    let rsa = |hash| Algorithm::Rsa { hash };

    // A server that names rsa-sha2-512: signed with SHA-512.
    let (ok, asked) = connect_offering(
        dir.path(),
        &user_key,
        vec![rsa(Some(HashAlg::Sha512)), rsa(Some(HashAlg::Sha256))],
    )
    .await;
    assert!(ok, "rsa-sha2-512 offered");
    assert!(
        !asked.is_empty() && asked.iter().all(|f| *f == SHA2_512),
        "{asked:?}"
    );

    // One that names only rsa-sha2-256: SHA-256, never SHA-1.
    let (ok, asked) =
        connect_offering(dir.path(), &user_key, vec![rsa(Some(HashAlg::Sha256))]).await;
    assert!(ok, "rsa-sha2-256 offered");
    assert!(
        !asked.is_empty() && asked.iter().all(|f| *f == SHA2_256),
        "{asked:?}"
    );

    // One that names no RSA signature at all, as a server that says nothing
    // does: SHA-512 still, never SHA-1.
    let (ok, asked) = connect_offering(dir.path(), &user_key, Vec::new()).await;
    assert!(ok, "nothing named for RSA");
    assert!(
        !asked.is_empty() && asked.iter().all(|f| *f == SHA2_512),
        "{asked:?}"
    );

    // Only when the server names ssh-rsa and nothing else is SHA-1 used.
    let (ok, asked) = connect_offering(dir.path(), &user_key, vec![rsa(None)]).await;
    assert!(ok, "only ssh-rsa offered");
    assert!(
        !asked.is_empty() && asked.iter().all(|f| *f == 0),
        "{asked:?}"
    );
}
