//! Dynamic remote actor reference for distributed actors
//!
//! This module provides a DynamicDistributedActorRef that works without requiring
//! the actor type to implement Message<M>, enabling clients to send messages
//! to remote actors without defining the full distributed_actor! macro.

use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwapOption;
use rkyv::{Archive, Serialize as RSerialize};

use crate::actor::ActorId;
use crate::error::SendError;

use super::transport::{RemoteActorLocation, RemoteTransport};
use super::type_hash::HasTypeHash;
use kameo_remote::{GossipError, connection_pool::ConnectionHandle};

/// A reference to a remote distributed actor (dynamic version)
///
/// Unlike DistributedActorRef, this doesn't require A: Message<M> constraints,
/// allowing clients to send messages without defining the actor locally.
///
/// Uses `Arc<ArcSwapOption>` for lock-free atomic connection swapping that is shared
/// across all clones. When one clone refreshes the connection (e.g., after a network
/// failure), ALL clones automatically see the new connection.
#[derive(Debug)]
pub struct DynamicDistributedActorRef<T = Box<super::kameo_transport::KameoTransport>> {
    /// The actor's ID
    pub(crate) actor_id: ActorId,
    /// The actor's location
    pub(crate) location: RemoteActorLocation,
    /// The transport to use for communication
    pub(crate) transport: T,
    /// Cached connection handle with lock-free atomic swapping for auto-refresh.
    /// Arc-wrapped so that clones share the same connection state.
    pub(crate) connection: Arc<ArcSwapOption<ConnectionHandle>>,
}

impl<T: Clone> Clone for DynamicDistributedActorRef<T> {
    fn clone(&self) -> Self {
        // Clone shares the same Arc<ArcSwapOption> so that all clones see connection
        // refreshes. This is important for K8s resilience.
        Self {
            actor_id: self.actor_id,
            location: self.location.clone(),
            transport: self.transport.clone(),
            connection: Arc::clone(&self.connection),
        }
    }
}

impl<T> DynamicDistributedActorRef<T>
where
    T: RemoteTransport,
{
    /// Create a new dynamic distributed actor reference
    pub fn new(actor_id: ActorId, location: RemoteActorLocation, transport: T) -> Self {
        Self {
            actor_id,
            location,
            transport,
            connection: Arc::new(ArcSwapOption::empty()),
        }
    }

    /// Get the actor's ID
    pub fn id(&self) -> ActorId {
        self.actor_id
    }

    /// Get the remote actor location
    pub fn location(&self) -> &RemoteActorLocation {
        &self.location
    }

    /// Send a tell message to the remote actor
    ///
    /// Note: This uses dynamic dispatch - no actor type constraints required
    pub fn tell<M>(&self, message: M) -> DynamicTellRequest<'_, M, T>
    where
        M: HasTypeHash + Send + 'static,
    {
        DynamicTellRequest {
            actor_ref: self,
            message,
            timeout: None,
        }
    }

    /// Send an ask message to the remote actor
    ///
    /// Note: This uses dynamic dispatch - no actor type constraints required
    pub fn ask<M, R>(&self, message: M) -> DynamicAskRequest<'_, M, R, T>
    where
        M: HasTypeHash + Send + 'static,
    {
        DynamicAskRequest {
            actor_ref: self,
            message,
            timeout: None,
            _phantom_reply: PhantomData,
        }
    }
}

// Specialized implementation for KameoTransport to cache connections
impl DynamicDistributedActorRef<Box<super::kameo_transport::KameoTransport>> {
    /// Look up a distributed actor by name (with connection caching for optimal performance)
    pub async fn lookup(
        name: &str,
        transport: Box<super::kameo_transport::KameoTransport>,
    ) -> Result<Option<Self>, Box<dyn std::error::Error + Send + Sync>> {
        if let Some(location) = transport.lookup_actor(name).await? {
            // Try to get the connection for caching
            let connection = match transport.get_connection_for_location(&location).await {
                Ok(conn) => conn,
                Err(_) => {
                    return Ok(None);
                }
            };

            Ok(Some(Self {
                actor_id: location.actor_id,
                location,
                transport,
                connection: Arc::new(ArcSwapOption::new(Some(Arc::new(connection)))),
            }))
        } else {
            Ok(None)
        }
    }

