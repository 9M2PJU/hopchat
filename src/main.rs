//! hopchat: offline terminal chat over an Offline Protocol mesh.
//!
//! Peers on the same network find each other with no server and no
//! internet; every conversation is end-to-end encrypted with MLS.

mod engine;
mod history;
mod net;
mod ui;

use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

const USAGE: &str = "usage: hopchat [--name NICK] [--dir PATH] [--downloads PATH]
               [--echo | --say TEXT | --file PATH] [--to NICK]

  --name NICK       what peers see you as (default: $USER)
  --dir PATH        where your identity and history live (default: ~/.hopchat/NICK)
  --downloads PATH  where received files are saved (default: ~/Downloads/hopchat)
  --echo            no UI: answer every message (a bot to test against)
  --say TEXT        no UI: send TEXT to the first peer that connects
  --file PATH       no UI: send the file at PATH to the first peer that connects
  --to NICK         with --say/--file: only send to a peer with this nick

Run two copies with different names to chat with yourself on one machine.";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut nick = std::env::var("USER").unwrap_or_else(|_| "anon".into());
    let mut dir: Option<PathBuf> = None;
    let mut downloads: Option<PathBuf> = None;
    let mut echo = false;
    let mut say: Option<String> = None;
    let mut file: Option<PathBuf> = None;
    let mut to: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--name" | "-n" => nick = args.next().ok_or(USAGE)?,
            "--dir" => dir = Some(args.next().ok_or(USAGE)?.into()),
            "--downloads" => downloads = Some(args.next().ok_or(USAGE)?.into()),
            "--echo" => echo = true,
            "--say" => say = Some(args.next().ok_or(USAGE)?),
            "--file" => file = Some(args.next().ok_or(USAGE)?.into()),
            "--to" => to = Some(args.next().ok_or(USAGE)?),
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => return Err(USAGE.into()),
        }
    }
    let nick: String = nick
        .chars()
        .filter(|c| !c.is_whitespace())
        .take(24)
        .collect();
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let dir = dir.unwrap_or_else(|| home.join(".hopchat").join(&nick));
    let downloads = downloads.unwrap_or_else(|| {
        let folder = home.join("Downloads");
        if folder.is_dir() {
            folder.join("hopchat")
        } else {
            dir.join("downloads")
        }
    });
    if let Some(path) = &file
        && !path.is_file()
    {
        return Err(format!("{} is not a file", path.display()).into());
    }

    let (update_tx, update_rx) = mpsc::channel();
    let (input_tx, input_rx) = mpsc::channel();

    let (engine, me, store_key) = engine::open(&dir, &nick, downloads.clone(), update_tx)?;

    // The network reports into the engine's input queue.
    let (net_tx, net_rx) = mpsc::channel();
    net::start(me.clone(), net_tx)?;
    {
        let input_tx = input_tx.clone();
        thread::spawn(move || {
            for event in net_rx {
                if input_tx.send(engine::Input::Net(event)).is_err() {
                    break;
                }
            }
        });
    }
    thread::spawn(move || engine.run(input_rx));

    if echo || say.is_some() || file.is_some() {
        headless(&me, update_rx, input_tx, echo, say, file, to);
        return Ok(());
    }

    let store = match history::Store::open(&dir, &store_key) {
        Ok(store) => Some(store),
        Err(e) => {
            eprintln!("history disabled: {e}");
            None
        }
    };
    let app = ui::App::new(me.nick, me.address, store, downloads);
    let mut terminal = ratatui::init();
    enable_extras();
    let result = app.run(&mut terminal, update_rx, input_tx);
    disable_extras();
    ratatui::restore();
    result?;
    Ok(())
}

