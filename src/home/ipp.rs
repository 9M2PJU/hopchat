//! Network printers and copiers, over IPP: the Internet Printing Protocol
//! every networked printer from the last decade speaks, multifunction
//! copiers included. They announce themselves over Bonjour as
//! `_ipp._tcp`, and answer `Get-Printer-Attributes` with their state,
//! their ink or toner levels and anything wrong, such as a paper jam.
//!
//! Home only reads a printer, and asks it to identify itself when tested.
//! It never sends a job: nothing anyone says to `@home` can use up paper.
//!
//! A printer is known by the UUID it reports, kept where other devices
//! keep a MAC address, so it is still itself after DHCP moves it.

use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::sync::{mpsc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::driver::{Backend, Category, Found};
use super::printer::PrinterDriver;
use super::{Device, State};

const SERVICE: &str = "_ipp._tcp.local.";
const PORT: u16 = 631;
const WAIT: Duration = Duration::from_millis(2500);
/// How long a scan listens for printers announcing themselves.
const DISCOVER: Duration = Duration::from_secs(3);
/// The most a printer's answer may be; its attributes are a few KB.
const MAX_ANSWER: u64 = 1 << 20;
/// Where printers usually take requests, tried in turn for one added by
/// its address: IPP Everywhere's path, then older ones.
const PATHS: &[&str] = &["ipp/print", "ipp", ""];

const GET_PRINTER_ATTRIBUTES: u16 = 0x000B;
const IDENTIFY_PRINTER: u16 = 0x003C;
/// `server-error-version-not-supported`: the printer wants another version.
const VERSION_NOT_SUPPORTED: u16 = 0x0503;
const IPP_2_0: [u8; 2] = [2, 0];
const IPP_1_1: [u8; 2] = [1, 1];

/// What home asks a printer for.
const WANTED: &[&str] = &[
    "printer-make-and-model",
    "printer-info",
    "printer-uuid",
    "printer-state",
    "printer-state-reasons",
    "marker-names",
    "marker-levels",
    "marker-low-levels",
    "operations-supported",
];

/// IPP printers and copiers, as a home backend.
pub struct Ipp;

fn found(ip: Ipv4Addr, uuid: Option<String>, port: u16, path: &str, model: Option<String>) -> Found {
    let mut data = json!({"port": port, "path": path});
    if let Some(model) = model {
        data["model"] = json!(model);
    }
    Found { backend: "ipp", ip, mac: uuid.map(|u| uuid_of(&u)), unit: None, data, checked: None }
}

/// A UUID however it was written ("urn:uuid:…" by IPP, bare in a TXT
/// record), in one form, so both ways of finding a printer agree.
fn uuid_of(said: &str) -> String {
    said.trim().trim_start_matches("urn:uuid:").to_ascii_lowercase()
}

fn model_in(data: &Value) -> Option<&str> {
    data.get("model").and_then(Value::as_str).filter(|m| !m.is_empty())
}

fn model(d: &Device) -> Option<&str> {
    model_in(&d.data)
}

/// The printer at `ip`, on the port and path `data` keeps for it.
fn at(ip: Ipv4Addr, data: &Value) -> Printer {
    let port = data.get("port").and_then(Value::as_u64).and_then(|p| u16::try_from(p).ok()).unwrap_or(PORT);
    let path = data.get("path").and_then(Value::as_str).unwrap_or(PATHS[0]);
    Printer { ip, port, path: path.into() }
}

fn printer(d: &Device) -> Printer {
    at(d.ip, &d.data)
}

impl Backend for Ipp {
    fn id(&self) -> &'static str {
        "ipp"
    }

    fn category(&self) -> Category {
        Category::Printer
    }

    fn label(&self) -> &'static str {
        "printers"
    }

    fn discover(&self, each: &dyn Fn(Found)) {
        let Ok(daemon) = mdns_sd::ServiceDaemon::new() else { return };
        let Ok(browse) = daemon.browse(SERVICE) else { return };
        let until = Instant::now() + DISCOVER;
        let mut seen = std::collections::HashSet::new();
        let (asked_tx, asked) = mpsc::channel();
        // A printer whose announcement has no UUID is asked for one on its
        // own thread, so a slow one doesn't keep the others from being
        // heard. Every such thread has finished when the scope ends.
        thread::scope(|threads| {
            while let Some(left) = until.checked_duration_since(Instant::now()) {
                match browse.recv_timeout(left) {
                    Ok(mdns_sd::ServiceEvent::ServiceResolved(service)) => {
                        let Some(ip) = service.get_addresses_v4().into_iter().min() else { continue };
                        if !seen.insert(service.get_fullname().to_string()) {
                            continue;
                        }
                        match announced(ip, service.get_port(), |k| service.get_property_val_str(k)) {
                            Some(f) if f.mac.is_none() => {
                                let tx = asked_tx.clone();
                                threads.spawn(move || {
                                    let _ = tx.send(asked_about(f));
                                });
                            }
                            Some(f) => each(f),
                            None => {}
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
                asked.try_iter().for_each(each);
            }
        });
        asked.try_iter().for_each(each);
        let _ = daemon.shutdown();
    }

    fn probe(&self, ip: Ipv4Addr) -> Option<Found> {
        // Most addresses aren't printers: don't wait on IPP for each path.
        TcpStream::connect_timeout(&SocketAddr::from((ip, PORT)), WAIT).ok()?;
        PATHS.iter().find_map(|path| {
            let attrs = Printer { ip, port: PORT, path: (*path).into() }.attributes().ok()?;
            Some(told(found(ip, None, PORT, path, None), &attrs))
        })
    }

    fn name(&self, d: &Device) -> Option<String> {
        model(d).map(String::from)
    }

    fn about(&self, d: &Device) -> String {
        match model(d) {
            Some(m) => format!("{m}; reports its state, ink or toner, and paper"),
            None => "reports its state, ink or toner, and paper".into(),
        }
    }

    fn found_detail(&self, d: &Device) -> String {
        format!("IPP · {}", model(d).unwrap_or("printer"))
    }

    fn check(&self, d: &Device) -> (State, String) {
        match printer(d).attributes() {
            Ok(attrs) => (State::Ready, attrs.summary()),
            Err(_) => (State::Offline, "not answering".into()),
        }
    }

    fn test(&self, d: &mut Device) -> Result<String, String> {
        let p = printer(d);
        let attrs = p.attributes()?;
        if !attrs.ints("operations-supported").any(|op| op == i32::from(IDENTIFY_PRINTER)) {
            return Err(format!("{} can't identify itself; it is the printer at {}", d.name, d.ip));
        }
        p.identify().map(|()| format!("asked {} to show itself", d.name))
    }

    fn test_help(&self) -> &'static str {
        " makes it beep or flash so you can find it"
    }

    fn as_printer(&self) -> Option<&dyn PrinterDriver> {
        Some(self)
    }
}

impl PrinterDriver for Ipp {
    fn status(&self, d: &Device) -> Result<String, String> {
        printer(d).attributes().map(|a| a.summary())
    }
}

/// The printer a Bonjour announcement is for, given its address, port
/// and TXT record; `None` for one home leaves out.
fn announced<'a>(ip: Ipv4Addr, port: u16, txt: impl Fn(&str) -> Option<&'a str>) -> Option<Found> {
    // A computer sharing a printer it drives (CUPS says so with this key):
    // the printer itself is found on its own.
    if txt("printer-type").is_some() {
        return None;
    }
    let path = txt("rp").unwrap_or(PATHS[0]).trim_matches('/');
    let model = txt("ty").map(str::trim).filter(|m| !m.is_empty()).map(String::from);
    // IPP Everywhere requires a UUID in the TXT record, but not every
    // printer has one: one without is left for `asked_about`.
    Some(found(ip, txt("UUID").map(String::from), port, path, model))
}

