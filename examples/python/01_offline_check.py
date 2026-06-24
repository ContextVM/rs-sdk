#!/usr/bin/env python3
"""Offline sanity check for the contextvm_ffi Python binding.

Run this first to confirm the binding and native library are installed and
matched. It exercises only the non-network APIs, so it needs no internet:

    python3 01_offline_check.py

If this fails with `InternalError("UniFFI ... mismatch")`, the generated
`contextvm_ffi.py` and the native library were taken from different releases
(see README.md).
"""

import contextvm_ffi as cvm


def main() -> None:
    print(f"contextvm-ffi version: {cvm.version()}")

    # --- key management --------------------------------------------------
    keys = cvm.Keys.generate()
    pubkey = keys.public_key()  # 64-char hex
    secret = keys.secret_key()  # 64-char hex
    npub = cvm.pubkey_hex_to_npub(pubkey)  # bech32 (npub1...)

    print(f"public key (hex):  {pubkey}")
    print(f"public key (npub): {npub}")
    print(f"secret key (hex):  {secret[:16]}...")

    # Round-trip: reconstruct the keypair from its secret hex.
    restored = cvm.Keys.from_secret_key(secret)
    assert restored.public_key() == pubkey, "key reconstruction failed"
    print("key reconstruction from secret: OK")

    # --- JSON-RPC message helpers ---------------------------------------
    # These build the JSON strings accepted by Client.send() /
    # Server.send_response() / Proxy.send().
    print("\nbuilt request:      ", cvm.make_request("1", "tools/list", None))
    print("built tool call:    ", cvm.make_request("2", "tools/call", '{"name": "echo"}'))
    print("built notification: ", cvm.make_notification("notifications/initialized", None))
    print("built response:     ", cvm.make_response("1", '{"tools": []}'))

    # --- config records (construction only; no network) ------------------
    client_config = cvm.ClientConfig(
        relay_urls=["wss://relay.damus.io"],
        server_pubkey=pubkey,
        encryption_mode=cvm.EncryptionMode.OPTIONAL,
        gift_wrap_mode=cvm.GiftWrapMode.OPTIONAL,
        is_stateless=False,
        timeout_secs=30,
        discovery_relay_urls=[],
        fallback_operational_relay_urls=[],
    )
    print(
        "\nclient config: server=%s... encryption=%s"
        % (client_config.server_pubkey[:16], client_config.encryption_mode)
    )

    print("\nAll offline checks passed. The binding is installed correctly.")


if __name__ == "__main__":
    main()
