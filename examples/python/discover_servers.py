#!/usr/bin/env python3
"""Discover announced ContextVM MCP servers and their published tools.

The Python equivalent of examples/discovery.rs. Requires internet access to
Nostr relays.

    python3 discover_servers.py [relay_url ...]

Defaults to wss://relay.damus.io and wss://nos.lol.
"""

import sys

import contextvm_ffi as cvm

DEFAULT_RELAYS = ["wss://relay.damus.io", "wss://nos.lol"]


def main() -> int:
    relays = sys.argv[1:] or DEFAULT_RELAYS

    keys = cvm.Keys.generate()
    pool = cvm.RelayPool(keys)
    print(f"Connecting to {len(relays)} relay(s)...")
    pool.connect(relays)

    discovery = cvm.Discovery()

    print("Discovering announced MCP servers...\n")
    servers = discovery.discover_servers(pool, relays)
    if not servers:
        print("No servers found. Try other relays, or wait a moment and re-run.")
        pool.disconnect()
        return 0

    for s in servers:
        name = s.name or "unnamed"
        print(f"Server: {name} (npub: {cvm.pubkey_hex_to_npub(s.pubkey)[:24]}...)")
        if s.about:
            print(f"  about: {s.about}")

        # Tools published by this specific provider.
        tools = discovery.discover_tools(pool, s.pubkey, name, relays)
        if tools:
            print(f"  tools ({len(tools)}):")
            for t in tools:
                desc = t.description.replace("\n", " ")[:80]
                print(f"    - {t.tool_name}: {desc}")
        print()

    # One-shot aggregate discovery across all providers on the relays.
    print("All discoverable tools across providers:")
    all_tools = discovery.discover_all_tools(pool, relays)
    for t in all_tools:
        provider = t.provider_display_name or t.provider_name or t.provider_pubkey[:8]
        desc = t.description.replace("\n", " ")[:60]
        print(f"  [{provider}] {t.tool_name}: {desc}")
    print(f"\nTotal: {len(all_tools)} tool(s) from {len(servers)} server(s).")

    pool.disconnect()
    return 0


if __name__ == "__main__":
    sys.exit(main())
