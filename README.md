# hopchat

Offline terminal chat built on the [Offline Protocol SDK](https://github.com/Offline-Protocol/offline-protocol-sdk) and [ratatui](https://ratatui.rs).
No server and no internet: peers on the same network find each other, and every conversation is end-to-end encrypted with MLS (RFC 9420).

```
cargo run -- --name alice          # on one machine (or terminal)
cargo run -- --name bob            # on another
```

Two terminals on one machine work too. Each name gets its own identity under `~/.hopchat/<name>`.

## What you get

- **Quiet to look at**: soft pastels on a dusk background, no glow and no flicker. Everyone gets their own colour, the same in the sidebar and their messages, so you can tell who said what at a glance. It opens with a moment of the name fading in (any key skips it), and while nobody is around the lobby shows slow ripples instead of an empty page.
- **Nothing to memorise**: the keys along the bottom are buttons, so click them. Click a chat on the left to open it. Click a message to reply, open, copy or retry it. Type `@` in a group or the lobby and it offers `@home` and `@ai`; Tab completes.

- **Files**: press Ctrl-O to browse, drag files onto the window, or type `/file PATH`. Each transfer shows a live progress bar with rate and time left. Transfers are chunked and MLS-encrypted, up to 100 MB each. Received files are saved to `~/Downloads/hopchat` (override with `--downloads`), never overwriting anything and never escaping that folder.
- **Delivery you can see**: `◷` queued, `✓` on the wire, `✓✓` delivered, cyan `✓✓ read` once they've seen it, `✗` failed (press `R` to retry).
- **Replies, typing indicators, unread badges**, plus a bell and a window-title count when a message arrives while you're elsewhere.
- **History that survives restarts**: each conversation is sealed with XChaCha20-Poly1305 under your profile's key in `~/.hopchat/<name>/history`. Messages written while a peer is away are delivered when they come back.
- **A real composer**: multi-line input (Shift/Alt-Enter or Ctrl-J), word motions, slash commands with completion.

| Key | Action |
| --- | --- |
| Enter | send (with any attached files) |
| Ctrl-O | attach files |
| Tab / Shift-Tab, Alt-1…9 | switch conversation |
| click a message, or ↑ on an empty line | select it: `r` reply, `o` open, `f` show in folder, `y` copy, `R` retry |
| PgUp / PgDn, mouse wheel | scroll |
| Ctrl-G | new group: name it, pick people, Enter |
| Ctrl-D | devices: find, pair and test them |
| Ctrl-E | protocol event log |
| F1 | help |
| Ctrl-C / Ctrl-Q | quit |

Commands: `/file`, `/downloads`, `/info`, `/me`, `/clear`, `/log`, `/help`, `/quit`, for groups and the lobby `/group`, `/invite`, `/members`, `/leave`, for devices `/devices`, `/home`, and for the model `/ai`.

## The lobby

hopchat opens in the **Lobby**, pinned at the top of the sidebar. Everyone on your network who runs hopchat is in it by themselves: there's nothing to create and nobody to invite.

```
anyone up for lunch?
@home turn the lights down
@ai what's a good name for a sourdough starter?
```

Your own `@home` and `@ai` always work in the lobby, with nothing to set up beyond the devices and the model themselves. Only you can use them there until you say otherwise: `/home on` and `/ai on` in the lobby (or `g` and `i` in the Devices panel while it's open) let everyone nearby use your devices and ask your model. `off` makes them yours again.

A lobby message goes, end-to-end encrypted, to each person who is connected when you send it. It's a room, not a mailbox: someone who joins later doesn't get what was said before, and there are no delivery ticks. For something that should wait for a person, write to them directly. The lobby can't be left, and files still go one-to-one.

## Me

Right under the lobby is **Me**: a conversation with nobody else in it. Jot notes to yourself, and talk to your own devices and model:

```
remember to call the landlord
@home dim the desk lamp to 30%
@ai what can I make with eggs, spinach and feta?
```

Nothing written in Me goes on the network. It stays on this machine, kept in history like any other conversation. `@home` and `@ai` always answer you here, with nothing to switch on, and nobody else can ever reach them through it. There is nobody to invite and no leaving it, and files still go one-to-one.

## Groups

Press **Ctrl-G** (or click **+ New group** at the top of the sidebar, or type `/group`). Name the group, press ↓, tick people with Space, and press Enter. The group is created with everyone you picked invited, and it opens. Only people who are online can be picked, because an invite needs their key package. `/invite` in a group opens the same picker to add more people. The quick forms still work: `/group house` creates a group straight away, and `/invite bob` adds one person. Groups are MLS groups (RFC 9420) carried over the same peer-to-peer links, so they need no server either. Files still go one-to-one.

## Home: talk to your devices

You can talk to the smart devices on your own machine in [Me](#me) or the [lobby](#the-lobby), and anyone in a group (or in the lobby, once you let them) can too. Mention `@home`:

```
@home dim the lights to 20% and put on netflix
@home make the lamp purple
@home turn the tv down a bit
@home is the light on?
```

A local model (via [Ollama](https://ollama.com)) turns the request into device actions, and home answers in the conversation with a 🏠. Nothing leaves your network.

Setup on the host:

1. `ollama serve`, with a tool-calling model pulled (`ollama pull qwen2.5:7b` is the default; `/home model NAME` picks another). The host can skip this if someone else in the group runs a model: see [One model for the whole group](#one-model-for-the-whole-group).
2. Press **Ctrl-D** (or `/devices`). hopchat scans the network for Wiz lights and LG TVs and lists what it finds.
   - **A light:** Enter adds it, `t` blinks it so you can tell which one it is, and `n` gives it a name people will use ("Desk lamp").
   - **A TV:** Enter pairs it. The TV asks whether to allow hopchat; choose *Allow* with the remote. For "turn the TV on" to work, enable *Turn on via Wi-Fi* (or *Mobile TV On*) in the TV's settings. A TV only shows up while it's switched on.
   - The panel also shows whether the model is ready, and the exact command to run if it isn't.
3. To try it yourself, open the lobby and mention `@home`. To let other people in, open a group (or the lobby) and press `g` in the panel (or type `/home on`). `g` again, or `/home off`, revokes it.

If a device is on another subnet or the scan can't see it, `/home add 192.168.1.40` adds it by address.

Devices are remembered by MAC address, so one that gets a new IP from the router is followed, not lost. Each light's capabilities come from its module name: a white-tones bulb is never asked for a colour, and home tells people so.

Being in the group is the permission: everyone MLS has admitted to a group where home is on can use the devices, and nobody else can. In the lobby it's you, plus everyone with a secure session to you if you switched home on there. Home only answers while the host's hopchat is running. Settings are kept in `~/.hopchat/<name>/home.json`. That file holds each TV's client key and its pinned certificate: the TV's certificate is self-signed, so hopchat trusts the one it sees at pairing and refuses a different one later.

Devices: Wiz bulbs (local UDP, port 38899) and LG webOS TVs (the WebSocket API on port 3001 over TLS, or 3000 on older sets). The code is in `src/home/`.

## One model for the whole group

Only one person in a group needs to run a model. Everyone else can use it.

**Ask it anything.** In the lobby you can ask your own model straight away. In a group, the person whose machine runs the model types `/ai on` (or presses `i` in the Devices panel). After that, anyone in the group can mention `@ai`:

```
@ai what's a good name for a sourdough starter?
@ai summarise what alice just said
@ai and in French?
```

The answer comes back as a 🤖 reply to the question. Each group's recent questions and answers are remembered, so follow-ups work. `/ai forget` clears that memory, and `/ai off` stops answering.

**It thinks for `@home` too.** The machine with the devices doesn't need a model. If none is ready there, home sends each step to a model that someone in the group shares, and runs the device actions itself. The other machine only does the thinking and never touches the devices. The Devices panel shows `borrowing bob (qwen2.5:7b)`. `@ai` works the same way on a host whose own model is down.

**Sharing.** A machine whose model is ready offers it to the people it is connected to. It answers only someone who is in a group with it (the lobby counts, so anyone with a secure session to it), for that group's question, and at most two requests at a time. It shows a note when someone starts using it ("alice is using your qwen2.5:7b"). `/ai share off` stops lending, and `/ai share on` resumes it. A bare `/ai` shows the model, where it's on, and whose models you could borrow.

The questions, and for `@home` the device names and states, go to the lender inside the same end-to-end encrypted one-to-one session as chat. They are never shown as messages. Nothing leaves your network.

## Headless modes

```
hopchat --name bot --echo                         # answers every message and file: something to test against
hopchat --name alice --say "hello" --to bot       # sends one message, prints what happens
hopchat --name alice --file ./notes.pdf --to bot  # sends one file
hopchat --name alice --group house --home --to bob --say "@home is the light on?"
                                                  # starts a group, invites bob, lets it use the devices
hopchat --name alice --group quiz --ai --to bob --say "@ai what is 2+2?"
                                                  # the same, and lets the group ask the model
hopchat --name alice --lobby --to bob --say "hi all"
                                                  # says it in the lobby once bob is there
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