/// `f`, with what the printer says of itself when asked; as it was if the
/// printer doesn't answer.
fn asked_about(f: Found) -> Found {
    match at(f.ip, &f.data).attributes() {
        Ok(attrs) => told(f, &attrs),
        Err(_) => f,
    }
}

/// `f` with what a printer's attributes add: its UUID and model when `f`
/// has none, and what it is doing, so no one need ask it again at once.
fn told(mut f: Found, attrs: &Attributes) -> Found {
    if f.mac.is_none() {
        f.mac = attrs.text("printer-uuid").map(uuid_of);
    }
    if model_in(&f.data).is_none()
        && let Some(model) = attrs.model()
    {
        f.data["model"] = json!(model);
    }
    f.checked = Some((State::Ready, attrs.summary()));
    f
}

struct Printer {
    ip: Ipv4Addr,
    port: u16,
    path: String,
}

impl Printer {
    fn uri(&self, scheme: &str) -> String {
        format!("{scheme}://{}:{}/{}", self.ip, self.port, self.path)
    }

    fn attributes(&self) -> Result<Attributes, String> {
        let mut req = Request::new(GET_PRINTER_ATTRIBUTES, &self.uri("ipp"));
        req.keywords("requested-attributes", WANTED);
        self.send(req)
    }

    fn identify(&self) -> Result<(), String> {
        let mut req = Request::new(IDENTIFY_PRINTER, &self.uri("ipp"));
        req.name("requesting-user-name", "hopchat");
        self.send(req).map(drop)
    }