    /// Load the cached connection (lock-free)
    fn load_connection(&self) -> Option<Arc<ConnectionHandle>> {
        self.connection.load_full()
    }

    /// Refresh connection from transport and store atomically
    ///
    /// Returns the new connection on success, or error if connection fails.
    async fn refresh_connection(&self) -> Result<Arc<ConnectionHandle>, SendError> {
        let new_conn = self
            .transport
            .get_connection_for_location(&self.location)
            .await
            .map_err(|_| SendError::ActorStopped)?;
        let arc_conn = Arc::new(new_conn);
        self.connection.store(Some(arc_conn.clone()));
        Ok(arc_conn)
    }

    /// Check if an error is connection-related and should trigger a refresh
    fn is_connection_error(err: &GossipError) -> bool {
        matches!(
            err,
            GossipError::Network(_) | GossipError::PeerNotFound(_) | GossipError::Shutdown
        )
    }
}

/// A pending tell request to a distributed actor (dynamic version)
#[derive(Debug)]
pub struct DynamicTellRequest<'a, M, T = Box<super::kameo_transport::KameoTransport>> {
    actor_ref: &'a DynamicDistributedActorRef<T>,
    message: M,
    timeout: Option<Duration>,
}

impl<'a, M, T> DynamicTellRequest<'a, M, T>
where
    M: HasTypeHash
        + Send
        + 'static
        + Archive
        + for<'b> RSerialize<
            rkyv::rancor::Strategy<
                rkyv::ser::Serializer<
                    rkyv::util::AlignedVec,
                    rkyv::ser::allocator::ArenaHandle<'b>,
                    rkyv::ser::sharing::Share,
                >,
                rkyv::rancor::Error,
            >,
        >,
    T: RemoteTransport,
{
    /// Set a timeout for the tell operation
    pub fn timeout(mut self, duration: Duration) -> Self {
        self.timeout = Some(duration);
        self
    }

    /// Send the tell message
    pub async fn send(self) -> Result<(), SendError> {
        // Serialize the message with rkyv
        let message_ref = &self.message;
        let payload = rkyv::to_bytes::<rkyv::rancor::Error>(message_ref)
            .map_err(|_e| SendError::ActorStopped)?;

        // Get the message type hash
        let type_hash = M::TYPE_HASH.as_u32();

        // Try to use cached connection first if available
        if let Some(conn) = self.actor_ref.connection.load_full() {
            return Self::try_send_payload(&conn, self.actor_ref.actor_id, type_hash, &payload)
                .await
                .map_err(|_| SendError::ActorStopped);
        }

        Err(SendError::MissingConnection)
    }

    /// Internal helper to send pre-serialized payload
    async fn try_send_payload(
        conn: &ConnectionHandle,
        actor_id: ActorId,
        type_hash: u32,
        payload: &[u8],
    ) -> Result<(), GossipError> {
        // Direct binary protocol - no wrapper object, no double serialization!
        // Format: [length:4][type:1][correlation_id:2][reserved:9][actor_id:8][type_hash:4][payload_len:4][payload:N]

        let inner_size = 12 + 16 + payload.len(); // header (type+corr+reserved=12) + actor fields + payload
        let mut message = Vec::with_capacity(4 + inner_size);

        // Length prefix (4 bytes) - this is the size AFTER the length prefix
        message.extend_from_slice(&(inner_size as u32).to_be_bytes());

        // Header: [type:1][correlation_id:2][reserved:9]
        message.push(3u8); // MessageType::ActorTell
        message.extend_from_slice(&0u16.to_be_bytes()); // No correlation for tell
        message.extend_from_slice(&[0u8; 9]); // 9 reserved bytes for 32-byte alignment

        // Actor message: [actor_id:8][type_hash:4][payload_len:4][payload:N]
        message.extend_from_slice(&actor_id.into_u64().to_be_bytes());
        message.extend_from_slice(&type_hash.to_be_bytes());
        message.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        message.extend_from_slice(payload);

        // Try to send using cached connection - direct ring buffer access!
        conn.send_binary_message(&message).await
    }
}

