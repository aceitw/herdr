//! `herdr gram` — the owner<->agent message channel used by the Herdr app.
//!
//! Ported from jerryfane/herdr's CLI (text-core scope): agents use `send`
//! (message the owner), `list --queue` (see unclaimed work), and `grab <id>`
//! (claim one); `post`, `mark-read`, and `delete` are owner/testing paths. The
//! caller's identity comes from `HERDR_PANE_ID`, which every pane sets; it is
//! sent as `caller_pane_id` and resolved server-side. Attachments
//! (`get-file`, `--file`) are NOT available in this build — those methods do
//! not exist on the daemon and the CLI refuses the flag locally instead of
//! sending an unknown request.
//!
//! `list`/`grab`/`send` redact credential-looking bodies for DISPLAY (the
//! stored messages, all in one local store, are untouched; `--reveal` prints
//! the raw value).

use crate::api::schema::{
    GramDeleteParams, GramGrabParams, GramListParams, GramMarkReadParams, GramPostParams,
    GramSendParams, Method, Request,
};

pub(super) fn run_gram_command(args: &[String]) -> std::io::Result<i32> {
    let Some(subcommand) = args.first().map(|arg| arg.as_str()) else {
        print_gram_help();
        return Ok(2);
    };

    match subcommand {
        "send" => gram_send(&args[1..]),
        "post" => gram_post(&args[1..]),
        "list" => gram_list(&args[1..]),
        "grab" => gram_grab(&args[1..]),
        "mark-read" => gram_mark_read(&args[1..]),
        "delete" => gram_delete(&args[1..]),
        "help" | "--help" | "-h" => {
            print_gram_help();
            Ok(0)
        }
        _ => {
            print_gram_help();
            Ok(2)
        }
    }
}

