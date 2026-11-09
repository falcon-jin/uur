use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::config::Config;
use crate::input;
use crate::protocol::{self, RECORD_BYTES, RECORD_HELLO};

struct SharedInput {
    backend: Box<dyn input::InputBackend>,
    keys: HashMap<u16, usize>,
    buttons: HashMap<u16, usize>,
}

struct ConnectionInput {
    shared: Arc<Mutex<SharedInput>>,
    name: &'static str,
    keys: HashSet<u16>,
    buttons: HashSet<u16>,
}

impl ConnectionInput {
    fn press(&mut self, code: u16, down: bool, keyboard: bool) -> Result<()> {
        let mut shared = self.shared.lock().unwrap();
        let held = if keyboard {
            &mut self.keys
        } else {
            &mut self.buttons
        };
        let owners = if keyboard {
            &shared.keys
        } else {
            &shared.buttons
        };
        let count = owners.get(&code).copied().unwrap_or(0);
        if !down && !held.contains(&code) {
            return Ok(());
        }
        if down || count <= 1 {
            if keyboard {
                shared.backend.key(code, down)?;
            } else {
                shared.backend.button(code, down)?;
            }
        }
        let owners = if keyboard {
            &mut shared.keys
        } else {
            &mut shared.buttons
        };
        if down {
            if held.insert(code) {
                *owners.entry(code).or_default() += 1;
            }
        } else {
            held.remove(&code);
            if count <= 1 {
                owners.remove(&code);
            } else {
                owners.insert(code, count - 1);
            }
        }
        Ok(())
    }
}

impl input::InputBackend for ConnectionInput {
    fn name(&self) -> &'static str {
        self.name
    }
    fn key(&mut self, code: u16, down: bool) -> Result<()> {
        self.press(code, down, true)
    }
    fn button(&mut self, code: u16, down: bool) -> Result<()> {
        self.press(code, down, false)
    }
    fn motion(&mut self, absolute: bool, x: i32, y: i32) -> Result<()> {
        self.shared.lock().unwrap().backend.motion(absolute, x, y)
    }
    fn wheel(&mut self, horizontal: bool, delta: i32) -> Result<()> {
        self.shared.lock().unwrap().backend.wheel(horizontal, delta)
    }
    fn release_all(&mut self) -> Result<()> {
        let mut error = None;
        for code in self.keys.clone() {
            if let Err(e) = self.key(code, false) {
                error = Some(e);
            }
        }
        for code in self.buttons.clone() {
            if let Err(e) = self.button(code, false) {
                error = Some(e);
            }
        }
        // A disconnected client must not retain ownership if a release fails.
        let mut shared = self.shared.lock().unwrap();
        for code in self.keys.drain() {
            if let Some(count) = shared.keys.get_mut(&code) {
                *count -= 1;
                if *count == 0 {
                    shared.keys.remove(&code);
                }
            }
        }
        for code in self.buttons.drain() {
            if let Some(count) = shared.buttons.get_mut(&code) {
                *count -= 1;
                if *count == 0 {
                    shared.buttons.remove(&code);
                }
            }
        }
        error.map_or(Ok(()), Err)
    }
}

/// Print the X11 pointer position — the before/after observable for hook
/// verification.
pub fn print_pointer() -> Result<()> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt;
    let (conn, screen) = x11rb::connect(None).context("connecting to X display")?;
    let root = conn.setup().roots[screen].root;
    let reply = conn.query_pointer(root)?.reply()?;
    println!("{} {}", reply.root_x, reply.root_y);
    Ok(())
}

