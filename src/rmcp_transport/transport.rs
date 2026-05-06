//! rmcp transport integration for raw ContextVM Nostr transports.

use crate::{
    core::error::Error,
    rmcp_transport::worker::{NostrClientWorker, NostrServerWorker},
    transport::{client::NostrClientTransport, server::NostrServerTransport},
};

impl rmcp::transport::IntoTransport<rmcp::RoleServer, Error, rmcp::transport::worker::WorkerAdapter>
    for NostrServerTransport
{
    /// Convert the raw server transport into rmcp's transport model via the
    /// worker bridge.
    fn into_transport(
        self,
    ) -> impl rmcp::transport::Transport<rmcp::RoleServer, Error = Error> + 'static {
        NostrServerWorker::from_transport(self).into_transport()
    }
}

impl rmcp::transport::IntoTransport<rmcp::RoleClient, Error, rmcp::transport::worker::WorkerAdapter>
    for NostrClientTransport
{
    /// Convert the raw client transport into rmcp's transport model via the
    /// worker bridge.
    fn into_transport(
        self,
    ) -> impl rmcp::transport::Transport<rmcp::RoleClient, Error = Error> + 'static {
        NostrClientWorker::from_transport(self).into_transport()
    }
}