    fn send(&self, req: Request) -> Result<Attributes, String> {
        let mut request = req.finish();
        let (mut status, mut attrs) = self.post(&request)?;
        // A printer that only speaks IPP 1.1 says so with this status; ask
        // it again in 1.1, as CUPS does. Nothing home asks needs 2.0.
        if status == VERSION_NOT_SUPPORTED {
            request[..2].copy_from_slice(&IPP_1_1);
            (status, attrs) = self.post(&request)?;
        }
        // 0x0000–0x00FF are the successful ones.
        if status > 0x00FF {
            return Err(format!("the printer refused (IPP status {status:#06x})"));
        }
        Ok(attrs)
    }

    /// Sends one encoded request; the answer's status and attributes.
    fn post(&self, request: &[u8]) -> Result<(u16, Attributes), String> {
        let mut response = agent()
            .post(self.uri("http"))
            .header("Content-Type", "application/ipp")
            .send(request)
            .map_err(|e| format!("the printer isn't answering: {e}"))?;
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_ANSWER)
            .read_to_vec()
            .map_err(|e| format!("couldn't read the printer's answer: {e}"))?;
        parse(&body).ok_or_else(|| "the printer's answer made no sense".into())
    }
}

/// One client for every printer, so its connections are reused.
fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_connect(Some(WAIT))
            .timeout_global(Some(WAIT))
            // A printer has no reason to send home elsewhere, and following
            // would let anything on the LAN point it at any URL.
            .max_redirects(0)
            .build()
            .new_agent()
    })
}

/// An IPP request, in the binary form of RFC 8010.
struct Request {
    bytes: Vec<u8>,
}

const OPERATION: u8 = 0x01;
const END: u8 = 0x03;
const INTEGER: u8 = 0x21;
const ENUM: u8 = 0x23;
const BEGIN_COLLECTION: u8 = 0x34;
const END_COLLECTION: u8 = 0x37;
const TEXT_WITH_LANGUAGE: u8 = 0x35;
const NAME_WITH_LANGUAGE: u8 = 0x36;
const NAME: u8 = 0x42;
const KEYWORD: u8 = 0x44;
const URI: u8 = 0x45;
const CHARSET: u8 = 0x47;
const LANGUAGE: u8 = 0x48;

impl Request {
    fn new(operation: u16, uri: &str) -> Request {
        let mut bytes = IPP_2_0.to_vec();
        bytes.extend(operation.to_be_bytes());
        bytes.extend(1u32.to_be_bytes());
        bytes.push(OPERATION);
        let mut req = Request { bytes };
        req.value(CHARSET, "attributes-charset", "utf-8");
        req.value(LANGUAGE, "attributes-natural-language", "en");
        req.value(URI, "printer-uri", uri);
        req
    }

    fn value(&mut self, tag: u8, name: &str, value: &str) {
        self.bytes.push(tag);
        for part in [name, value] {
            self.bytes.extend((part.len() as u16).to_be_bytes());
            self.bytes.extend(part.as_bytes());
        }
    }

    fn name(&mut self, name: &str, value: &str) {
        self.value(NAME, name, value);
    }

    /// A keyword attribute with several values: every value after the
    /// first goes with no name.
    fn keywords(&mut self, name: &str, values: &[&str]) {
        for (i, v) in values.iter().enumerate() {
            self.value(KEYWORD, if i == 0 { name } else { "" }, v);
        }
    }