// Specialized implementation for KameoTransport with auto-retry on connection failure
impl<'a, M> DynamicTellRequest<'a, M, Box<super::kameo_transport::KameoTransport>>
where
    M: HasTypeHash
        + Send
        + 'static
        + Archive
        + for<'b> RSerialize<
            rkyv::rancor::Strategy<
                rkyv::ser::Serializer<
                    rkyv::util::AlignedVec,
                    rkyv::ser::allocator::ArenaHandle<'b>,
                    rkyv::ser::sharing::Share,
                >,
                rkyv::rancor::Error,
            >,
        >,
{
    /// Send with automatic connection refresh on failure.
    ///
    /// On connection error, this method will:
    /// 1. Refresh the connection from the transport
    /// 2. Retry the send once with the new connection
    ///
    /// This provides K8s resilience when pods restart with new IPs.
    pub async fn send_with_retry(self) -> Result<(), SendError> {
        // Pre-serialize the message once for potential retry
        let type_hash = M::TYPE_HASH.as_u32();
        let payload = rkyv::to_bytes::<rkyv::rancor::Error>(&self.message)
            .map_err(|_e| SendError::ActorStopped)?;

        let actor_id = self.actor_ref.actor_id;

        // First attempt with cached connection
        if let Some(conn) = self.actor_ref.load_connection() {
            match Self::try_send_payload(&conn, actor_id, type_hash, &payload).await {
                Ok(()) => return Ok(()),
                Err(e) if DynamicDistributedActorRef::<Box<super::kameo_transport::KameoTransport>>::is_connection_error(&e) => {
                    // Connection error - try to refresh and retry
                    tracing::debug!("Connection error on dynamic tell, attempting refresh and retry");
                    if let Ok(new_conn) = self.actor_ref.refresh_connection().await {
                        return Self::try_send_payload(&new_conn, actor_id, type_hash, &payload)
                            .await
                            .map_err(|_| SendError::ActorStopped);
                    }
                }
                Err(_) => return Err(SendError::ActorStopped),
            }
        }

        // No connection - try to establish one
        match self.actor_ref.refresh_connection().await {
            Ok(conn) => Self::try_send_payload(&conn, actor_id, type_hash, &payload)
                .await
                .map_err(|_| SendError::ActorStopped),
            Err(e) => Err(e),
        }
    }
}

/// A pending ask request to a distributed actor (dynamic version)
#[derive(Debug)]
pub struct DynamicAskRequest<'a, M, R, T = Box<super::kameo_transport::KameoTransport>> {
    actor_ref: &'a DynamicDistributedActorRef<T>,
    message: M,
    timeout: Option<Duration>,
    _phantom_reply: PhantomData<R>,
}