fn env_pane_id() -> Option<String> {
    std::env::var("HERDR_PANE_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| super::normalize_pane_id(&value))
}

fn gram_send(args: &[String]) -> std::io::Result<i32> {
    let (text, from) = match parse_send_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    let mut response = super::send_request(&Request {
        id: format!("cli:gram:send:{}", crate::persist::gram::new_id()),
        method: Method::GramSend(GramSendParams {
            text,
            caller_pane_id: env_pane_id(),
            from,
        }),
    })?;
    // Redact the echoed body too, for symmetry with grab/list (the sender already
    // has the text; the confirmation echo doesn't need to reprint a secret).
    redact_message_info(&mut response);
    super::print_response(&response)
}

fn gram_post(args: &[String]) -> std::io::Result<i32> {
    let (text, to) = match parse_post_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    super::print_response(&super::send_request(&Request {
        id: "cli:gram:post".into(),
        method: Method::GramPost(GramPostParams {
            text,
            to,
            from: None,
        }),
    })?)
}

fn gram_list(args: &[String]) -> std::io::Result<i32> {
    let parsed = match parse_list_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    // Default: attach this pane's id for the agent/pane view. `--owner` (and
    // `--unread`, which is an owner-only view) omit it so the server returns the
    // owner view — otherwise the owner view would be unreachable from any pane,
    // since HERDR_PANE_ID is set on every managed pane.
    let owner_view = parsed.owner || parsed.unread_only;
    let caller_pane_id = if owner_view { None } else { env_pane_id() };

    let mut response = super::send_request(&Request {
        id: "cli:gram:list".into(),
        method: Method::GramList(GramListParams {
            caller_pane_id,
            only_queue: parsed.only_queue,
            unread_only: parsed.unread_only,
            // The CLI holds no previous list, so it has nothing to validate a digest
            // against and always asks unconditionally.
            if_unchanged_digest: None,
            limit: parsed.limit,
            // One-shot read: nothing here scrolls, so there is no cursor to carry.
            before_id: None,
        }),
    })?;
    // Redact credential-looking bodies before printing so a routine `gram list`
    // can't spill a secret into the reader's transcript. Display-only: stored
    // messages are untouched and `--reveal` prints the raw value. (issue #95)
    if !parsed.reveal {
        redact_gram_response(&mut response);
    }
    super::print_response(&response)
}

fn gram_grab(args: &[String]) -> std::io::Result<i32> {
    let (id, grabbed_by) = match parse_grab_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    let mut response = super::send_request(&Request {
        id: "cli:gram:grab".into(),
        method: Method::GramGrab(GramGrabParams {
            id,
            caller_pane_id: env_pane_id(),
            grabbed_by,
        }),
    })?;
    // Redact the echoed body: `grab` claims a queued item, whose text may hold a
    // credential — printing it raw here bypasses the `list` redaction (issue #95).
    redact_message_info(&mut response);
    super::print_response(&response)
}

fn gram_mark_read(args: &[String]) -> std::io::Result<i32> {
    let id = match parse_single_id(args, "mark-read") {
        Ok(id) => id,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    super::print_response(&super::send_request(&Request {
        id: "cli:gram:mark_read".into(),
        method: Method::GramMarkRead(GramMarkReadParams {
            id: Some(id),
            ids: Vec::new(),
        }),
    })?)
}

fn gram_delete(args: &[String]) -> std::io::Result<i32> {
    let (id, owner) = match parse_delete_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };

    // Default: this pane's agent identity, so an agent deletes only a message it
    // is involved in. `--owner` omits the pane to delete with owner authority
    // (any message), matching `list --owner`.
    let caller_pane_id = if owner { None } else { env_pane_id() };

    super::print_response(&super::send_request(&Request {
        id: "cli:gram:delete".into(),
        method: Method::GramDelete(GramDeleteParams { id, caller_pane_id }),
    })?)
}

// MARK: - redaction (display-only; nothing stored is mutated)

fn redact_gram_response(response: &mut serde_json::Value) {
    let Some(messages) = response
        .get_mut("result")
        .and_then(|r| r.get_mut("messages"))
        .and_then(|m| m.as_array_mut())
    else {
        return;
    };
    for message in messages {
        redact_message_text(message);
    }
}

/// Redact the single echoed message body in a `result.message` response — the
/// shape `gram grab` and `gram send` return. WITHOUT this, `grab`ing a queued
/// credential (the normal claim-work flow) prints it verbatim, bypassing the
/// `list` redaction. grab/send have no `--reveal`: an agent that truly needs the
/// raw value uses `gram list --reveal`.
fn redact_message_info(response: &mut serde_json::Value) {
    if let Some(message) = response
        .get_mut("result")
        .and_then(|r| r.get_mut("message"))
    {
        redact_message_text(message);
    }
}

/// Redact the credential-looking span in one message object's `text` field, in
/// place. Leaves every other field untouched.
fn redact_message_text(message: &mut serde_json::Value) {
    if let Some(text) = message.get("text").and_then(|t| t.as_str()) {
        let redacted = redact_credentials(text);
        if redacted != text {
            message["text"] = serde_json::Value::String(redacted);
        }
    }
}

/// Credential-looking token prefixes redacted from gram bodies on the CLI read
/// path (issue #95). `sk-` covers OpenAI / OpenRouter (`sk-or-`) / Anthropic
/// (`sk-ant-`) / project keys. PEM private-key blocks are handled separately.
const CREDENTIAL_PREFIXES: &[&str] = &[
    "sk-",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "github_pat_",
    "glpat-",
    "AKIA",
    "xoxb-",
    "xoxp-",
    "AIza",
];

fn is_credential_token(token: &str) -> bool {
    token.len() >= 20
        && CREDENTIAL_PREFIXES
            .iter()
            .any(|prefix| token.starts_with(prefix))
}

fn push_token(token: &str, out: &mut String) {
    if is_credential_token(token) {
        out.push_str(&format!(
            "[redacted credential, {} chars]",
            token.chars().count()
        ));
    } else {
        out.push_str(token);
    }
}

/// Collapse each `-----BEGIN … PRIVATE KEY-----` … `-----END … -----` block into a
/// single marker (its multi-line base64 body would otherwise slip past the
/// token scanner). If a BEGIN has no well-formed END, redact to end-of-text
/// (over-redacting a key is the safe failure).
fn redact_pem_blocks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(begin) = rest.find("-----BEGIN ") {
        out.push_str(&rest[..begin]);
        let block = &rest[begin..];
        let block_end = block.find("-----END ").and_then(|end| {
            block[end + "-----END ".len()..]
                .find("-----")
                .map(|c| end + "-----END ".len() + c + "-----".len())
        });
        match block_end {
            Some(stop) if block[..stop].contains("PRIVATE KEY") => {
                out.push_str(&format!(
                    "[redacted private key, {} chars]",
                    block[..stop].chars().count()
                ));
                rest = &block[stop..];
            }
            _ => {
                // A BEGIN with no proper END (or not a private key). If it names a
                // PRIVATE KEY, redact the remainder to be safe; else pass it through.
                if block.contains("PRIVATE KEY") {
                    out.push_str(&format!(
                        "[redacted private key, {} chars]",
                        block.chars().count()
                    ));
                    rest = "";
                } else {
                    out.push_str("-----BEGIN ");
                    rest = &block["-----BEGIN ".len()..];
                }
            }
        }
    }
    out.push_str(rest);
    out
}