    /// The request as sent, ended.
    fn finish(mut self) -> Vec<u8> {
        self.bytes.push(END);
        self.bytes
    }
}

#[derive(Debug, PartialEq)]
pub(super) enum Val {
    Int(i32),
    Text(String),
    Other,
}

/// What a printer answered: each attribute with its values, in order.
#[derive(Debug, Default)]
pub(super) struct Attributes(Vec<(String, Vec<Val>)>);

impl Attributes {
    fn values(&self, name: &str) -> impl Iterator<Item = &Val> {
        self.0.iter().filter(move |(n, _)| n == name).flat_map(|(_, v)| v)
    }

    fn texts(&self, name: &str) -> impl Iterator<Item = &str> {
        self.values(name).filter_map(|v| match v {
            Val::Text(t) => Some(t.as_str()),
            _ => None,
        })
    }

    fn text(&self, name: &str) -> Option<&str> {
        self.texts(name).map(str::trim).find(|t| !t.is_empty())
    }

    fn ints(&self, name: &str) -> impl Iterator<Item = i32> {
        self.values(name).filter_map(|v| match v {
            Val::Int(n) => Some(*n),
            _ => None,
        })
    }

    fn model(&self) -> Option<String> {
        self.text("printer-make-and-model").or_else(|| self.text("printer-info")).map(String::from)
    }

    /// What it is doing, how much ink or toner it has, and anything
    /// wrong, e.g. "idle · black 40%, cyan 8% (low) · out of paper".
    fn summary(&self) -> String {
        let state = match self.ints("printer-state").next() {
            Some(3) => "idle",
            Some(4) => "printing",
            Some(5) => "stopped",
            _ => "answering",
        };
        let mut parts = vec![state.to_string()];
        let names: Vec<&str> = self.texts("marker-names").collect();
        let levels: Vec<i32> = self.ints("marker-levels").collect();
        let lows: Vec<i32> = self.ints("marker-low-levels").collect();
        let supplies: Vec<String> = names
            .iter()
            .zip(&levels)
            .enumerate()
            .filter_map(|(i, (name, level))| {
                let name = name.trim().to_lowercase();
                // -1 and -2 are "unknown", -3 "some left".
                match *level {
                    0..=100 => {
                        let low = lows.get(i).is_some_and(|low| level <= low);
                        Some(format!("{name} {level}%{}", if low { " (low)" } else { "" }))
                    }
                    -3 => Some(format!("{name} ok")),
                    _ => None,
                }
            })
            .collect();
        if !supplies.is_empty() {
            parts.push(supplies.join(", "));
        }
        // Several keywords can say the same thing ("media-empty" and
        // "media-needed"), and not always side by side: say each once.
        let mut wrong: Vec<String> = Vec::new();
        for said in self.texts("printer-state-reasons").filter_map(reason) {
            if !wrong.contains(&said) {
                wrong.push(said);
            }
        }
        if !wrong.is_empty() {
            parts.push(wrong.join(", "));
        }
        parts.join(" · ")
    }
}

/// A `printer-state-reasons` keyword in words; `None` for one that says
/// nothing is wrong.
fn reason(keyword: &str) -> Option<String> {
    let k = ["-report", "-warning", "-error"].iter().fold(keyword.trim(), |k, end| k.strip_suffix(end).unwrap_or(k));
    let said = match k {
        "none" | "other" | "" => return None,
        "media-empty" | "media-needed" => "out of paper",
        "media-low" => "paper low",
        "media-jam" => "paper jam",
        "toner-low" => "toner low",
        "toner-empty" => "out of toner",
        "marker-supply-low" => "ink low",
        "marker-supply-empty" => "out of ink",
        "marker-waste-almost-full" => "waste ink almost full",
        "marker-waste-full" => "waste ink full",
        "door-open" | "cover-open" | "interlock-open" => "a door is open",
        "input-tray-missing" => "a paper tray is out",
        "output-area-full" => "output tray full",
        "offline" | "shutdown" => "offline",
        "paused" | "moving-to-paused" => "paused",
        "spool-area-full" => "its queue is full",
        k => return Some(k.replace('-', " ")),
    };
    Some(said.into())
}

