# hopchat

Offline terminal chat built on the [Offline Protocol SDK](https://github.com/Offline-Protocol/offline-protocol-sdk) and [ratatui](https://ratatui.rs).
No server and no internet: peers on the same network find each other, and every conversation is end-to-end encrypted with MLS (RFC 9420).

```
cargo run -- --name alice          # on one machine (or terminal)
cargo run -- --name bob            # on another
```

Two terminals on one machine work too. Each name gets its own identity under `~/.hopchat/<name>`.

| Key | Action |
| --- | --- |
| Enter | send |
| Tab / ↑↓ | switch peer |
| Ctrl-E | show/hide the protocol event log |
| Esc / Ctrl-C | quit |

Delivery ticks: `◷` queued, `✓` on the wire, `✓✓ 42ms` acknowledged by the recipient, `✗` failed.

## Headless modes

```
hopchat --name bot --echo             # answers every message: something to test against
hopchat --name alice --say "hello"    # sends one message to the first peer, prints what happens
```

## How it works

The SDK does no I/O itself, so hopchat supplies the "platform bridge" for its peer-stream (`wifi_direct`) transport:

- **Discovery** (`src/net.rs`): a UDP multicast beacon on `239.255.42.99:47999` announces `address, tcp port, nick` every 2 s. The lower address dials.
- **Streams**: one TCP connection per peer. The first frame in each direction is the SDK's signed identity assertion, verified with `verify_identity_assertion`. A peer is announced only under the address that assertion proves, never under what the beacon claims.
- **Engine** (`src/engine.rs`): owns `OfflineProtocol` on one thread. It feeds frames in, drains `get_next_message()` out to the right stream, and forwards protocol events to the UI. Discovering a neighbor triggers the automatic key exchange.
- **Storage**: the SDK's file stores, sealed with a key in `~/.hopchat/<name>/store.key`.

The nick in the beacon is unauthenticated and only for display; the `off1…` address is the identity.
