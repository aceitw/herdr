use std::io::{self, BufRead as _, Write};
use std::os::fd::AsRawFd as _;
use std::os::unix::net::UnixStream;

pub(crate) fn run_api_client_bridge(args: &[String]) -> io::Result<()> {
    let encoded_request = match args {
        [] => None,
        [encoded] => Some(encoded.as_str()),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "api-bridge accepts at most one encoded request",
            ));
        }
    };
    let request_line = match encoded_request {
        Some(encoded) => decode_request_arg(encoded)?,
        None => {
            let mut line = String::new();
            if io::stdin().lock().read_line(&mut line)? == 0 {
                return Ok(());
            }
            line.trim_end_matches(['\r', '\n']).to_owned()
        }
    };
    if request_line.trim().is_empty() {
        return Ok(());
    }

    let socket_path = crate::api::socket_path();
    let conn = match UnixStream::connect(&socket_path) {
        Ok(conn) => conn,
        Err(err) => {
            let mut stdout = io::stdout().lock();
            return emit_transport_error(&mut stdout, &request_line, &err);
        }
    };

    // A round-trip client closes the SSH channel after reading its response, not
    // before. Wake subscriptions and other long-lived reads when stdout loses
    // its peer without treating stdin's normal half-close as cancellation.
    if let Ok(teardown) = conn.try_clone() {
        let output_fd = io::stdout().as_raw_fd();
        std::thread::spawn(move || wait_for_output_hangup_then_shutdown(output_fd, teardown));
    }

    let mut stdout = io::stdout().lock();
    send_and_stream(conn, &request_line, &mut stdout)
}

fn wait_for_output_hangup_then_shutdown(output_fd: std::os::fd::RawFd, teardown: UnixStream) {
    let mut poll_fd = libc::pollfd {
        fd: output_fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        poll_fd.revents = 0;
        let result = unsafe { libc::poll(&mut poll_fd, 1, -1) };
        if result < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if poll_fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            let _ = teardown.shutdown(std::net::Shutdown::Both);
            return;
        }
    }
}

fn decode_request_arg(encoded: &str) -> io::Result<String> {
    use base64::Engine as _;

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
    let request =
        String::from_utf8(bytes).map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
    Ok(request.trim_end_matches(['\r', '\n']).to_owned())
}

fn send_and_stream<W: Write>(
    mut conn: UnixStream,
    request_line: &str,
    out: &mut W,
) -> io::Result<()> {
    if let Err(err) = conn
        .write_all(request_line.as_bytes())
        .and_then(|()| conn.write_all(b"\n"))
        .and_then(|()| conn.flush())
    {
        return emit_transport_error(out, request_line, &err);
    }

    for reply in io::BufReader::new(conn).lines() {
        match reply {
            Ok(line) => {
                out.write_all(line.as_bytes())?;
                out.write_all(b"\n")?;
                out.flush()?;
            }
            Err(err) => return emit_transport_error(out, request_line, &err),
        }
    }
    Ok(())
}

fn emit_transport_error<W: Write>(
    out: &mut W,
    request_line: &str,
    err: &io::Error,
) -> io::Result<()> {
    let id = serde_json::from_str::<serde_json::Value>(request_line)
        .ok()
        .and_then(|value| {
            value
                .get("id")
                .and_then(|id| id.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_default();
    let envelope = serde_json::json!({
        "id": id,
        "error": {
            "code": "transport_error",
            "message": format!("api-bridge: {err}"),
        }
    });
    writeln!(out, "{envelope}")?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HerdrUp base64-encodes the whole request line into argv chunks because no
    /// JSON metacharacter may need shell quoting over SSH. The decoder must also
    /// accept the trailing newline a login shell can leave on the argument.
    #[test]
    fn decode_request_arg_returns_the_trimmed_request_line() {
        use base64::Engine as _;

        let request = r#"{"id":"bridge-1","method":"ping","params":{}}"#;
        let encoded = base64::engine::general_purpose::STANDARD.encode(request);
        assert_eq!(decode_request_arg(&encoded).unwrap(), request);
        let with_newline =
            base64::engine::general_purpose::STANDARD.encode(format!("{request}\r\n"));
        assert_eq!(decode_request_arg(&with_newline).unwrap(), request);
    }

    #[test]
    fn decode_request_arg_rejects_invalid_base64() {
        let result = decode_request_arg("not base64 at all");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn decode_request_arg_rejects_non_utf8_payload() {
        use base64::Engine as _;

        let encoded = base64::engine::general_purpose::STANDARD.encode([0xff, 0xfe]);
        assert!(decode_request_arg(&encoded).is_err());
    }

    /// HerdrUp's connect classifier keys on this exact envelope: a `transport_error`
    /// code whose message starts with `api-bridge: ` and carries a recognizable OS
    /// error for the daemon socket. The request id must survive into the reply so
    /// the app can correlate the failure instead of dropping it as unparseable.
    #[test]
    fn transport_error_envelope_correlates_the_request_id() {
        let request_line = r#"{"id":"req-42","method":"agent.list","params":{}}"#;

        // A path under a nonexistent directory is a stable ENOENT ("os error 2"),
        // which is exactly what distinguishes absent daemon from refused socket.
        let missing_dir =
            std::env::temp_dir().join(format!("herdr-bridge-test-{}-missing", std::process::id()));
        let err = UnixStream::connect(missing_dir.join("api.sock"))
            .expect_err("connecting into a missing directory must fail");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);

        let mut out: Vec<u8> = Vec::new();
        emit_transport_error(&mut out, request_line, &err).unwrap();

        let rendered = String::from_utf8(out).unwrap();
        let envelope: serde_json::Value = serde_json::from_str(rendered.trim()).unwrap();
        assert_eq!(envelope["id"], "req-42");
        assert_eq!(envelope["error"]["code"], "transport_error");
        let message = envelope["error"]["message"].as_str().unwrap();
        assert!(message.starts_with("api-bridge: "), "{message}");
        assert!(
            message.contains("No such file or directory"),
            "the app's daemonUnavailable classifier matches the OS error text: {message}"
        );
    }

    #[test]
    fn transport_error_envelope_survives_an_unparseable_request() {
        let err = io::Error::from_raw_os_error(2);
        let mut out: Vec<u8> = Vec::new();
        emit_transport_error(&mut out, "not json at all", &err).unwrap();

        let envelope: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(envelope["id"], "");
        assert_eq!(envelope["error"]["code"], "transport_error");
    }

    /// One request line in, every reply line out, in order, until the daemon side
    /// closes. Subscriptions (HerdrUp's live terminal and agent events) ride this
    /// same path, so ordering and multi-line replies are the contract, not
    /// incidental.
    #[test]
    fn send_and_stream_forwards_requests_and_streams_replies_in_order() {
        let (client_a, mut server_a) = UnixStream::pair().unwrap();

        let replies = std::thread::spawn(move || {
            let mut reader = io::BufReader::new(&mut server_a);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(line.trim_end_matches('\n'), r#"{"id":"x","method":"ping"}"#);
            writeln!(server_a, r#"{{"id":"x","result":"pong"}}"#).unwrap();
            writeln!(server_a, r#"{{"id":"x","result":"pong"}}"#).unwrap();
        });

        let conserved_request = r#"{"id":"x","method":"ping"}"#;
        let mut out: Vec<u8> = Vec::new();
        send_and_stream(client_a, conserved_request, &mut out).unwrap();
        replies.join().unwrap();

        let rendered = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 2);
    }
}
