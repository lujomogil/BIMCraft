//! `bimcraft-cli mcp --live`: the agent's MCP stdio relayed, line by line, to the desktop's live link, so what it builds
//! is built in the open window. With no window listening it serves headless, as `bimcraft-cli mcp` does, and says so on
//! stderr: an agent configured for the live link still has BIMCraft.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// How long to wait for a window to answer the connection.
const CONNECT: Duration = Duration::from_secs(2);

/// The window's live link at `addr`, if one listens.
pub fn connect(addr: SocketAddr) -> Option<TcpStream> {
    TcpStream::connect_timeout(&addr, CONNECT).ok()
}

/// Relay `input` to the window and its replies to `output` until either side closes. Each line goes on its own, and its
/// reply (none for a notification) is written before the next is read, as the stdio transport has it.
pub fn relay(stream: TcpStream, input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
    let mut to = stream.try_clone()?;
    let mut from = BufReader::new(stream);
    for line in input.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        writeln!(to, "{trimmed}")?;
        to.flush()?;
        if !expects_reply(trimmed) {
            continue;
        }
        let mut reply = String::new();
        if from.read_line(&mut reply)? == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "the BIMCraft window closed the live link"));
        }
        output.write_all(reply.as_bytes())?;
        output.flush()?;
    }
    Ok(())
}

/// Whether the window answers a line: a request (or a batch holding one, or a line that doesn't parse, which it answers
/// with an error); never a notification or a response.
fn expects_reply(line: &str) -> bool {
    // As `mcp::reply` answers: a request with an id, or anything that isn't a message, gets one.
    let request = |m: &serde_json::Value| match (m.as_object(), m.get("method").is_some()) {
        (None, _) => true,
        (Some(_), true) => m.get("id").is_some(),
        (Some(o), false) => !(o.contains_key("result") || o.contains_key("error")),
    };
    match serde_json::from_str::<serde_json::Value>(line) {
        Ok(serde_json::Value::Array(batch)) => batch.is_empty() || batch.iter().any(request),
        Ok(msg) => request(&msg),
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bimcraft_core::{engine::Session, mcp};
    use serde_json::{Value, json};
    use std::net::TcpListener;

    /// A stand-in for the window: answers each line with `mcp::reply` on a session of its own.
    fn window() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut out = stream.try_clone().unwrap();
            let mut session = Session::new();
            for line in BufReader::new(stream).lines() {
                if let Some(r) = mcp::reply(&mut session, &line.unwrap()) {
                    writeln!(out, "{r}").unwrap();
                }
            }
        });
        addr
    }

    #[test]
    fn the_agent_talks_to_the_window_through_the_relay() {
        let stream = connect(window()).expect("the window listens");
        let input = [
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "execute",
                "arguments": {"command": "wall", "params": {"points": [[0, 0], [5000, 0]]}}}}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "inspect_model", "arguments": {}}}),
        ]
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
        let mut out = Vec::new();
        relay(stream, input.as_bytes(), &mut out).unwrap();
        let replies: Vec<Value> = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(replies.iter().map(|r| r["id"].as_u64()).collect::<Vec<_>>(), [Some(1), Some(2), Some(3)]);
        assert_eq!(replies[0]["result"]["serverInfo"]["name"], "bimcraft");
        let model: Value = serde_json::from_str(replies[2]["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(model["counts"]["wall"], 1);
    }

    #[test]
    fn nothing_listening_is_no_connection() {
        let free = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        assert!(connect(free).is_none());
    }

    #[test]
    fn notifications_and_responses_wait_for_no_reply() {
        assert!(expects_reply(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#));
        assert!(!expects_reply(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#));
        assert!(!expects_reply(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#));
        assert!(expects_reply(r#"{"jsonrpc":"2.0","id":1}"#));
        assert!(expects_reply("{not json"));
        assert!(expects_reply("[]"));
        assert!(!expects_reply(r#"[{"jsonrpc":"2.0","method":"notifications/x"}]"#));
    }
}
