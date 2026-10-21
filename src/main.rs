//! hopchat: offline terminal chat over an Offline Protocol mesh.
//!
//! Peers on the same network find each other with no server and no
//! internet; every conversation is end-to-end encrypted with MLS.

mod engine;
mod net;
mod ui;

use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

const USAGE: &str = "usage: hopchat [--name NICK] [--dir PATH] [--echo | --say TEXT]

  --name NICK   what peers see you as (default: $USER)
  --dir PATH    where your identity lives (default: ~/.hopchat/NICK)
  --echo        no UI: answer every message (a bot to test against)
  --say TEXT    no UI: send TEXT to the first peer that connects

Run two copies with different names to chat with yourself on one machine.";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut nick = std::env::var("USER").unwrap_or_else(|_| "anon".into());
    let mut dir: Option<PathBuf> = None;
    let mut echo = false;
    let mut say: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--name" | "-n" => nick = args.next().ok_or(USAGE)?,
            "--dir" => dir = Some(args.next().ok_or(USAGE)?.into()),
            "--echo" => echo = true,
            "--say" => say = Some(args.next().ok_or(USAGE)?),
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
    let dir = dir.unwrap_or_else(|| {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        home.join(".hopchat").join(&nick)
    });

    let (update_tx, update_rx) = mpsc::channel();
    let (input_tx, input_rx) = mpsc::channel();

    let (engine, me) = engine::open(&dir, &nick, update_tx)?;

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

    if echo || say.is_some() {
        headless(&me, update_rx, input_tx, echo, say);
        return Ok(());
    }

    let app = ui::App::new(me.nick, me.address);
    let mut terminal = ratatui::init();
    let result = app.run(&mut terminal, update_rx, input_tx);
    ratatui::restore();
    result?;
    Ok(())
}

/// No TUI: prints what happens. `--echo` answers every message it gets;
/// `--say TEXT` sends TEXT to the first peer whose secure session is up.
fn headless(
    me: &net::LocalIdentity,
    updates: mpsc::Receiver<engine::Update>,
    commands: mpsc::Sender<engine::Input>,
    echo: bool,
    mut say: Option<String>,
) {
    use engine::{Command, Input, Update};
    use offline_protocol::Event;

    println!("{} is {}", me.nick, me.address);
    let send = |to: String, text: String| {
        let _ = commands.send(Input::Ui(Command::Send { to, text }));
    };
    for update in updates {
        match update {
            Update::PeerSeen { .. } => {}
            Update::PeerUp { address } => println!("link up {address}"),
            Update::PeerDown { address } => println!("link down {address}"),
            Update::Queued { to, id, text } => println!("queued {id} to {to}: {text}"),
            Update::Incoming { from, text, hops } => {
                println!("message from {from} ({hops} hop): {text}");
                if echo {
                    send(from, format!("echo: {text}"));
                }
            }
            Update::SessionReady { address } => {
                println!("secure session with {address}");
                if let Some(text) = say.take() {
                    send(address, text);
                }
            }
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
