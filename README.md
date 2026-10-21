# hopchat

Offline terminal chat built on the [Offline Protocol SDK](https://github.com/Offline-Protocol/offline-protocol-sdk) and [ratatui](https://ratatui.rs).
No server and no internet: peers on the same network find each other, and every conversation is end-to-end encrypted with MLS (RFC 9420).

```
cargo run -- --name alice          # on one machine (or terminal)
cargo run -- --name bob            # on another
```

Two terminals on one machine work too. Each name gets its own identity under `~/.hopchat/<name>`.

## What you get

- **Files**: press Ctrl-O to browse, drag files onto the window, or type `/file PATH`. Each transfer shows a live progress bar with rate and time left. Transfers are chunked and MLS-encrypted, up to 100 MB each. Received files are saved to `~/Downloads/hopchat` (override with `--downloads`), never overwriting anything and never escaping that folder.
- **Delivery you can see**: `◷` queued, `✓` on the wire, `✓✓` delivered, blue `✓✓ read` once they've seen it, `✗` failed (press `R` to retry).
- **Replies, typing indicators, unread badges**, plus a bell and a window-title count when a message arrives while you're elsewhere.
- **History that survives restarts**: each conversation is sealed with XChaCha20-Poly1305 under your profile's key in `~/.hopchat/<name>/history`. Messages written while a peer is away are delivered when they come back.
- **A real composer**: multi-line input (Shift/Alt-Enter or Ctrl-J), word motions, slash commands with completion.

| Key | Action |
| --- | --- |
| Enter | send (with any attached files) |
| Ctrl-O | attach files |
| Tab / Shift-Tab, Alt-1…9 | switch conversation |
| ↑ on an empty line | select a message: `r` reply, `o` open, `f` show in folder, `y` copy, `R` retry |
| PgUp / PgDn, mouse wheel | scroll |
| Ctrl-E | protocol event log |
| F1 | help |
| Ctrl-C / Ctrl-Q | quit |

Commands: `/file`, `/downloads`, `/info`, `/me`, `/clear`, `/log`, `/help`, `/quit`, and for groups `/group`, `/invite`, `/members`, `/leave`, `/home`.

## Groups

`/group house` starts a group; open it and `/invite bob` adds anyone nearby. Groups are MLS groups (RFC 9420) carried over the same peer-to-peer links, so they need no server either. Files still go one-to-one.

## Home: talk to your devices

Anyone in a group can talk to the smart devices on the machine that hosts them. Mention `@home`:

```
@home dim the lights to 20% and put on netflix
@home make the lamp purple
@home turn the tv down a bit
@home is the light on?
```

A local model (via [Ollama](https://ollama.com)) turns the request into device actions, and home answers in the group with a 🏠. Nothing leaves your network.

Setup on the host:

1. `ollama serve`, with a tool-calling model pulled (`ollama pull qwen2.5:7b` is the default; `/home model NAME` picks another).
2. The Wiz bulb is found by itself on first start. If it isn't, use `/home bulb 192.168.1.10`.
3. `/home tv 192.168.1.30` pairs the LG TV. Accept the prompt on the TV within a minute. For "turn the TV on" to work, enable *Turn on via Wi-Fi* (or *Mobile TV On*) in the TV's settings.
4. In each group that should have access: `/home on` (and `/home off` to revoke it).

Being in the group is the permission: everyone MLS has admitted to a group where home is on can use the devices, and nobody else can. Home only answers while the host's hopchat is running. Settings are kept in `~/.hopchat/<name>/home.json`. That file holds the TV's client key and its pinned certificate: the TV's certificate is self-signed, so hopchat trusts the one it sees at pairing and refuses a different one later.

Devices: Wiz bulbs (local UDP, port 38899) and LG webOS TVs (the WebSocket API on port 3001 over TLS, or 3000 on older sets). The code is in `src/home/`.

## Headless modes

```
hopchat --name bot --echo                         # answers every message and file: something to test against
hopchat --name alice --say "hello" --to bot       # sends one message, prints what happens
hopchat --name alice --file ./notes.pdf --to bot  # sends one file
hopchat --name alice --group house --home --to bob --say "@home is the light on?"
                                                  # starts a group, invites bob, lets it use the devices
```

Without `--to`, `--say` and `--file` go to the first peer whose secure session comes up.

## How it works

The SDK does no I/O itself, so hopchat supplies the "platform bridge" for its peer-stream (`wifi_direct`) transport:

- **Discovery** (`src/net.rs`): a UDP multicast beacon on `239.255.42.99:47999` announces `address, tcp port, nick` every 2 s. The lower address dials.
- **Streams**: one TCP connection per peer. The first frame in each direction is the SDK's signed identity assertion, verified with `verify_identity_assertion`. A peer is announced only under the address that assertion proves, never under what the beacon claims.
- **Engine** (`src/engine.rs`): owns `OfflineProtocol` on one thread. It feeds frames in, drains `get_next_message()` out to the right stream, and forwards protocol events to the UI. Discovering a neighbor triggers the automatic key exchange.
- **Files**: the SDK's `send_media` chunks and seals each file and windows the chunks over the stream. The engine queues files until a session and a transfer slot (two per peer) are free. A transfer interrupted by quitting is re-supplied from its source path when the SDK asks for it.
- **Storage**: the SDK's file stores and hopchat's history, sealed with a key in `~/.hopchat/<name>/store.key`.

The nick in the beacon is unauthenticated and only for display; the `off1…` address is the identity.