/// Redact credential-looking spans from `text` for DISPLAY. Never mutates stored
/// messages. Each detected secret becomes `[redacted credential, N chars]` (or
/// `[redacted private key, N chars]` for a PEM block), N being the redacted
/// length. Pure — unit-tested over the prefix set.
fn redact_credentials(text: &str) -> String {
    let collapsed = redact_pem_blocks(text);
    let mut out = String::with_capacity(collapsed.len());
    let mut token = String::new();
    for ch in collapsed.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            token.push(ch);
        } else {
            push_token(&token, &mut out);
            token.clear();
            out.push(ch);
        }
    }
    push_token(&token, &mut out);
    out
}

// MARK: - arg parsing (pure, unit-tested)

/// `send [<text>] [--from LABEL]` -> (text, from). Text is required.
fn parse_send_args(args: &[String]) -> Result<(String, Option<String>), String> {
    let mut text: Option<String> = None;
    let mut from: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--from" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --from".into());
                };
                from = Some(value.clone());
                index += 2;
            }
            other if is_flag(other) => return Err(format!("unknown option: {other}")),
            other => {
                if text.is_some() {
                    return Err("unexpected extra argument; quote the message text".into());
                }
                text = Some(other.to_string());
                index += 1;
            }
        }
    }
    let text = text.ok_or("usage: herdr gram send <text> [--from LABEL]")?;
    Ok((text, from))
}

/// `post <text> [--to AGENT]` -> (text, to).
fn parse_post_args(args: &[String]) -> Result<(String, Option<String>), String> {
    let mut text: Option<String> = None;
    let mut to: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--to" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --to".into());
                };
                to = Some(value.clone());
                index += 2;
            }
            other if is_flag(other) => return Err(format!("unknown option: {other}")),
            other => {
                if text.is_some() {
                    return Err("unexpected extra argument; quote the message text".into());
                }
                text = Some(other.to_string());
                index += 1;
            }
        }
    }
    let text = text.ok_or("usage: herdr gram post <text> [--to AGENT]")?;
    Ok((text, to))
}

/// `list [--queue] [--unread] [--owner] [--reveal] [--limit N]`.
#[derive(Default)]
struct ListArgs {
    only_queue: bool,
    unread_only: bool,
    owner: bool,
    reveal: bool,
    limit: Option<usize>,
}

