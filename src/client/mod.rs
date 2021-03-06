// Copyright (c) 2020 russh-agent developers
//
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or http://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. All files in the project carrying such notice may not be copied,
// modified, or distributed except according to those terms.

//! An asynchronous ssh-agent client implementation
//!
//! # Example
//! ```
//! # use russh_agent::{Result, client::{Client, Message}};
//! # use bytes::Bytes;
//! # use std::{env, time::Duration};
//! # use tokio::{join, net::UnixStream, spawn, time::delay_for};
//! #
//! #[tokio::main]
//! async fn main() -> Result<()> {
//!   // Get the agent socket here
//!   let (actual_agent, sock) = setup_socket().await?;
//!   let (sender, mut receiver, mut client) = Client::new();
//!
//!   if actual_agent {
//!     // This is the client task
//!     let ssh_agent_client = spawn(client.run(sock));
//!
//!     // This is a simulated sender of messages to the client
//!     let mut sender = sender.clone();
//!     let work = spawn(async move {
//!        let _ = sender.send(Message::List).await;
//!        delay_for(Duration::from_millis(100)).await;
//!        let _ = sender.send(Message::Shutdown).await;
//!     });
//!
//!     // This is the receiver of agent responses
//!     let receive = spawn(async move {
//!        loop {
//!            if let Some(msg) = receiver.recv().await {
//!                // Process your msg here!
//!            } else {
//!                break;
//!            }
//!        }
//!     });
//!
//!     let _ = join!(ssh_agent_client, receive, work);
//!   }
//!   Ok(())
//! }
//!
//! async fn setup_socket() -> Result<(bool, UnixStream)> {
//!   Ok(match env::var("SSH_AUTH_SOCK") {
//!     Ok(v) => (true, UnixStream::connect(v).await?),
//!     Err(_) => {
//!         let (up, _down) = UnixStream::pair()?;
//!         (false, up)
//!     }
//!   })
//! }
//! ```

mod constraint;
mod message;

pub use constraint::Constraint;
pub use message::Message;

use crate::{
    error::Result,
    packet::{
        identity::{
            AddIdentity, AddIdentityConstrained, RemoveAll, RemoveIdentity, RequestIdentities,
        },
        lock::Lock,
        sign::SignRequest,
        unlock::Unlock,
        IntoPacket, Packet,
    },
};
use bytes::Bytes;
use getset::Setters;
use slog::{error, trace, Logger};
use slog_try::{try_error, try_trace};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::mpsc::{channel, Receiver, Sender},
};

/// An asynchronous ssh-agent client implementation
#[derive(Debug, Setters)]
pub struct Client {
    /// An optional slog logger
    #[set = "pub"]
    logger: Option<Logger>,
    receiver: Receiver<Message>,
    sender: Sender<Bytes>,
}

impl Client {
    /// Create a new ssh-agent client.
    ///
    /// This returns a sender that should be used to request ssh-agent work
    /// via [Message](crate::client::Message), and a receiver to listen for the results
    /// of those requests in [Bytes](bytes::Bytes).
    #[must_use]
    pub fn new() -> (Sender<Message>, Receiver<Bytes>, Self) {
        let (msg_sender, msg_receiver) = channel(10);
        let (agent_sender, agent_receiver) = channel(10);

        let client = Self {
            logger: None,
            receiver: msg_receiver,
            sender: agent_sender,
        };

        (msg_sender, agent_receiver, client)
    }

    /// Run the agent handler
    ///
    /// # Errors
    ///
    pub async fn run<R>(mut self, mut stream: R) -> Result<()>
    where
        R: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let mut disconnected = false;