impl<'a, M, R, T> DynamicAskRequest<'a, M, R, T>
where
    M: HasTypeHash
        + Send
        + 'static
        + Archive
        + for<'b> RSerialize<
            rkyv::rancor::Strategy<
                rkyv::ser::Serializer<
                    rkyv::util::AlignedVec,
                    rkyv::ser::allocator::ArenaHandle<'b>,
                    rkyv::ser::sharing::Share,
                >,
                rkyv::rancor::Error,
            >,
        >,
    R: Archive
        + for<'b> RSerialize<
            rkyv::rancor::Strategy<
                rkyv::ser::Serializer<
                    rkyv::util::AlignedVec,
                    rkyv::ser::allocator::ArenaHandle<'b>,
                    rkyv::ser::sharing::Share,
                >,
                rkyv::rancor::Error,
            >,
        >,
    <R as Archive>::Archived: for<'b> rkyv::Deserialize<R, rkyv::rancor::Strategy<rkyv::de::Pool, rkyv::rancor::Error>>
        + for<'b> rkyv::bytecheck::CheckBytes<
            rkyv::rancor::Strategy<
                rkyv::validation::Validator<
                    rkyv::validation::archive::ArchiveValidator<'b>,
                    rkyv::validation::shared::SharedValidator,
                >,
                rkyv::rancor::Error,
            >,
        >,
    T: RemoteTransport,
{
    /// Set a timeout for the ask operation
    pub fn timeout(mut self, duration: Duration) -> Self {
        self.timeout = Some(duration);
        self
    }

    /// Send the ask message and wait for reply
    pub async fn send(self) -> Result<R, SendError> {
        // Get the raw bytes
        let reply_bytes = self.send_raw().await?;

        // Deserialize the reply using rkyv
        let reply = match rkyv::from_bytes::<R, rkyv::rancor::Error>(&reply_bytes) {
            Ok(r) => r,
            Err(_e) => {
                return Err(SendError::ActorStopped);
            }
        };

        Ok(reply)
    }

    /// Send the ask message and wait for reply - returns raw bytes for zero-copy access
    pub async fn send_raw(self) -> Result<bytes::Bytes, SendError> {
        // Serialize the message with rkyv
        let message_ref = &self.message;
        let payload = rkyv::to_bytes::<rkyv::rancor::Error>(message_ref)
            .map_err(|_e| SendError::ActorStopped)?;

        // Get the message type hash
        let type_hash = M::TYPE_HASH.as_u32();

        // Default timeout if not specified
        let default_timeout = Duration::from_secs(2);
        let timeout = self.timeout.unwrap_or(default_timeout);

        let reply_bytes = if let Some(conn) = self.actor_ref.connection.load_full() {
            Self::try_send_ask(&conn, self.actor_ref.actor_id, type_hash, &payload, timeout).await?
        } else {
            return Err(SendError::MissingConnection);
        };

        Ok(reply_bytes)
    }

    /// Internal helper to send pre-serialized ask payload
    async fn try_send_ask(
        conn: &ConnectionHandle,
        actor_id: ActorId,
        type_hash: u32,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<bytes::Bytes, SendError> {
        let threshold = conn.streaming_threshold();
        if payload.len() > threshold {
            // For streaming, scale timeout based on payload size
            let streaming_timeout = {
                let payload_mb = payload.len() as u64 / (1024 * 1024);
                Duration::from_secs(30 + payload_mb)
            };

            let reply = conn
                .ask_streaming_bytes(
                    bytes::Bytes::copy_from_slice(payload),
                    type_hash,
                    actor_id.into_u64(),
                    streaming_timeout,
                )
                .await
                .map_err(|e| match e {
                    GossipError::Timeout => SendError::Timeout(None),
                    _ => SendError::ActorStopped,
                })?;
            return Ok(reply);
        }

        // Small message - use regular ask
        let actor_message = kameo_remote::registry::RegistryMessage::ActorMessage {
            actor_id: actor_id.into_u64().to_string(),
            type_hash,
            payload: payload.to_vec(),
            correlation_id: None, // conn.ask() will handle correlation ID
        };

        // Serialize using rkyv
        let message_bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&actor_message)
            .map_err(|_e| SendError::ActorStopped)?;

        // Try to send using cached connection
        let reply = match tokio::time::timeout(timeout, conn.ask(&message_bytes)).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(_e)) => return Err(SendError::ActorStopped),
            Err(_) => return Err(SendError::Timeout(None)),
        };

        Ok(reply)
    }
}

