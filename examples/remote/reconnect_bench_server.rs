//! Remote reconnect benchmark server (kameo).
//!
//! Terminal 1:
//!   cargo run --release --example reconnect_bench_server --features remote 127.0.0.1:29200
//!
//! Terminal 2:
//!   cargo run --release --example reconnect_bench_client --features remote /tmp/kameo_tls/reconnect_bench_server.pub 127.0.0.1:29200
//!
//! Manual test:
//! - Start both.
//! - Kill the server with Ctrl+C, then restart it.
//! - The client should log `on_link_died`, reconnect, and continue progress.

use kameo::{distributed_actor, Actor};
use kameo::actor::{ActorId, ActorRef, WeakActorRef};
use kameo::error::ActorStopReason;
use kameo::remote::v2_bootstrap;
use kameo_remote::{SecretKey, tls};
use std::env;
use std::fs;
use std::path::Path;
use std::time::Duration;
use thiserror::Error;

const SERVER_NAME: &str = "reconnect_bench_server";

struct EchoActor;

#[derive(kameo::RemoteMessage, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug)]
struct Ping(u64);

impl kameo::message::Message<Ping> for EchoActor {
    type Reply = u64;

    async fn handle(&mut self, msg: Ping, _ctx: &mut kameo::message::Context<Self, Self::Reply>) {
        kameo::reply::reply(msg.0);
    }
}

distributed_actor! {
    EchoActor {
        Ping,
    }
}

impl Actor for EchoActor {
    type Args = ();
    type Error = ServerError;

    async fn on_start(_args: Self::Args, _actor_ref: ActorRef<Self>) -> Result<Self, Self::Error> {
        Ok(Self)
    }

    async fn on_link_died(
        &mut self,
        actor_ref: WeakActorRef<Self>,
        id: ActorId,
        reason: ActorStopReason,
    ) -> Result<std::ops::ControlFlow<ActorStopReason>, Self::Error> {
        tracing::info!(
            local_actor_id = ?actor_ref.id(),
            remote_actor_id = ?id,
            reason = ?reason,
            "[SERVER] on_link_died"
        );
        Ok(std::ops::ControlFlow::Continue(()))
    }

    async fn on_link_established(
        &mut self,
        actor_ref: WeakActorRef<Self>,
        id: ActorId,
    ) -> Result<std::ops::ControlFlow<ActorStopReason>, Self::Error> {
        tracing::info!(
            local_actor_id = ?actor_ref.id(),
            remote_actor_id = ?id,
            "[SERVER] on_link_established"
        );
        Ok(std::ops::ControlFlow::Continue(()))
    }
}

#[tokio::main]
async fn main() -> Result<(), ServerError> {
    tls::ensure_crypto_provider();
    tracing_subscriber::fmt().init();

    let args: Vec<String> = env::args().collect();
    let bind_addr = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "127.0.0.1:29200".to_string());
    let bind_addr = bind_addr
        .parse::<std::net::SocketAddr>()
        .map_err(|e| ServerError::ParseAddr(e.to_string()))?;

    let key_path = "/tmp/kameo_tls/reconnect_bench_server.key";
    let pub_path = "/tmp/kameo_tls/reconnect_bench_server.pub";
    let secret_key = load_or_generate_key(key_path, pub_path)?;

    let _transport = v2_bootstrap::bootstrap_with_keypair(bind_addr, secret_key.to_keypair()).await?;

    let server_ref: ActorRef<EchoActor> = <EchoActor as Actor>::spawn(());
    kameo::remote::distributed_actor_ref::set_global_transport(_transport);
    // NOTE: v2_bootstrap already sets the global transport internally in normal usage.
    // This explicit call is only here to keep this example isolated if the bootstrap API changes.

    // Register actor (sync so client can reconnect without sleeps).
    kameo::remote::v2_bootstrap::get_distributed_handler();
    // Register via transport directly.
    // In this repo's current API, registration is exposed on the transport instance returned by bootstrap.
    // The transport is already set globally and used by lookup(), but we still need to register.
    let transport = kameo::remote::distributed_actor_ref::GLOBAL_TRANSPORT
        .lock()
        .unwrap()
        .clone()
        .expect("global transport not set");
    transport
        .register_distributed_actor_sync(SERVER_NAME.to_string(), &server_ref, Duration::from_secs(2))
        .await
        .map_err(|e| ServerError::Transport(e.to_string()))?;

    println!("✅ Listening on: {}", bind_addr);
    println!("✅ Server actor registered as '{}'", SERVER_NAME);
    println!("Public key: {}", pub_path);
    println!("Press Ctrl+C to stop (hard exit)\n");

    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        println!("🛑 [SERVER] Ctrl+C received, exiting immediately");
        std::process::exit(1);
    });

    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

fn load_or_generate_key(key_path: &str, pub_path: &str) -> Result<SecretKey, ServerError> {
    let key_path = Path::new(key_path);

    if let Some(parent) = key_path.parent() {
        fs::create_dir_all(parent)?;
    }

    if key_path.exists() {
        let key_hex = fs::read_to_string(key_path)?;
        let key_bytes = hex::decode(key_hex.trim())?;

        if key_bytes.len() != 32 {
            return Err(ServerError::InvalidKeyLength(key_bytes.len()));
        }

        let mut arr = [0u8; 32];
        arr.copy_from_slice(&key_bytes);
        let secret = SecretKey::from_bytes(&arr)?;
        fs::write(pub_path, hex::encode(secret.public_key_bytes()))?;
        Ok(secret)
    } else {
        let secret = SecretKey::generate();
        fs::write(key_path, hex::encode(secret.to_bytes()))?;
        fs::write(pub_path, hex::encode(secret.public_key_bytes()))?;
        Ok(secret)
    }
}

#[derive(Debug, Error)]
enum ServerError {
    #[error("invalid key length: expected 32, got {0}")]
    InvalidKeyLength(usize),
    #[error("invalid bind address: {0}")]
    ParseAddr(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Hex(#[from] hex::FromHexError),
    #[error(transparent)]
    Gossip(#[from] kameo_remote::GossipError),
    #[error("transport error: {0}")]
    Transport(String),
}