/// The status code and attributes of an IPP answer; `None` for bytes that
/// aren't one. Values inside collections are skipped: home asks for none.
fn parse(body: &[u8]) -> Option<(u16, Attributes)> {
    let status = u16::from_be_bytes(body.get(2..4)?.try_into().ok()?);
    // Past the version, status and request id.
    let mut at = 8;
    let mut attrs = Attributes::default();
    let mut depth = 0usize;
    loop {
        let tag = *body.get(at)?;
        at += 1;
        if tag == END {
            return Some((status, attrs));
        }
        // A group starts: its attributes are read like any other.
        if tag < 0x10 {
            continue;
        }
        let name = take(body, &mut at)?;
        let value = take(body, &mut at)?;
        match tag {
            BEGIN_COLLECTION => {
                // Its name, at the top, still says the attribute exists.
                if depth == 0 {
                    push(&mut attrs, name, Val::Other);
                }
                depth += 1;
                continue;
            }
            END_COLLECTION => {
                depth = depth.checked_sub(1)?;
                continue;
            }
            _ if depth > 0 => continue,
            _ => {}
        }
        let val = match tag {
            INTEGER | ENUM => Val::Int(i32::from_be_bytes(value.try_into().ok()?)),
            TEXT_WITH_LANGUAGE | NAME_WITH_LANGUAGE => {
                let mut inner = 0;
                let _language = take(value, &mut inner)?;
                Val::Text(String::from_utf8_lossy(take(value, &mut inner)?).into_owned())
            }
            0x41..=0x49 => Val::Text(String::from_utf8_lossy(value).into_owned()),
            _ => Val::Other,
        };
        push(&mut attrs, name, val);
    }
}

/// An attribute with a name starts; one with none is another value of
/// the last.
fn push(attrs: &mut Attributes, name: &[u8], val: Val) {
    match attrs.0.last_mut() {
        Some((_, values)) if name.is_empty() => values.push(val),
        _ => attrs.0.push((String::from_utf8_lossy(name).into_owned(), vec![val])),
    }
}

/// A length-prefixed field at `at`, moving past it.
fn take<'a>(body: &'a [u8], at: &mut usize) -> Option<&'a [u8]> {
    let len = usize::from(u16::from_be_bytes(body.get(*at..*at + 2)?.try_into().ok()?));
    let field = body.get(*at + 2..*at + 2 + len)?;
    *at += 2 + len;
    Some(field)
}

/// A printer on localhost that answers IPP as `answer` says, for tests.
#[cfg(test)]
pub(super) mod fake {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;

    use super::{parse, Attributes};

    /// The operations asked of it, in order.
    pub type Asked = Arc<Mutex<Vec<(u16, Attributes)>>>;

    /// Its port, and what it was asked.
    pub fn printer(answer: impl Fn(u16) -> Vec<u8> + Send + Sync + 'static) -> (u16, Asked) {
        speaking(move |_, op| answer(op))
    }