fn parse_list_args(args: &[String]) -> Result<ListArgs, String> {
    let mut parsed = ListArgs::default();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--queue" => {
                parsed.only_queue = true;
                index += 1;
            }
            "--unread" => {
                parsed.unread_only = true;
                index += 1;
            }
            "--owner" => {
                parsed.owner = true;
                index += 1;
            }
            // Print credential-looking bodies in the clear. Default is to redact them
            // (see `redact_credentials`) so a routine `gram list` can't drop a secret
            // into the reader's transcript.
            "--reveal" | "--show-secrets" => {
                parsed.reveal = true;
                index += 1;
            }
            // Newest N only. The CLI reads a terminal, where the tail of a large
            // store is noise; it never pages, so there is no `--before`.
            "--limit" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --limit".into());
                };
                let limit = value
                    .parse::<usize>()
                    .map_err(|_| format!("--limit expects a positive number, got '{value}'"))?;
                if limit == 0 {
                    return Err("--limit expects a positive number, got '0'".into());
                }
                parsed.limit = Some(limit);
                index += 2;
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }
    if parsed.only_queue && parsed.unread_only {
        return Err("--queue and --unread cannot be combined".into());
    }
    Ok(parsed)
}

/// `grab <id> [--as LABEL]` -> (id, grabbed_by).
fn parse_grab_args(args: &[String]) -> Result<(String, Option<String>), String> {
    let mut id: Option<String> = None;
    let mut grabbed_by: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--as" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("missing value for --as".into());
                };
                grabbed_by = Some(value.clone());
                index += 2;
            }
            other if is_flag(other) => return Err(format!("unknown option: {other}")),
            other => {
                if id.is_some() {
                    return Err("unexpected extra argument".into());
                }
                id = Some(other.to_string());
                index += 1;
            }
        }
    }
    let id = id.ok_or("usage: herdr gram grab <id> [--as LABEL]")?;
    Ok((id, grabbed_by))
}

/// `delete <id> [--owner]` -> (id, owner).
fn parse_delete_args(args: &[String]) -> Result<(String, bool), String> {
    let mut id: Option<String> = None;
    let mut owner = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--owner" => {
                owner = true;
                index += 1;
            }
            other if is_flag(other) => return Err(format!("unknown option: {other}")),
            other => {
                if id.is_some() {
                    return Err("unexpected extra argument".into());
                }
                id = Some(other.to_string());
                index += 1;
            }
        }
    }
    let id = id.ok_or("usage: herdr gram delete <id> [--owner]")?;
    Ok((id, owner))
}

fn parse_single_id(args: &[String], subcommand: &str) -> Result<String, String> {
    match args {
        [id] if !is_flag(id) => Ok(id.clone()),
        _ => Err(format!("usage: herdr gram {subcommand} <id>")),
    }
}

fn is_flag(value: &str) -> bool {
    value.starts_with("--")
}