/// Serve authenticated loopback hook connections, serializing individual
/// input events without holding the backend lock while waiting for records.
pub fn serve() -> Result<()> {
    let config = Config::load()?;
    let mut preferred =
        std::env::var("UUR_INPUT_BACKEND").unwrap_or_else(|_| config.input_backend.clone());
    if config.capture_backend == "wayland" {
        if preferred == "auto" {
            preferred = "uinput".into();
        }
        if preferred == "portal" {
            anyhow::bail!("direct Wayland capture uses uinput; choose UUR_INPUT_BACKEND=uinput or return to portal capture");
        }
    }
    let selected = input::select(&preferred)?;
    if let Some(path) = std::env::var_os("UUR_BACKEND_STATUS") {
        std::fs::write(path, selected.name()).context("writing backend status")?;
    }
    let backend_name = selected.name();
    let backend = Arc::new(Mutex::new(SharedInput {
        backend: selected,
        keys: HashMap::new(),
        buttons: HashMap::new(),
    }));
    let backend_handle = backend.clone();
    let addr = format!("127.0.0.1:{}", config.bridge_port);
    let listener = TcpListener::bind(&addr).with_context(|| format!("binding {addr}"))?;
    listener.set_nonblocking(true)?;
    println!("{}", t!("bridge.listening", port = config.bridge_port));

    // The service process hooks itself too, so multiple authenticated
    // connections coexist; each gets its own thread.  Injection is
    // serialized through the single backend.
    loop {
        backend.lock().unwrap().backend.check_health()?;
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::ConnectionAborted
                ) =>
            {
                continue
            }
            Err(error) => return Err(error).context("accepting hook connection"),
        };
        let config = config.clone();
        let backend_handle = backend_handle.clone();
        thread::spawn(move || {
            let mut stream = stream;
            if stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .is_err()
            {
                return;
            }
            match authenticate(&mut stream, &config) {
                Ok(true) => {}
                Ok(false) => return,
                Err(_) => return,
            }
            if stream.set_read_timeout(None).is_err() {
                return;
            }
            use input::InputBackend;
            // Read sockets without holding the shared desktop input lock.
            let mut connection = ConnectionInput {
                shared: backend_handle,
                name: backend_name,
                keys: HashSet::new(),
                buttons: HashSet::new(),
            };
            println!("{}", t!("bridge.connected", backend = connection.name()));
            match pump(&mut stream, &mut connection) {
                Ok(count) => {
                    println!("{}", t!("bridge.session_summary", count = count));
                    println!("{}", t!("bridge.disconnected_clean"));
                }
                Err(error) => {
                    eprintln!(
                        "{}",
                        t!("bridge.disconnected_error", error = error.to_string())
                    );
                }
            }
            let _ = connection.release_all();
        });
    }
}

fn authenticate(stream: &mut TcpStream, config: &Config) -> Result<bool> {
    let mut header = [0u8; protocol::HEADER_BYTES];
    stream.read_exact(&mut header).context("reading header")?;
    let header = protocol::read_header(&header);
    if header.magic != protocol::MAGIC
        || header.version != protocol::VERSION
        || header.kind != RECORD_HELLO
    {
        return Ok(false);
    }
    let expected: [u8; 32] = decode_token(&config.bridge_token);
    let mut token = [0u8; 32];
    stream.read_exact(&mut token)?;
    if tokens_differ(&token, &expected) {
        let shown: String = token[..4].iter().map(|b| format!("{b:02x}")).collect();
        eprintln!("{}", t!("bridge.bad_token_fingerprint", prefix = shown));
        return Ok(false);
    }
    Ok(true)
}