// Specialized implementation for KameoTransport with auto-retry on connection failure
impl<'a, M, R> DynamicAskRequest<'a, M, R, Box<super::kameo_transport::KameoTransport>>
where
    M: HasTypeHash
        + Send
        + 'static
        + Archive
        + for<'b> RSerialize<
            rkyv::rancor::Strategy<
                rkyv::ser::Serializer<
                    rkyv::util::AlignedVec,
                    rkyv::ser::allocator::ArenaHandle<'b>,
                    rkyv::ser::sharing::Share,
                >,
                rkyv::rancor::Error,
            >,
        >,
    R: Archive
        + for<'b> RSerialize<
            rkyv::rancor::Strategy<
                rkyv::ser::Serializer<
                    rkyv::util::AlignedVec,
                    rkyv::ser::allocator::ArenaHandle<'b>,
                    rkyv::ser::sharing::Share,
                >,
                rkyv::rancor::Error,
            >,
        >,
    <R as Archive>::Archived: for<'b> rkyv::Deserialize<R, rkyv::rancor::Strategy<rkyv::de::Pool, rkyv::rancor::Error>>
        + for<'b> rkyv::bytecheck::CheckBytes<
            rkyv::rancor::Strategy<
                rkyv::validation::Validator<
                    rkyv::validation::archive::ArchiveValidator<'b>,
                    rkyv::validation::shared::SharedValidator,
                >,
                rkyv::rancor::Error,
            >,
        >,
{
    /// Send with automatic connection refresh on failure and deserialize reply.
    ///
    /// # Idempotency Warning
    ///
    /// This method retries ONCE on connection failure. If your message handler
    /// is NOT idempotent (e.g., creates resources, increments counters), use
    /// `send()` instead and handle reconnection manually.
    pub async fn send_with_retry(self) -> Result<R, SendError> {
        let reply_bytes = self.send_raw_with_retry().await?;

        // Deserialize the reply using rkyv
        let reply = match rkyv::from_bytes::<R, rkyv::rancor::Error>(&reply_bytes) {
            Ok(r) => r,
            Err(_e) => {
                return Err(SendError::ActorStopped);
            }
        };

        Ok(reply)
    }

    /// Send with automatic connection refresh on failure - returns raw bytes.
    ///
    /// Same idempotency warning as `send_with_retry`.
    pub async fn send_raw_with_retry(self) -> Result<bytes::Bytes, SendError> {
        // Pre-serialize the message once for potential retry
        let type_hash = M::TYPE_HASH.as_u32();
        let payload = rkyv::to_bytes::<rkyv::rancor::Error>(&self.message)
            .map_err(|_e| SendError::ActorStopped)?;

        // Default timeout if not specified
        let default_timeout = Duration::from_secs(2);
        let timeout = self.timeout.unwrap_or(default_timeout);
        let actor_id = self.actor_ref.actor_id;

        // First attempt with cached connection
        if let Some(conn) = self.actor_ref.load_connection() {
            match Self::try_send_ask(&conn, actor_id, type_hash, &payload, timeout).await {
                Ok(reply) => return Ok(reply),
                Err(e) if Self::is_connection_error(&e) => {
                    // Connection error - try to refresh and retry
                    tracing::debug!("Connection error on dynamic ask, attempting refresh and retry");
                    if let Ok(new_conn) = self.actor_ref.refresh_connection().await {
                        return Self::try_send_ask(&new_conn, actor_id, type_hash, &payload, timeout).await;
                    }
                }
                Err(e) => return Err(e),
            }
        }

        // No connection - try to establish one
        match self.actor_ref.refresh_connection().await {
            Ok(conn) => Self::try_send_ask(&conn, actor_id, type_hash, &payload, timeout).await,
            Err(e) => Err(e),
        }
    }

    /// Check if an error is connection-related and should trigger a refresh
    fn is_connection_error(err: &SendError) -> bool {
        matches!(err, SendError::ActorStopped | SendError::MissingConnection)
    }
}
