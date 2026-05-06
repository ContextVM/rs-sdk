//! Direct rmcp adapter entrypoints over raw ContextVM Nostr transports.

use crate::{
    core::error::Error,
    rmcp_transport::worker::{NostrClientWorker, NostrServerWorker},
    transport::{client::NostrClientTransport, server::NostrServerTransport},
};

/// Direct rmcp adapter for [`NostrServerTransport`](src/transport/server/mod.rs:87).
pub struct NostrServerRmcpTransport {
    worker: NostrServerWorker,
}

impl NostrServerTransport {
    /// Convert this raw transport into an rmcp-compatible transport adapter.
    pub fn into_rmcp_transport(self) -> NostrServerRmcpTransport {
        NostrServerRmcpTransport {
            worker: NostrServerWorker::from_transport(self),
        }
    }
}

impl rmcp::transport::IntoTransport<rmcp::RoleServer, Error, rmcp::transport::worker::WorkerAdapter>
    for NostrServerRmcpTransport
{
    fn into_transport(
        self,
    ) -> impl rmcp::transport::Transport<rmcp::RoleServer, Error = Error> + 'static {
        self.worker.into_transport()
    }
}

/// Direct rmcp adapter for [`NostrClientTransport`](src/transport/client/mod.rs:69).
pub struct NostrClientRmcpTransport {
    worker: NostrClientWorker,
}

impl NostrClientTransport {
    /// Convert this raw transport into an rmcp-compatible transport adapter.
    pub fn into_rmcp_transport(self) -> NostrClientRmcpTransport {
        NostrClientRmcpTransport {
            worker: NostrClientWorker::from_transport(self),
        }
    }
}

impl rmcp::transport::IntoTransport<rmcp::RoleClient, Error, rmcp::transport::worker::WorkerAdapter>
    for NostrClientRmcpTransport
{
    fn into_transport(
        self,
    ) -> impl rmcp::transport::Transport<rmcp::RoleClient, Error = Error> + 'static {
        self.worker.into_transport()
    }
}