fn pump(stream: &mut TcpStream, backend: &mut dyn input::InputBackend) -> Result<u64> {
    let mut header = [0u8; protocol::HEADER_BYTES];
    let mut record = [0u8; RECORD_BYTES];
    let mut injected: u64 = 0;
    let mut warned_unsupported_key = false;
    loop {
        // EOF is the hook exiting: a clean disconnect, not an error.
        match stream.read_exact(&mut header) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(injected);
            }
            Err(error) => return Err(error.into()),
        }
        let header = protocol::read_header(&header);
        if header.magic != protocol::MAGIC {
            anyhow::bail!("bad magic");
        }
        match header.kind {
            protocol::RECORD_MOUSE | protocol::RECORD_KEYBOARD => {
                stream.read_exact(&mut record)?;
                let record = protocol::read_record(&record);
                if record.kind == protocol::RECORD_KEYBOARD && record.code == 0 {
                    // Unicode and scan-code SendInput events do not carry a
                    // virtual-key code. Text/scan-code transport is not
                    // supported yet, but one such event must not tear down
                    // the connection and block later ordinary key events.
                    if !warned_unsupported_key {
                        eprintln!(
                            "ignoring keyboard input without a virtual-key code; Unicode and scan-code input are not supported yet"
                        );
                        warned_unsupported_key = true;
                    }
                    continue;
                }
                dispatch(backend, &record)?;
                injected += 1;
            }
            _ => {
                // Unknown record: skip its payload if any, stay in sync.
                for _ in 0..(header.length / RECORD_BYTES as u32) {
                    stream.read_exact(&mut record)?;
                }
            }
        }
    }
}

fn dispatch(backend: &mut dyn input::InputBackend, record: &protocol::Record) -> Result<()> {
    match record.kind {
        protocol::RECORD_KEYBOARD => backend.key(record.code, record.state == 1)?,
        protocol::RECORD_MOUSE => {
            // a/b carry motion deltas or absolute coordinates; the wheel
            // selector reuses `code`.
            const MOTION: u16 = 0xffff;
            const WHEEL: u16 = 0xfffe;
            match record.code {
                MOTION => backend.motion(record.state == 1, record.a, record.b)?,
                WHEEL => backend.wheel(record.a != 0, record.b)?,
                _ => backend.button(record.code, record.state == 1)?,
            }
        }
        _ => {}
    }
    Ok(())
}

fn decode_token(token: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (index, chunk) in token.as_bytes().chunks(2).take(32).enumerate() {
        if let Ok(value) = u8::from_str_radix(std::str::from_utf8(chunk).unwrap_or("0"), 16) {
            out[index] = value;
        }
    }
    out
}