/// Mouse, bracketed paste (so dropped files arrive as one paste), focus
/// reports (for read receipts), and unambiguous keys where the terminal
/// offers them (so Shift-Enter is distinct from Enter).
fn enable_extras() {
    use ratatui::crossterm::event::{
        EnableBracketedPaste, EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    };
    use ratatui::crossterm::execute;
    let mut out = std::io::stdout();
    let _ = execute!(out, EnableMouseCapture, EnableBracketedPaste, EnableFocusChange);
    if matches!(ratatui::crossterm::terminal::supports_keyboard_enhancement(), Ok(true)) {
        let _ = execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
}

fn disable_extras() {
    use ratatui::crossterm::event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, PopKeyboardEnhancementFlags,
    };
    use ratatui::crossterm::execute;
    let mut out = std::io::stdout();
    if matches!(ratatui::crossterm::terminal::supports_keyboard_enhancement(), Ok(true)) {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(out, DisableMouseCapture, DisableBracketedPaste, DisableFocusChange);
}

/// No TUI: prints what happens. `--echo` answers every message it gets;
/// `--say TEXT` and `--file PATH` send to the first peer whose secure
/// session is up (and, with `--to NICK`, whose beacon carries that nick).
fn headless(
    me: &net::LocalIdentity,
    updates: mpsc::Receiver<engine::Update>,
    commands: mpsc::Sender<engine::Input>,
    echo: bool,
    mut say: Option<String>,
    mut file: Option<PathBuf>,
    to: Option<String>,
) {
    use std::collections::{HashMap, HashSet};

    use engine::{Command, Input, Update};
    use offline_protocol::Event;

    println!("{} is {}", me.nick, me.address);
    let send = |c: Command| {
        let _ = commands.send(Input::Ui(c));
    };
    let mut nicks: HashMap<String, String> = HashMap::new();
    let mut ready: HashSet<String> = HashSet::new();
    // Sends once a ready peer is known to be the wanted one; the beacon
    // with its nick and its session can arrive in either order.
    let mut deliver = |address: &str, nicks: &HashMap<String, String>| {
        let wanted = to
            .as_ref()
            .is_none_or(|n| nicks.get(address) == Some(n));
        if !wanted {
            return;
        }
        if let Some(text) = say.take() {
            send(Command::Send {
                to: address.to_string(),
                text,
                reply: None,
            });
        }
        if let Some(path) = file.take() {
            send(Command::SendFile {
                to: address.to_string(),
                path,
                file_id: engine::new_file_id(),
            });
        }
    };
    for update in updates {
        match update {
            Update::Typing { .. } => {}
            Update::PeerSeen { address, nick } => {
                nicks.insert(address.clone(), nick);
                if ready.contains(&address) {
                    deliver(&address, &nicks);
                }
            }
            Update::PeerUp { address } => println!("link up {address}"),
            Update::PeerDown { address } => println!("link down {address}"),
            Update::Queued { to, id, text, .. } => println!("queued {id} to {to}: {text}"),
            Update::Incoming {
                from, text, hops, ..
            } => {
                println!("message from {from} ({hops} hop): {text}");
                if echo {
                    send(Command::Send {
                        to: from,
                        text: format!("echo: {text}"),
                        reply: None,
                    });
                }
            }
            Update::SessionReady { address } => {
                println!("secure session with {address}");
                ready.insert(address.clone());
                deliver(&address, &nicks);
            }
            Update::FileQueued {
                file_id, name, size, ..
            } => println!("file queued {file_id}: {name} ({size} bytes)"),
            Update::FileProgress {
                file_id,
                done,
                total,
                ..
            } => println!("file progress {file_id} {done}/{total}"),
            Update::FileSent { file_id } => println!("file sent {file_id}"),
            Update::FileFailed { file_id, reason } => println!("file failed {file_id}: {reason}"),
            Update::FileReceived {
                from,
                name,
                size,
                path,
                ..
            } => {
                println!("file from {from}: {name} ({size} bytes) saved to {}", path.display());
                if echo {
                    send(Command::Send {
                        to: from,
                        text: format!("echo: got {name} ({size} bytes)"),
                        reply: None,
                    });
                }
            }
            Update::FileLost {
                from, name, reason, ..
            } => println!("file from {from} lost: {name}: {reason}"),
            Update::ResendWanted { file_id, .. } => println!("resend wanted {file_id}"),
            Update::Read { from, ids } => println!("read by {from}: {}", ids.join(",")),
            Update::Error(e) => println!("error {e}"),
            Update::Protocol(event) => match *event {
                Event::MessageDelivered {
                    message_id,
                    latency_ms,
                    ..
                } => println!("delivered {message_id} in {latency_ms}ms"),
                Event::MessageReceived {
                    message_id,
                    encrypted,
                    ..
                } => println!("received {message_id} encrypted={encrypted}"),
                Event::MessageFailed {
                    message_id, reason, ..
                } => println!("failed {message_id}: {reason}"),
                _ => {}
            },
        }
    }
}