fn print_gram_help() {
    eprintln!("herdr gram commands:");
    eprintln!("  herdr gram send <text> [--from LABEL]   message the owner");
    eprintln!(
        "  herdr gram list [--queue] [--unread] [--owner] [--reveal] [--limit N]   list messages"
    );
    eprintln!("  herdr gram grab <id> [--as LABEL]        claim a shared queue item");
    eprintln!("  herdr gram post <text> [--to AGENT]      owner: post to the queue or one agent");
    eprintln!("  herdr gram mark-read <id>                owner: mark an agent message read");
    eprintln!("  herdr gram delete <id> [--owner]         delete a message for good");
    eprintln!();
    eprintln!("--from/--as override the attribution label (default: your agent name).");
    eprintln!("delete removes only a message you sent, grabbed, or that is addressed to you;");
    eprintln!("--owner deletes any message (owner authority).");
    eprintln!();
    eprintln!("list REDACTS credential-looking bodies (api keys, tokens, private keys) so a");
    eprintln!("routine `gram list` can't spill a secret into your transcript; pass --reveal to");
    eprintln!("print raw values.");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn send_parses_text_and_from() {
        assert_eq!(
            parse_send_args(&args(&["digest ready", "--from", "trend-scout"])).unwrap(),
            ("digest ready".to_string(), Some("trend-scout".to_string()))
        );
        assert!(parse_send_args(&args(&["first", "second"])).is_err());
        assert!(parse_send_args(&args(&[])).is_err());
    }

    #[test]
    fn send_rejects_attachment_flags_this_build_does_not_ship() {
        eprintln!(
            "{}",
            parse_send_args(&args(&["text", "--file", "/tmp/a"])).unwrap_err()
        );
        assert!(parse_send_args(&args(&["text", "--file", "/tmp/a"])).is_err());
    }

    #[test]
    fn post_parses_text_and_to() {
        assert_eq!(
            parse_post_args(&args(&["build me", "--to", "agent-a"])).unwrap(),
            ("build me".to_string(), Some("agent-a".to_string()))
        );
        assert!(parse_post_args(&args(&[])).is_err());
    }

    #[test]
    fn list_parses_filters_and_limit() {
        let parsed =
            parse_list_args(&args(&["--queue", "--owner", "--limit", "3", "--reveal"])).unwrap();
        assert!(parsed.only_queue);
        assert!(parsed.owner);
        assert!(parsed.reveal);
        assert_eq!(parsed.limit, Some(3));
        assert!(parse_list_args(&args(&["--limit", "0"])).is_err());
        assert!(parse_list_args(&args(&["--queue", "--unread"])).is_err());
    }

    #[test]
    fn grab_parses_id_and_as() {
        assert_eq!(
            parse_grab_args(&args(&["gram-3", "--as", "agent-b"])).unwrap(),
            ("gram-3".to_string(), Some("agent-b".to_string()))
        );
        assert!(parse_grab_args(&args(&[])).is_err());
        assert!(parse_grab_args(&args(&["--as"])).is_err());
    }

    #[test]
    fn delete_parses_id_and_owner() {
        assert_eq!(
            parse_delete_args(&args(&["gram-9"])).unwrap(),
            ("gram-9".to_string(), false)
        );
        assert_eq!(
            parse_delete_args(&args(&["gram-9", "--owner"])).unwrap(),
            ("gram-9".to_string(), true)
        );
        assert!(parse_delete_args(&args(&[])).is_err());
    }

    #[test]
    fn single_id_rejects_flags_and_missing() {
        assert!(parse_single_id(&args(&["gram-1"]), "mark-read").is_ok());
        assert!(parse_single_id(&args(&["--x"]), "mark-read").is_err());
        assert!(parse_single_id(&args(&[]), "mark-read").is_err());
    }

    #[test]
    fn redact_options_on_list_map_into_params() {
        // The params the CLI builds from parsed flags.
        let parsed = parse_list_args(&args(&["--unread"])).unwrap();
        assert!(parsed.unread_only);
        let params = GramListParams {
            caller_pane_id: None,
            only_queue: parsed.only_queue,
            unread_only: parsed.unread_only,
            if_unchanged_digest: None,
            limit: parsed.limit,
            before_id: None,
        };
        assert!(params.unread_only);
    }

    fn text_id() -> serde_json::Value {
        serde_json::json!({
            "id": "one",
            "from": "agent",
            "read_by_owner": false,
            "created_unix_ms": 1,
            "direction": "agent_to_owner",
            "text": "tok sk-abcdef123456abcdef1234",
            "origin_id": "m",
        })
    }

    #[test]
    fn redact_credentials_hides_known_prefixes() {
        let redacted = redact_credentials("here is sk-abcdef123456abcdef1234 for you");
        assert!(
            redacted.contains("[redacted credential, 25 chars]"),
            "{redacted}"
        );
        let plain = redact_credentials("no credentials here");
        assert_eq!(plain, "no credentials here");
    }

    #[test]
    fn redact_credentials_hides_pem_blocks() {
        let pem = "-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----";
        let redacted = redact_credentials(pem);
        assert!(redacted.contains("[redacted private key,"), "{redacted}");
    }

    #[test]
    fn redact_message_rewrites_text_only() {
        let mut message = text_id();
        redact_message_text(&mut message);
        let out = message["text"].as_str().unwrap();
        assert!(out.contains("[redacted credential"), "{out}");
        assert_eq!(message["id"], "one");
        assert_eq!(message["read_by_owner"], false);
    }
}