    /// One that answers as `answer` says given the IPP version a request
    /// was sent in, as `[major, minor]`, and its operation.
    pub fn speaking(answer: impl Fn([u8; 2], u16) -> Vec<u8> + Send + Sync + 'static) -> (u16, Asked) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let asked: Asked = Default::default();
        let answer = Arc::new(answer);
        let log = asked.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let (answer, log) = (answer.clone(), log.clone());
                thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut stream = stream;
                    // One connection may carry several requests.
                    loop {
                        let mut length = 0;
                        loop {
                            let mut line = String::new();
                            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                                return;
                            }
                            if line == "\r\n" {
                                break;
                            }
                            if let Some((k, v)) = line.split_once(':')
                                && k.eq_ignore_ascii_case("content-length")
                            {
                                length = v.trim().parse().unwrap();
                            }
                        }
                        let mut body = vec![0; length];
                        reader.read_exact(&mut body).unwrap();
                        // A request is laid out as an answer is, with the
                        // operation where the status goes.
                        let (op, attrs) = parse(&body).expect("a well-formed IPP request");
                        let reply = answer([body[0], body[1]], op);
                        log.lock().unwrap().push((op, attrs));
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/ipp\r\nContent-Length: {}\r\n\r\n",
                            reply.len()
                        );
                        stream.write_all(head.as_bytes()).unwrap();
                        stream.write_all(&reply).unwrap();
                    }
                });
            }
        });
        (port, asked)
    }

    /// An IPP answer: `status`, then the given attributes in one printer
    /// group. An empty name is another value of the attribute before.
    pub fn answer(status: u16, attrs: &[(u8, &str, &[u8])]) -> Vec<u8> {
        let mut b = vec![2, 0];
        b.extend(status.to_be_bytes());
        b.extend(1u32.to_be_bytes());
        b.push(0x01);
        for (tag, name, value) in [(0x47, "attributes-charset", &b"utf-8"[..]), (0x48, "attributes-natural-language", b"en")] {
            put(&mut b, tag, name, value);
        }
        b.push(0x04);
        for (tag, name, value) in attrs {
            put(&mut b, *tag, name, value);
        }
        b.push(0x03);
        b
    }

    fn put(b: &mut Vec<u8>, tag: u8, name: &str, value: &[u8]) {
        b.push(tag);
        b.extend((name.len() as u16).to_be_bytes());
        b.extend(name.as_bytes());
        b.extend((value.len() as u16).to_be_bytes());
        b.extend(value);
    }

    /// A busy colour inkjet with no paper, that can identify itself.
    pub fn inkjet(op: u16) -> Vec<u8> {
        if op == super::IDENTIFY_PRINTER {
            return answer(0, &[]);
        }
        answer(0, &[
            (0x41, "printer-make-and-model", b"Acme InkJet 3000"),
            (0x45, "printer-uuid", b"urn:uuid:ABCD-1234"),
            (0x23, "printer-state", &4i32.to_be_bytes()),
            (0x44, "printer-state-reasons", b"media-empty-error"),
            (0x44, "", b"marker-supply-low-warning"),
            (0x41, "marker-names", b"Black"),
            (0x41, "", b"Cyan"),
            (0x41, "", b"Magenta"),
            (0x21, "marker-levels", &60i32.to_be_bytes()),
            (0x21, "", &8i32.to_be_bytes()),
            (0x21, "", &(-2i32).to_be_bytes()),
            (0x21, "marker-low-levels", &10i32.to_be_bytes()),
            (0x21, "", &10i32.to_be_bytes()),
            (0x21, "", &10i32.to_be_bytes()),
            (0x23, "operations-supported", &0x000Bi32.to_be_bytes()),
            (0x23, "", &0x003Ci32.to_be_bytes()),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{answer, inkjet, printer, speaking};
    use super::*;

    fn device(port: u16) -> Device {
        Device::new("ipp", "Office".into(), Ipv4Addr::LOCALHOST, None, json!({"port": port, "path": "ipp/print"}))
    }

    #[test]
    fn an_answer_reads_back_with_every_value_and_skips_collections() {
        let mut lang = Vec::new();
        for part in [&b"en"[..], b"Front desk"] {
            lang.extend((part.len() as u16).to_be_bytes());
            lang.extend(part);
        }
        let body = answer(0x0001, &[
            (0x35, "printer-info", &lang),
            (0x34, "media-col-default", b""),
            (0x4A, "", b"media-size"),
            (0x34, "", b""),
            (0x21, "x-dimension", &21000i32.to_be_bytes()),
            (0x37, "", b""),
            (0x37, "", b""),
            (0x23, "printer-state", &3i32.to_be_bytes()),
            (0x13, "printer-alert", b""),
        ]);
        let (status, attrs) = parse(&body).unwrap();
        assert_eq!(status, 1);
        assert_eq!(attrs.text("printer-info"), Some("Front desk"));
        assert_eq!(attrs.ints("printer-state").collect::<Vec<_>>(), [3]);
        assert_eq!(attrs.ints("x-dimension").count(), 0, "a collection's members aren't attributes");
        assert_eq!(attrs.values("media-col-default").collect::<Vec<_>>(), [&Val::Other]);
        assert_eq!(attrs.model().as_deref(), Some("Front desk"));
        // Cut anywhere, it is refused rather than misread.
        for end in 0..body.len() {
            assert!(parse(&body[..end]).is_none(), "cut at {end}");
        }
    }

    #[test]
    fn a_request_says_what_it_asks_of_which_printer() {
        let mut req = Request::new(GET_PRINTER_ATTRIBUTES, "ipp://10.0.0.9:631/ipp/print");
        req.keywords("requested-attributes", WANTED);
        let (op, attrs) = parse(&req.finish()).unwrap();
        assert_eq!(op, GET_PRINTER_ATTRIBUTES);
        assert_eq!(attrs.text("printer-uri"), Some("ipp://10.0.0.9:631/ipp/print"));
        assert_eq!(attrs.texts("requested-attributes").collect::<Vec<_>>(), WANTED);
    }

    #[test]
    fn a_summary_says_state_supplies_and_trouble() {
        let (_, attrs) = parse(&inkjet(GET_PRINTER_ATTRIBUTES)).unwrap();
        assert_eq!(attrs.summary(), "printing · black 60%, cyan 8% (low) · out of paper, ink low");
        let (_, bare) = parse(&answer(0, &[(0x23, "printer-state", &3i32.to_be_bytes()), (0x44, "printer-state-reasons", b"none")])).unwrap();
        assert_eq!(bare.summary(), "idle");
    }

    #[test]
    fn reasons_are_words() {
        assert_eq!(reason("media-jam-error").as_deref(), Some("paper jam"));
        assert_eq!(reason("toner-low-report").as_deref(), Some("toner low"));
        assert_eq!(reason("none"), None);
        assert_eq!(reason("fuser-over-temp-warning").as_deref(), Some("fuser over temp"));
    }

    #[test]
    fn an_announcement_is_a_printer_unless_a_computer_shares_it() {
        let txt = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| *v);
        let ip = Ipv4Addr::new(10, 0, 0, 9);
        let f = announced(ip, 631, txt(&[("rp", "ipp/print"), ("ty", "Brother MFC-L2710DW"), ("UUID", "E3248000-80CE-11DB-8000-30055C773BCF")])).unwrap();
        assert_eq!(f.mac.as_deref(), Some("e3248000-80ce-11db-8000-30055c773bcf"), "matches what IPP says, as urn:uuid:");
        assert_eq!(f.mac.as_deref(), Some(uuid_of("urn:uuid:E3248000-80CE-11DB-8000-30055C773BCF").as_str()));
        assert_eq!(f.data, json!({"port": 631, "path": "ipp/print", "model": "Brother MFC-L2710DW"}));
        assert!(announced(ip, 631, txt(&[("rp", "printers/Office"), ("printer-type", "0x809046"), ("UUID", "x")])).is_none());
    }

    #[test]
    fn a_printer_is_checked_named_and_tested_over_ipp() {
        let (port, asked) = printer(inkjet);
        let mut d = device(port);
        assert_eq!(Ipp.check(&d), (State::Ready, "printing · black 60%, cyan 8% (low) · out of paper, ink low".into()));
        assert_eq!(Ipp.test(&mut d), Ok("asked Office to show itself".into()));
        let ops: Vec<u16> = asked.lock().unwrap().iter().map(|(op, _)| *op).collect();
        assert_eq!(ops, [GET_PRINTER_ATTRIBUTES, GET_PRINTER_ATTRIBUTES, IDENTIFY_PRINTER]);
        let asked = asked.lock().unwrap();
        assert_eq!(asked[2].1.text("printer-uri"), Some(format!("ipp://127.0.0.1:{port}/ipp/print").as_str()));
    }

    #[test]
    fn a_printer_that_cant_identify_itself_says_so_and_is_never_sent_the_request() {
        let (port, asked) = printer(|_| answer(0, &[(0x23, "operations-supported", &0x000Bi32.to_be_bytes())]));
        let err = Ipp.test(&mut device(port)).unwrap_err();
        assert!(err.contains("can't identify itself"), "{err}");
        assert!(asked.lock().unwrap().iter().all(|(op, _)| *op == GET_PRINTER_ATTRIBUTES));
    }

    #[test]
    fn a_refusal_or_silence_is_not_a_printer() {
        let (port, _) = printer(|_| answer(0x0400, &[]));
        assert!(printer_err(port).contains("refused"));
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        assert_eq!(Ipp.check(&device(closed)).0, State::Offline);
    }

    #[test]
    fn a_printer_that_only_speaks_ipp_1_1_is_asked_again_in_1_1() {
        let (port, asked) = speaking(|version, op| if version == IPP_2_0 { answer(VERSION_NOT_SUPPORTED, &[]) } else { inkjet(op) });
        assert_eq!(Ipp.check(&device(port)), (State::Ready, "printing · black 60%, cyan 8% (low) · out of paper, ink low".into()));
        assert_eq!(asked.lock().unwrap().len(), 2, "once in 2.0, once in 1.1");
        let (port, asked) = speaking(|_, _| answer(VERSION_NOT_SUPPORTED, &[]));
        assert!(printer_err(port).contains("refused"), "one that wants neither is refused, not asked forever");
        assert_eq!(asked.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_redirect_is_not_followed() {
        // Where the redirect points: anything connecting at all is a failure.
        let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let elsewhere = target.local_addr().unwrap().port();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader, Read, Write};
            let Ok((stream, _)) = listener.accept() else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut length = 0;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some((k, v)) = line.split_once(':')
                    && k.eq_ignore_ascii_case("content-length")
                {
                    length = v.trim().parse().unwrap();
                }
            }
            reader.read_exact(&mut vec![0; length]).unwrap();
            // 303 is the one a client follows as a GET, body or not.
            let moved = format!("HTTP/1.1 303 See Other\r\nLocation: http://127.0.0.1:{elsewhere}/x\r\nContent-Length: 0\r\n\r\n");
            (&stream).write_all(moved.as_bytes()).unwrap();
        });
        assert!(Ipp.status(&device(port)).is_err());
        target.set_nonblocking(true).unwrap();
        assert!(target.accept().is_err(), "home never went where the redirect pointed");
    }

    #[test]
    fn a_reason_said_twice_is_said_once_even_apart() {
        let (_, attrs) = parse(&answer(0, &[
            (0x23, "printer-state", &3i32.to_be_bytes()),
            (0x44, "printer-state-reasons", b"media-empty-error"),
            (0x44, "", b"marker-supply-low-warning"),
            (0x44, "", b"media-needed-warning"),
        ]))
        .unwrap();
        assert_eq!(attrs.summary(), "idle · out of paper, ink low");
    }

    #[test]
    fn a_supply_with_no_level_is_left_out() {
        let (_, attrs) = parse(&answer(0, &[
            (0x23, "printer-state", &3i32.to_be_bytes()),
            (0x41, "marker-names", b"Black"),
            (0x41, "", b"Cyan"),
            (0x21, "marker-levels", &50i32.to_be_bytes()),
        ]))
        .unwrap();
        assert_eq!(attrs.summary(), "idle · black 50%");
    }

    #[test]
    fn an_announcement_with_no_uuid_is_completed_by_asking_the_printer() {
        let (port, _) = printer(inkjet);
        let ip = Ipv4Addr::LOCALHOST;
        let f = announced(ip, port, |k| (k == "rp").then_some("ipp/print")).unwrap();
        assert_eq!(f.mac, None, "an announcement alone doesn't wait on the printer");
        let f = asked_about(f);
        assert_eq!(f.mac.as_deref(), Some("abcd-1234"), "the same form a TXT record's UUID takes");
        assert_eq!(f.data["model"], "Acme InkJet 3000");
        assert_eq!(f.checked, Some((State::Ready, "printing · black 60%, cyan 8% (low) · out of paper, ink low".into())));
        // One that doesn't answer is still offered, as it was announced.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let f = asked_about(announced(ip, closed, |_| None).unwrap());
        assert_eq!((f.mac, f.checked), (None, None));
    }

    /// Lists the printers on this network and what each says: run with
    /// `cargo test scan_for_printers -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs a printer on the network"]
    fn scan_for_printers() {
        let found = std::sync::Mutex::new(Vec::new());
        Ipp.discover(&|f| found.lock().unwrap().push(f));
        for f in found.into_inner().unwrap() {
            let d = Device { mac: f.mac.clone(), ..Device::new("ipp", "found".into(), f.ip, None, f.data.clone()) };
            println!("{} {:?} {} → {:?}", f.ip, f.mac, f.data, Ipp.check(&d));
        }
    }

    fn printer_err(port: u16) -> String {
        Ipp.status(&device(port)).unwrap_err()
    }
}
