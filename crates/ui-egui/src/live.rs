//! The live link: an agent's MCP messages, run on the window's own session, so the model changes on screen as it is
//! built. Off unless asked for (`bimcraft --live`, or `BIMCRAFT_LIVE`); it listens on this machine only
//! (`mcp::live_addr`), and `bimcraft-cli mcp --live` relays an agent's stdio to it.
//!
//! A connection's thread reads a line, hands it to the window as a [`Job`] and wakes it; the window runs the jobs at the
//! start of its next frame ([`crate::BimApp::serve_live`]), through `mcp::reply` and so `Session::execute`, as any of its
//! own commands, and the thread writes the reply back. The session never leaves the window's thread.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

/// The longest line read: a script of a whole building fits many times over; more is no message.
pub const MAX_LINE: u64 = 64 << 20;

/// One line of the protocol, waiting for the window.
pub struct Job {
    pub line: String,
    pub reply: Sender<Option<String>>,
}

pub struct Live {
    pub addr: SocketAddr,
    jobs: Receiver<Job>,
}

impl Live {
    /// Listen on `addr`, which must be on this machine; `wake` asks the window for a frame (from another thread).
    pub fn start(addr: SocketAddr, wake: impl Fn() + Send + Sync + 'static) -> Result<Live, String> {
        if !addr.ip().is_loopback() {
            return Err(format!("the live link listens on this machine only, not on {}", addr.ip()));
        }
        let listener = TcpListener::bind(addr).map_err(|e| format!("live link on {addr}: {e}"))?;
        let addr = listener.local_addr().map_err(|e| format!("live link: {e}"))?;
        let (send, jobs) = mpsc::channel();
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);
        thread::Builder::new()
            .name("bimcraft-live".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    let (send, wake) = (send.clone(), Arc::clone(&wake));
                    // A connection that can't get a thread is dropped: the agent sees it close.
                    let _ = thread::Builder::new().name("bimcraft-live-client".into()).spawn(move || serve(stream, &send, &*wake));
                }
            })
            .map_err(|e| format!("live link: {e}"))?;
        Ok(Live { addr, jobs })
    }

    /// The lines waiting, oldest first.
    pub fn waiting(&self) -> Vec<Job> {
        self.jobs.try_iter().collect()
    }
}

/// Answer one connection, line by line, until it closes or the window goes.
fn serve(stream: TcpStream, jobs: &Sender<Job>, wake: &(dyn Fn() + Send + Sync)) {
    let Ok(mut out) = stream.try_clone() else { return };
    let mut input = BufReader::new(stream);
    loop {
        let mut line = String::new();
        match input.by_ref().take(MAX_LINE).read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) if !line.ends_with('\n') && line.len() as u64 >= MAX_LINE => {
                let _ = writeln!(
                    out,
                    r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":-32600,"message":"a message is at most {MAX_LINE} bytes"}}}}"#
                );
                return;
            }
            Ok(_) => {}
        }
        let (reply, answer) = mpsc::channel();
        if jobs.send(Job { line, reply }).is_err() {
            return;
        }
        wake();
        // The window answers every job it takes; none comes when it has closed.
        let Ok(answer) = answer.recv() else { return };
        if let Some(text) = answer
            && (writeln!(out, "{text}").is_err() || out.flush().is_err())
        {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BimApp;
    use bimcraft_core::engine::Session;
    use serde_json::{Value, json};
    use std::time::{Duration, Instant};

    /// Send `lines` over the live link from another thread, as the relay does, while the window runs its frames;
    /// the replies, in order.
    fn agent(app: &mut BimApp, lines: Vec<Value>) -> Vec<Value> {
        let addr = app.live.as_ref().map(|l| l.addr).unwrap();
        let client = thread::spawn(move || {
            let stream = TcpStream::connect(addr).unwrap();
            let mut out = stream.try_clone().unwrap();
            let mut input = BufReader::new(stream);
            lines
                .iter()
                .map(|l| {
                    writeln!(out, "{l}").unwrap();
                    let mut reply = String::new();
                    input.read_line(&mut reply).unwrap();
                    serde_json::from_str::<Value>(&reply).unwrap()
                })
                .collect::<Vec<_>>()
        });
        let start = Instant::now();
        while !client.is_finished() {
            assert!(start.elapsed() < Duration::from_secs(20), "the agent got no answer");
            app.serve_live();
            thread::sleep(Duration::from_millis(5));
        }
        client.join().unwrap()
    }

    fn call(id: u64, name: &str, arguments: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": arguments}})
    }

    fn walls(app: &BimApp) -> usize {
        app.session.model.elements.values().filter(|e| e.category() == "wall").count()
    }

    #[test]
    fn an_agent_builds_in_the_window_and_it_undoes_like_the_windows_own() {
        let mut app = BimApp::new(Session::new());
        app.live = Some(Live::start("127.0.0.1:0".parse().unwrap(), || {}).unwrap());
        let walls_in = call(1, "execute", json!({"command": "wall", "params": {"points": [[0, 0], [6000, 0], [6000, 4000]]}}));
        let replies = agent(&mut app, vec![walls_in, call(2, "inspect_model", json!({}))]);
        assert_ne!(replies[0]["result"]["isError"], json!(true), "{}", replies[0]);
        assert_eq!(walls(&app), 2, "the window's own session has the walls");
        assert!(app.dirty(), "what the agent built is unsaved work in the window");
        assert_eq!(app.session.history().len(), 1, "one undo step, as the window's Wall tool makes");
        let undone = agent(&mut app, vec![call(3, "execute", json!({"command": "undo"}))]);
        assert_ne!(undone[0]["result"]["isError"], json!(true));
        assert_eq!(walls(&app), 0);
    }

    #[test]
    fn a_new_project_starts_the_window_over_and_the_link_stays() {
        let mut app = BimApp::new(Session::new());
        app.live = Some(Live::start("127.0.0.1:0".parse().unwrap(), || {}).unwrap());
        agent(&mut app, vec![call(1, "execute", json!({"command": "level", "params": {"elevation": 6000, "name": "Roof"}}))]);
        app.sheet = crate::Sheet::Plan(app.session.model.levels().last().map(|(id, _)| *id).unwrap());
        let r = agent(&mut app, vec![call(2, "new_model", json!({"name": "Tower"}))]);
        assert_ne!(r[0]["result"]["isError"], json!(true));
        assert_eq!(app.session.model.name, "Tower");
        let first = app.session.model.levels().first().map(|(id, _)| *id).unwrap();
        assert_eq!(app.sheet, crate::Sheet::Plan(first), "the window shows the new project, not a level that's gone");
        assert!(app.live.is_some(), "starting over keeps the link");
        let again = agent(&mut app, vec![call(3, "inspect_model", json!({}))]);
        assert!(again[0]["result"].is_object());
    }

    #[test]
    fn only_this_machine_may_listen() {
        assert!(Live::start("0.0.0.0:0".parse().unwrap(), || {}).is_err());
    }
}
