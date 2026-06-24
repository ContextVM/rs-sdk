#!/usr/bin/env python3
"""Connect to a remote ContextVM MCP server as a client and call tools/list.

The Python equivalent of examples/proxy.rs. Requires internet access and a
known server pubkey.

    python3 proxy_tools_list.py <server_pubkey_hex> [relay_url ...]

The server pubkey is the 64-char hex public key of an announced ContextVM
server (discover one with discover_servers.py).
"""

import sys

import contextvm_ffi as cvm

DEFAULT_RELAYS = ["wss://relay.damus.io"]


def main() -> int:
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    server_pubkey = sys.argv[1]
    relays = sys.argv[2:] or DEFAULT_RELAYS

    keys = cvm.Keys.generate()
    print(f"Client npub: {cvm.pubkey_hex_to_npub(keys.public_key())}")

    config = cvm.ClientConfig(
        relay_urls=relays,
        server_pubkey=server_pubkey,
        encryption_mode=cvm.EncryptionMode.OPTIONAL,
        gift_wrap_mode=cvm.GiftWrapMode.OPTIONAL,
        is_stateless=False,
        timeout_secs=30,
        discovery_relay_urls=[],
        fallback_operational_relay_urls=[],
    )

    client = cvm.Client(keys, config)

    # MCP initialize handshake (sent on the underlying transport as soon as the
    # client learns the server's relay list / capabilities).
    init = cvm.make_request(
        "1",
        "initialize",
        '{"protocolVersion":"2025-06-18","capabilities":{},'
        '"clientInfo":{"name":"py-example","version":"0.1.0"}}',
    )
    print("Sending initialize...")
    client.send(init)
    init_reply = client.recv_timeout(15)
    print(f"  -> id={init_reply.id}")

    # Query the server's tools.
    print("Sending tools/list...")
    client.send(cvm.make_request("2", "tools/list", None))
    reply = client.recv_timeout(15)
    print(f"  -> id={reply.id}")
    print(reply.payload_json)

    client.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