        while !disconnected {
            tokio::select! {
                msg_opt = self.receiver.recv() => {
                    if let Some(msg) = msg_opt {
                        try_trace!(self.logger, "Agent <= {}", msg);
                        let (disconnect, pkt_opt) = match msg {
                            Message::Add(kind, key_blob, comment) => (false, Some(AddIdentity::new(kind, key_blob, comment).into_packet()?)),
                            Message::AddConstrained(kind, key_blob, comment, constraints) => (false, Some(AddIdentityConstrained::new(kind, key_blob, comment, constraints).into_packet()?)),
                            Message::Remove(key_blob) => (false, Some(RemoveIdentity::new(key_blob).into_packet()?)),
                            Message::RemoveAll => (false, Some(RemoveAll::default().into_packet()?)),
                            Message::List => (false, Some(RequestIdentities::default().into_packet()?)),
                            Message::Sign(key, data, flags) => (false, Some(SignRequest::new(key, data, flags).into_packet()?)),
                            Message::Lock(passphrase) => (false, Some(Lock::new(passphrase).into_packet()?)),
                            Message::Unlock(passphrase) => (false, Some(Unlock::new(passphrase).into_packet()?)),
                            Message::Shutdown => (true, None),
                        };
                        if disconnect && pkt_opt.is_none() {
                            try_trace!(self.logger, "Shutdown received");
                            disconnected = true;
                        } else if let Some(pkt) = pkt_opt {
                            try_trace!(self.logger, "Agent => {}", pkt.kind());
                            try_trace!(self.logger, "PKT: {}", pkt);
                            pkt.write_packet(&mut stream).await?;
                        } else {
                            disconnected = true;
                        }
                    } else {
                        try_error!(self.logger, "NONE received, sender likely dropped");
                        disconnected = true;
                    }
                }
                packet_res = Packet::read_packet(&mut stream) => {
                    match packet_res {
                        Ok(packet) => {
                            try_trace!(self.logger, "Agent <= {}", packet.kind());
                            if packet.kind().is_response() {
                                self.sender.send(packet.payload().clone()).await?;
                            } else {
                                try_error!(self.logger, "invalid response packet read! {}", packet);
                            }
                        }
                        Err(e) => try_error!(self.logger, "{}", e),
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::Client;
    use crate::{
        client::{Constraint, Message},
        error::Result,
        utils::hexy,
        utils::put_string,
    };
    use bytes::{Buf, Bytes, BytesMut};
    use ed25519_dalek::Keypair;
    use lazy_static::lazy_static;
    use rand::rngs::OsRng;
    use slog::{o, trace, Drain, Logger};
    use slog_async::Async;
    use slog_term::{FullFormat, TermDecorator};
    use slog_try::try_trace;
    use std::{
        env,
        sync::{Arc, Mutex},
        time::Duration,
    };
    use tokio::{
        join,
        net::UnixStream,
        spawn,
        sync::mpsc::{Receiver, Sender},
        time::delay_for,
    };

    lazy_static! {
        static ref PREV_KEYS: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(vec![]));
    }

    async fn setup_socket() -> Result<UnixStream> {
        let path = env::var("SSH_AUTH_SOCK")?;
        Ok(UnixStream::connect(path).await?)
    }

    #[tokio::test]
    async fn client() -> Result<()> {
        if let Ok(sock) = setup_socket().await {
            // Setup the ssh-agent client
            let (sender, receiver, mut client) = Client::new();

            // Setup some logging
            let logger = if let Ok(val) = env::var("RA_LOG_TEST") {
                println!("RA_LOG_TEST: {}", val);
                let decorator = TermDecorator::new().build();
                let term_drain = FullFormat::new(decorator).build().fuse();
                let async_drain = Async::new(term_drain).build().fuse();
                let log = Logger::root(async_drain, o!());
                let _ = client.set_logger(Some(log.clone()));
                Some(log)
            } else {
                None
            };

            // This is the client task
            let client = spawn(client.run(sock));

            // This is a simulated sender of messages
            let send = spawn(send(sender.clone()));

            // This is the receiver of agent responses
            let receive = spawn(receive(receiver, logger));

            // Start 'em all up
            let (_, recv, _) = join!(client, receive, send);

            if let Ok(res) = recv {
                if let Ok(responses) = res {
                    assert_eq!(16, responses.len());
                    assert_eq!(
                        vec![12, 6, 14, 6, 5, 6, 14, 6, 6, 14, 6, 5, 6, 14, 6, 12],
                        responses
                    );
                    Ok(())
                } else {
                    Err("protocol:receive task failed".into())
                }
            } else {
                Err("protocol:receive task failed".into())
            }
        } else {
            Ok(())
        }
    }

    async fn send(mut sender: Sender<Message>) -> Result<()> {
        // List the remaining identites
        list_identities(&mut sender).await?;

        // Add an identity
        if let Ok(pk) = add_identity(&mut sender, None).await {
            // Sign something
            sign_data(&mut sender, &pk).await?;
            // Lock the agent
            lock_agent(&mut sender).await?;
            // Sign something (this should generate a failure at the reciever)
            sign_data(&mut sender, &pk).await?;
            // Unlock the agent
            unlock_agent(&mut sender).await?;
            // Sign something
            sign_data(&mut sender, &pk).await?;
            // Remove the identity
            remove_identity(&mut sender, &pk).await?;
        }

        // Add a constrained identity
        let constraint = Constraint::lifetime(1000);
        if let Ok(pk) = add_identity(&mut sender, Some(constraint.payload().clone())).await {
            // Sign something
            sign_data(&mut sender, &pk).await?;
            // Lock the agent
            lock_agent(&mut sender).await?;
            // Sign something (this should generate a failure at the reciever)
            sign_data(&mut sender, &pk).await?;
            // Unlock the agent
            unlock_agent(&mut sender).await?;
            // Sign something
            sign_data(&mut sender, &pk).await?;
            // Remove the identity
            remove_identity(&mut sender, &pk).await?;
        }

        // List the remaining identites
        list_identities(&mut sender).await?;

        // Shut it down
        delay_for(Duration::from_millis(250)).await;
        let _ = sender.send(Message::Shutdown).await;

        Ok(())
    }

    async fn receive(mut receiver: Receiver<Bytes>, logger: Option<Logger>) -> Result<Vec<u8>> {
        let mut responses = vec![];
        while let Some(mut msg) = receiver.recv().await {
            try_trace!(logger, "Receiver <= Msg");

            if let Some(log) = &logger {
                let _ = hexy("MSG", log, &msg);
            }
            responses.push(msg.get_u8());
        }
        Ok(responses)
    }

    async fn add_identity(
        sender: &mut Sender<Message>,
        const_opt: Option<Bytes>,
    ) -> Result<Vec<u8>> {
        let mut csprng = OsRng {};
        let keypair = Keypair::generate(&mut csprng);
        let key_bytes = keypair.to_bytes();
        let mut add_ident_payload = BytesMut::new();
        let public_key = &key_bytes[32..];
        put_string(&mut add_ident_payload, public_key)?;
        put_string(&mut add_ident_payload, &key_bytes)?;

        let add = if let Some(constraints) = const_opt {
            Message::AddConstrained(
                Bytes::from_static(b"ssh-ed25519"),
                add_ident_payload.freeze(),
                Bytes::from_static(b"test key"),
                constraints,
            )
        } else {
            Message::Add(
                Bytes::from_static(b"ssh-ed25519"),
                add_ident_payload.freeze(),
                Bytes::from_static(b"test key"),
            )
        };
        sender.send(add).await?;
        delay_for(Duration::from_millis(100)).await;
        Ok(public_key.into())
    }

    async fn remove_identity(sender: &mut Sender<Message>, pk: &[u8]) -> Result<()> {
        let mut key_blob = BytesMut::new();
        put_string(&mut key_blob, b"ssh-ed25519")?;
        put_string(&mut key_blob, pk)?;
        let remove = Message::Remove(key_blob.freeze());
        sender.send(remove).await?;
        delay_for(Duration::from_millis(100)).await;
        Ok(())
    }

    async fn sign_data(sender: &mut Sender<Message>, pk: &[u8]) -> Result<()> {
        let mut key_blob = BytesMut::new();
        put_string(&mut key_blob, b"ssh-ed25519")?;
        put_string(&mut key_blob, pk)?;
        let sign = Message::Sign(key_blob.freeze(), Bytes::from_static(b"testing"), 0);
        sender.send(sign).await?;
        delay_for(Duration::from_millis(100)).await;
        Ok(())
    }

    async fn lock_agent(sender: &mut Sender<Message>) -> Result<()> {
        let lock = Message::Lock(Bytes::from_static(b"test"));
        sender.send(lock).await?;
        delay_for(Duration::from_millis(100)).await;
        Ok(())
    }

    async fn unlock_agent(sender: &mut Sender<Message>) -> Result<()> {
        let unlock = Message::Unlock(Bytes::from_static(b"test"));
        sender.send(unlock).await?;
        delay_for(Duration::from_millis(100)).await;
        Ok(())
    }

    async fn list_identities(sender: &mut Sender<Message>) -> Result<()> {
        sender.send(Message::List).await?;
        delay_for(Duration::from_millis(100)).await;
        Ok(())
    }

    #[allow(dead_code)]
    async fn remove_all_identities(sender: &mut Sender<Message>) -> Result<()> {
        let remove_all = Message::RemoveAll;
        sender.send(remove_all).await?;
        delay_for(Duration::from_millis(100)).await;
        Ok(())
    }
}