/// Branch-free comparison; the token never leaves loopback but avoid
/// handing timing data to same-user processes anyway.
fn tokens_differ(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::InputBackend;
    use std::io::Write;
    use std::sync::mpsc;
    use std::time::Duration;

    struct Recorder(mpsc::Sender<String>);
    impl InputBackend for Recorder {
        fn name(&self) -> &'static str {
            "test"
        }
        fn key(&mut self, code: u16, down: bool) -> Result<()> {
            if code == 0 {
                anyhow::bail!("invalid virtual-key code");
            }
            self.0.send(format!("key {code} {down}"))?;
            Ok(())
        }
        fn button(&mut self, code: u16, down: bool) -> Result<()> {
            self.0.send(format!("button {code} {down}"))?;
            Ok(())
        }
        fn motion(&mut self, _: bool, _: i32, _: i32) -> Result<()> {
            self.0.send("motion".into())?;
            Ok(())
        }
        fn wheel(&mut self, _: bool, _: i32) -> Result<()> {
            Ok(())
        }
        fn release_all(&mut self) -> Result<()> {
            panic!("must release only owned inputs")
        }
    }

    fn connection(shared: &Arc<Mutex<SharedInput>>) -> ConnectionInput {
        ConnectionInput {
            shared: shared.clone(),
            name: "test",
            keys: HashSet::new(),
            buttons: HashSet::new(),
        }
    }

    fn shared() -> (Arc<Mutex<SharedInput>>, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel();
        (
            Arc::new(Mutex::new(SharedInput {
                backend: Box::new(Recorder(tx)),
                keys: HashMap::new(),
                buttons: HashMap::new(),
            })),
            rx,
        )
    }

    fn input_packet(kind: u32, code: u16, state: u16) -> Vec<u8> {
        let mut packet = Vec::new();
        for value in [protocol::MAGIC, protocol::VERSION, kind, 16, kind] {
            packet.extend_from_slice(&value.to_le_bytes());
        }
        packet.extend_from_slice(&code.to_le_bytes());
        packet.extend_from_slice(&state.to_le_bytes());
        packet.extend_from_slice(&0i32.to_le_bytes());
        packet.extend_from_slice(&0i32.to_le_bytes());
        packet
    }

    #[test]
    fn zero_virtual_key_does_not_disconnect_later_keyboard_input() {
        let (shared, events) = shared();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let mut backend = connection(&shared);
        let thread = thread::spawn(move || pump(&mut server, &mut backend).unwrap());

        client
            .write_all(&input_packet(protocol::RECORD_KEYBOARD, 0, 1))
            .unwrap();
        client
            .write_all(&input_packet(protocol::RECORD_KEYBOARD, 65, 1))
            .unwrap();
        drop(client);

        assert_eq!(thread.join().unwrap(), 1);
        assert_eq!(events.try_iter().collect::<Vec<_>>(), ["key 65 true"]);
    }

    #[test]
    fn idle_connection_does_not_block_another_connections_motion() {
        let (shared, events) = shared();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let mut idle = connection(&shared);
        let thread = thread::spawn(move || pump(&mut server, &mut idle).unwrap());
        let mut packet = Vec::new();
        for value in [
            protocol::MAGIC,
            1,
            protocol::RECORD_MOUSE,
            16,
            protocol::RECORD_MOUSE,
        ] {
            packet.extend_from_slice(&value.to_le_bytes());
        }
        packet.extend_from_slice(&0xffffu16.to_le_bytes());
        packet.extend_from_slice(&0u16.to_le_bytes());
        packet.extend_from_slice(&1i32.to_le_bytes());
        packet.extend_from_slice(&0i32.to_le_bytes());
        client.write_all(&packet).unwrap();
        assert_eq!(
            events.recv_timeout(Duration::from_secs(2)).unwrap(),
            "motion"
        );
        let mut other = connection(&shared);
        let other_thread = thread::spawn(move || other.motion(false, 1, 0).unwrap());
        let delivered = events.recv_timeout(Duration::from_secs(2));
        drop(client);
        thread.join().unwrap();
        other_thread.join().unwrap();
        assert_eq!(delivered.unwrap(), "motion");
    }

    #[test]
    fn disconnect_preserves_keys_and_buttons_held_by_another_connection() {
        let (shared, events) = shared();
        let mut first = connection(&shared);
        let mut second = connection(&shared);
        first.key(65, true).unwrap();
        second.key(65, true).unwrap();
        first.button(1, true).unwrap();
        second.button(1, true).unwrap();
        first.release_all().unwrap();
        assert_eq!(
            events.try_iter().collect::<Vec<_>>(),
            [
                "key 65 true",
                "key 65 true",
                "button 1 true",
                "button 1 true"
            ]
        );
        second.release_all().unwrap();
        assert_eq!(
            events.try_iter().collect::<Vec<_>>(),
            ["key 65 false", "button 1 false"]
        );
    }

    #[test]
    fn disconnect_forgets_ownership_when_backend_release_fails() {
        let (shared, events) = shared();
        let mut client = connection(&shared);
        client.key(65, true).unwrap();
        client.button(1, true).unwrap();
        drop(events);
        assert!(client.release_all().is_err());
        assert!(client.keys.is_empty());
        assert!(client.buttons.is_empty());
        let shared = shared.lock().unwrap();
        assert!(shared.keys.is_empty());
        assert!(shared.buttons.is_empty());
    }

    #[test]
    fn repeated_press_is_released_once_and_unowned_release_is_ignored() {
        let (shared, events) = shared();
        let mut first = connection(&shared);
        let mut second = connection(&shared);
        first.key(65, true).unwrap();
        first.key(65, true).unwrap();
        second.key(65, false).unwrap();
        first.release_all().unwrap();
        assert_eq!(
            events.try_iter().collect::<Vec<_>>(),
            ["key 65 true", "key 65 true", "key 65 false"]
        );
        assert!(shared.lock().unwrap().keys.is_empty());
    }
}
