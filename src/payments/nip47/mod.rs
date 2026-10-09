//! NIP-47 (Nostr Wallet Connect) client infrastructure for the CEP-8 Lightning rail.
//!
//! Four pieces: [`parse_nwc_uri`] for connection strings, permissive wire
//! [`types`] that will not turn a paid invoice into a parse failure, a typed
//! [`NwcError`] that says whether a failed call may still have reached the
//! wallet, and [`NwcClient`], a request/response and notification client.
//!
//! The client owns a relay pool built from the connection's own relays
//! ([`NwcClient::connect`]); [`NwcClient::with_pool`] injects one for tests.
//!
//! Behind the off-by-default `nwc` feature. It enables `nostr-sdk/nip47`,
//! which pulls in NIP-04 and therefore adds `aes`, `cbc` and `cipher` to the
//! dependency tree.

pub mod client;
pub mod error;
pub mod types;
pub mod uri;

#[cfg(feature = "test-utils")]
pub mod mock_wallet;

pub use client::{NwcClient, NwcClientOptions, DEFAULT_RESPONSE_TIMEOUT};
pub use error::NwcError;
pub use types::{
    sats_to_msats, NwcErrorBody, NwcInvoiceResult, NwcNotificationPayload, NwcPayInvoiceResult,
    NwcResponseEnvelope, NOTIFICATION_PAYMENT_RECEIVED,
};
pub use uri::{parse_nwc_uri, NwcConnection};

#[cfg(feature = "test-utils")]
pub use mock_wallet::{MockWallet, MockWalletQuirks, WalletBehavior};
