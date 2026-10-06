//! Gram message handlers: the owner<->agent channel surfaced in the app.
//!
//! Ported from jerryfane/herdr so the HerdrUp iOS app can message agents over
//! this daemon's JSON API. This build carries the TEXT CORE only: `gram.send`,
//! `gram.post`, `gram.list` (with paging and the conditional digest), `gram.grab`,
//! `gram.mark_read`, and `gram.delete`. Attachments (`gram.upload_chunk`,
//! `gram.get_file`), federation relaying (`gram.relay`), guest sharing, and the
//! APNs push emit are all intentionally absent: the store keeps the
//! `file: None` shape so the wire contract matches what the app is written
//! against, and out-of-scope features fail with the daemon's `unknown_method`
//! — an honest capability boundary, not a silent shim.
//!
//! Identity and its guarantees. There is no per-connection identity, so the
//! caller passes its `HERDR_PANE_ID` as `caller_pane_id` and the server resolves
//! it (see [`App::caller_identity`]) to a single label: the agent's **name** when
//! one is set, else the pane's public id. The agent name is the durable choice —
//! it is persisted in the session snapshot and restored across a restart or a
//! live-handoff (the deploy path), which the terminal id is not. The grab is
//! first-wins atomic at the storage layer regardless of identity, so no two
//! agents can ever claim the same item; identity affects only which items a
//! caller sees as "mine" in the agent view.
//!
//! Sender/owner attribution is advisory, not authenticated — the trust domain is
//! already flat. An agent that supplies its caller pane may delete only a
//! message it is involved in; "owner" is the ABSENCE of a caller pane, so a
//! local caller that omits it acts with owner authority. This is COOPERATIVE
//! FILTERING within a flat trust domain, not authenticated isolation.

use super::responses::{encode_error, encode_success};
use crate::api::schema::{
    GramDeleteParams, GramDirection, GramFileInfo, GramGrabParams, GramListParams,
    GramMarkReadParams, GramMessageInfo, GramPostParams, GramSendParams, ResponseResult,
};
use crate::app::App;
use crate::persist::gram::{
    new_id, GramDirection as StoredDirection, GramItem, MAX_LABEL_BYTES, MAX_TEXT_BYTES,
};

/// Ceiling on a `gram.list` page. A page is meant to be one screenful plus the
/// scroll ahead of it; 500 is far past that and still an order of magnitude under
/// the store sizes that made the unpaged answer slow. Clamping (rather than
/// rejecting) keeps a client that asks for too much working.
const GRAM_LIST_MAX_LIMIT: usize = 500;

/// Why a claim could not be completed.
enum GrabError {
    NotFound,
    /// The item is not a shared, still-open queue item (direct message, wrong
    /// direction, or already claimed by name below).
    NotGrabbable,
    /// Already claimed; carries the current grabber's identity.
    AlreadyGrabbed(String),
}

/// The result of a delete attempt, decided under the store lock.
#[derive(Debug, PartialEq)]
enum DeleteOutcome {
    /// The message was removed.
    Deleted,
    /// No message with that id.
    NotFound,
    /// The message exists but the calling agent is not involved in it.
    Forbidden,
}

impl App {
    pub(super) fn handle_gram_send(&mut self, id: String, params: GramSendParams) -> String {
        let text = params.text.trim();
        // A file-only message (no caption) is fine; an empty text-only message is
        // not.
        if let Some(err) = validate_text(&id, text, false) {
            return err;
        }
        if let Some(err) = validate_label(&id, "from", params.from.as_deref()) {
            return err;
        }
        if self.no_session {
            return gram_unavailable(id);
        }

        let from = self.resolve_sender(params.from.as_deref(), params.caller_pane_id.as_deref());
        let message_id = new_id();
        let store_id = crate::persist::machine::get_or_create();
        let sender = params
            .caller_pane_id
            .as_deref()
            .and_then(|pane| self.caller_sender(pane));
        let item = GramItem {
            id: message_id,
            direction: StoredDirection::AgentToOwner,
            from: from.clone(),
            to: None,
            text: text.to_string(),
            grabbed_by: None,
            grabbed_unix_ms: None,
            created_unix_ms: super::unix_millis_now(),
            read_by_owner: false,
            file: None,
            origin_id: store_id.clone(),
            sender,
        };

        match crate::persist::gram::append(item.clone()) {
            // Push delivery is not in this build's scope — see the module header
            // for what is intentionally absent.
            Ok(_) => encode_success(
                id,
                ResponseResult::GramSent {
                    message: gram_item_to_info(item),
                    store_id,
                },
            ),
            Err(err) => encode_error(id, "gram_store_save_failed", err.to_string()),
        }
    }

    pub(super) fn handle_gram_post(&mut self, id: String, params: GramPostParams) -> String {
        self.store_gram_post(id, params, true)
    }

    fn store_gram_post(&mut self, id: String, params: GramPostParams, check_live: bool) -> String {
        let text = params.text.trim();
        if let Some(err) = validate_text(&id, text, false) {
            return err;
        }
        if let Some(err) = validate_label(&id, "to", params.to.as_deref()) {
            return err;
        }
        if self.no_session {
            return gram_unavailable(id);
        }

        let to = params
            .to
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        // A direct message must name a live agent, else it would be visible to no
        // one and never expire — a silent black hole. Omit `to` for the shared
        // queue instead.
        if let Some(target) = to.as_ref().filter(|_| check_live) {
            if !self.is_live_agent_name(target) {
                return encode_error(
                    id,
                    "invalid_params",
                    format!(
                        "no live agent named '{target}'; omit --to to post to the shared queue"
                    ),
                );
            }
        }

        let message_id = new_id();
        let store_id = crate::persist::machine::get_or_create();
        let item = GramItem {
            id: message_id,
            direction: StoredDirection::OwnerToAgent,
            from: params.from.unwrap_or_else(|| "owner".to_string()),
            to,
            text: text.to_string(),
            grabbed_by: None,
            grabbed_unix_ms: None,
            created_unix_ms: super::unix_millis_now(),
            // The owner's own message is not an unread inbox item for the owner.
            read_by_owner: true,
            file: None,
            origin_id: store_id.clone(),
            sender: None,
        };

        match crate::persist::gram::append(item.clone()) {
            Ok(_) => encode_success(
                id,
                ResponseResult::GramSent {
                    // An owner post names no sender machine.
                    message: gram_item_to_info(item),
                    store_id,
                },
            ),
            Err(err) => encode_error(id, "gram_store_save_failed", err.to_string()),
        }
    }

    pub(super) fn handle_gram_list(&mut self, id: String, params: GramListParams) -> String {
        self.handle_gram_list_for(id, params, None)
    }

    fn handle_gram_list_for(
        &mut self,
        id: String,
        params: GramListParams,
        forced_identity: Option<&str>,
    ) -> String {
        if self.no_session {
            return gram_unavailable(id);
        }
        let limit = match params.limit {
            // An explicit zero would answer "nothing here" for a store that is not
            // empty, which a scrolling reader cannot tell from the end of the list.
            // A client asking for no messages is a bug worth surfacing.
            Some(0) => {
                return encode_error(
                    id,
                    "invalid_params",
                    "limit must be greater than zero; omit it to read the whole list",
                )
            }
            // Clamped rather than rejected: an over-eager client still gets a valid,
            // bounded page instead of an error it cannot act on.
            Some(requested) => Some(requested.min(GRAM_LIST_MAX_LIMIT)),
            None => None,
        };

        let items = crate::persist::gram::load();
        let filtered = match (forced_identity, params.caller_pane_id.as_deref()) {
            // A supplied caller pane selects the agent view. Failing open to the
            // owner view (as an earlier version did) would silently drop
            // `only_queue` and return a state-dependent answer; mirror
            // `pane.current`'s pane_not_found instead. (Not a confidentiality
            // boundary — the owner view is reachable by omitting the pane.)
            (None, Some(pane)) => {
                // `unread_only` is an owner-view filter with no meaning here; reject
                // the combination rather than silently ignore it.
                if params.unread_only {
                    return encode_error(
                        id,
                        "invalid_params",
                        "unread_only is only valid in the owner view; omit caller_pane_id",
                    );
                }
                let Some(identity) = self.caller_identity(pane) else {
                    return encode_error(
                        id,
                        "unknown_caller",
                        "caller_pane_id is not a known pane; omit it to read as the owner",
                    );
                };
                filter_agent_view(&items, &identity, params.only_queue)
            }
            (None, None) => filter_owner_view(&items, params.only_queue, params.unread_only),
            (Some(identity), Some(_)) => {
                if params.unread_only {
                    return encode_error(
                        id,
                        "invalid_params",
                        "unread_only is only valid in the owner view",
                    );
                }
                filter_agent_view(&items, identity, params.only_queue)
            }
            (Some(_), None) => {
                return encode_error(id, "forbidden", "a remote caller must identify its pane")
            }
        };
        // Counted over the whole filtered list, BEFORE paging: the badge and the
        // Read-all affordance describe the inbox, not the window the client happens
        // to be holding.
        let unread_count = filtered.iter().filter(|item| is_unread(item)).count();
        // Store order is oldest-first; clients want newest-first.
        let mut messages: Vec<GramMessageInfo> =
            filtered.into_iter().rev().map(gram_item_to_info).collect();
        let store_id = crate::persist::machine::get_or_create();
        // Over the FULL list, not the page, so a paging client can keep polling the
        // head for a few hundred bytes.
        let digest = list_digest(&store_id, &messages);
        // Conditional fetch. The digest covers the store id as well as the messages, so
        // a client that has been pointed at a DIFFERENT store can never be told
        // "unchanged" while holding another store's list.
        //
        // Head-only: a request with `before_id` asks for an older page the client does
        // not hold yet, so answering "unchanged" would starve its scroll.
        if params.before_id.is_none()
            && params.if_unchanged_digest.as_deref() == Some(digest.as_str())
        {
            return encode_success(id, ResponseResult::GramListUnchanged { store_id, digest });
        }
        // Paging happens after the audience filter and after the reverse, so a cursor
        // means the same thing — "the message after this one, going older" — in the
        // owner view and the agent view alike.
        if let Some(cursor) = params.before_id.as_deref() {
            match messages.iter().position(|message| message.id == cursor) {
                Some(index) => {
                    messages.drain(..=index);
                }
                // Never fall back to the head: a client whose cursor aged out of the
                // list (deleted message, switched filter) would otherwise be handed
                // page 1 again on every scroll, forever.
                None => return encode_error(id, "invalid_params", "before_id is not in this list"),
            }
        }
        let has_more = limit.is_some_and(|limit| messages.len() > limit);
        if let Some(limit) = limit {
            messages.truncate(limit);
        }
        encode_success(
            id,
            ResponseResult::GramList {
                messages,
                store_id,
                digest,
                has_more,
                unread_count,
            },
        )
    }

    pub(super) fn handle_gram_grab(&mut self, id: String, params: GramGrabParams) -> String {
        if let Some(err) = validate_label(&id, "grabbed_by", params.grabbed_by.as_deref()) {
            return err;
        }
        if self.no_session {
            return gram_unavailable(id);
        }

        // Claimant = explicit --as override, else the caller pane's identity.
        let who = params
            .grabbed_by
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| {
                params
                    .caller_pane_id
                    .as_deref()
                    .and_then(|pane| self.caller_identity(pane))
            });
        let Some(who) = who else {
            return encode_error(
                id,
                "unknown_caller",
                "could not resolve the grabbing agent; pass a valid caller_pane_id or grabbed_by",
            );
        };

        let target_id = params.id.clone();
        let now = super::unix_millis_now();
        // The claim runs under the store's advisory lock, and the app loop
        // serializes API requests, so this check-then-set is atomic across both
        // threads and processes — first grab wins, independent of identity. A lost
        // race changes nothing, so it does not rewrite the store.
        let outcome = crate::persist::gram::update_if_changed(move |items| {
            let result = (|| {
                let Some(item) = items.iter_mut().find(|item| item.id == target_id) else {
                    return Err(GrabError::NotFound);
                };
                if item.direction != StoredDirection::OwnerToAgent || item.to.is_some() {
                    return Err(GrabError::NotGrabbable);
                }
                if let Some(existing) = &item.grabbed_by {
                    return Err(GrabError::AlreadyGrabbed(existing.clone()));
                }
                item.grabbed_by = Some(who.clone());
                item.grabbed_unix_ms = Some(now);
                Ok(item.clone())
            })();
            let changed = result.is_ok();
            (result, changed)
        });

        match outcome {
            Ok((Ok(item), _)) => encode_success(
                id,
                ResponseResult::GramGrabbed {
                    // A claimed queue item is an owner post: no sender machine.
                    message: gram_item_to_info(item),
                },
            ),
            Ok((Err(GrabError::NotFound), _)) => {
                encode_error(id, "not_found", "no gram message with that id")
            }
            Ok((Err(GrabError::NotGrabbable), _)) => encode_error(
                id,
                "not_grabbable",
                "that message is not a shared-queue item",
            ),
            Ok((Err(GrabError::AlreadyGrabbed(owner)), _)) => {
                encode_error(id, "already_grabbed", format!("already grabbed by {owner}"))
            }
            Err(err) => encode_error(id, "gram_store_save_failed", err.to_string()),
        }
    }

    pub(super) fn handle_gram_mark_read(
        &mut self,
        id: String,
        params: GramMarkReadParams,
    ) -> String {
        if self.no_session {
            return gram_unavailable(id);
        }

        let targets: Vec<String> = params.targets().map(str::to_string).collect();
        if targets.is_empty() {
            return encode_error(id, "invalid_params", "pass id or ids");
        }
        // Returns (found, changed): every id must exist before any is marked,
        // and a re-mark of already-read messages does not rewrite the store.
        let outcome = crate::persist::gram::update_if_changed(move |items| {
            if !targets
                .iter()
                .all(|target| items.iter().any(|item| &item.id == target))
            {
                return (false, false);
            }
            let mut changed = false;
            for item in items.iter_mut().filter(|item| targets.contains(&item.id)) {
                changed |= !item.read_by_owner;
                item.read_by_owner = true;
            }
            (true, changed)
        });
        match outcome {
            Ok((true, _)) => encode_success(id, ResponseResult::Ok {}),
            Ok((false, _)) => encode_error(id, "not_found", "no gram message with that id"),
            Err(err) => encode_error(id, "gram_store_save_failed", err.to_string()),
        }
    }

    pub(super) fn handle_gram_delete(&mut self, id: String, params: GramDeleteParams) -> String {
        if self.no_session {
            return gram_unavailable(id);
        }

        // Resolve the caller's authority. The owner's app sends no caller pane and
        // may delete anything; an agent supplies its pane and may delete only a
        // message it is involved in. A caller pane that names no live pane is an
        // error, mirroring `gram.list` — not a silent fall-through to owner power.
        let identity = match params.caller_pane_id.as_deref() {
            Some(pane) => match self.caller_identity(pane) {
                Some(identity) => Some(identity),
                None => {
                    return encode_error(
                        id,
                        "unknown_caller",
                        "caller_pane_id is not a known pane; omit it to delete as the owner",
                    );
                }
            },
            None => None,
        };

        self.handle_gram_delete_for(id, params.id, identity)
    }

    fn handle_gram_delete_for(
        &mut self,
        id: String,
        target_id: String,
        identity: Option<String>,
    ) -> String {
        let outcome = crate::persist::gram::update_if_changed(move |items| {
            apply_delete(items, &target_id, identity.as_deref())
        });

        match outcome {
            Ok((DeleteOutcome::Deleted, _)) => encode_success(id, ResponseResult::Ok {}),
            Ok((DeleteOutcome::NotFound, _)) => {
                encode_error(id, "not_found", "no gram message with that id")
            }
            Ok((DeleteOutcome::Forbidden, _)) => encode_error(
                id,
                "forbidden",
                "you can only delete a gram message you sent, grabbed, or that is addressed to you",
            ),
            Err(err) => encode_error(id, "gram_store_save_failed", err.to_string()),
        }
    }

    /// Whether some live terminal has this exact unique agent name. Used to reject
    /// a direct `gram.post` to a nonexistent agent instead of black-holing it.
    fn is_live_agent_name(&self, name: &str) -> bool {
        self.state
            .terminals
            .values()
            .any(|terminal| terminal.agent_name.as_deref() == Some(name))
    }

    /// Resolve a public pane id (an agent's `HERDR_PANE_ID`) to its identity: the
    /// per-agent name if set (durable across restart / live-handoff, since it is
    /// snapshotted and restored), else the pane's public id.
    fn caller_identity(&self, caller_pane_id: &str) -> Option<String> {
        let (ws_idx, pane_id) = self.parse_pane_id(caller_pane_id)?;
        let terminal_id = self.state.workspaces.get(ws_idx)?.terminal_id(pane_id)?;
        self.state
            .terminals
            .get(terminal_id)
            .and_then(|terminal| terminal.agent_name.clone())
            .filter(|name| !name.trim().is_empty())
            .or_else(|| self.public_pane_id(ws_idx, pane_id))
    }

    /// Resolve a sender label: an explicit `from` wins; else the caller pane's
    /// resolved identity; else the generic "agent".
    fn resolve_sender(&self, from: Option<&str>, caller_pane_id: Option<&str>) -> String {
        from.map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| caller_pane_id.and_then(|pane| self.caller_identity(pane)))
            .unwrap_or_else(|| "agent".to_string())
    }

    /// The terminal id and agent kind behind a caller pane, recorded on the Gram
    /// it sends so a later view follows the agent, not a reusable name.
    fn caller_sender(&self, caller_pane_id: &str) -> Option<crate::persist::gram::GramSender> {
        let (ws_idx, pane_id) = self.parse_pane_id(caller_pane_id)?;
        let terminal_id = self.state.workspaces.get(ws_idx)?.terminal_id(pane_id)?;
        let terminal = self.state.terminals.get(terminal_id)?;
        Some(crate::persist::gram::GramSender {
            terminal_id: terminal_id.to_string(),
            agent: terminal.effective_agent_label().map(str::to_string),
        })
    }
}

fn validate_text(id: &str, text: &str, allow_empty: bool) -> Option<String> {
    if text.is_empty() && !allow_empty {
        return Some(encode_error(
            id.to_string(),
            "invalid_params",
            "text is empty",
        ));
    }
    if text.len() > MAX_TEXT_BYTES {
        return Some(encode_error(
            id.to_string(),
            "invalid_params",
            format!("text exceeds {MAX_TEXT_BYTES} bytes; send large content as a file"),
        ));
    }
    None
}

fn validate_label(id: &str, field: &str, value: Option<&str>) -> Option<String> {
    match value.map(str::trim) {
        Some(value) if value.len() > MAX_LABEL_BYTES => Some(encode_error(
            id.to_string(),
            "invalid_params",
            format!("{field} exceeds {MAX_LABEL_BYTES} bytes"),
        )),
        _ => None,
    }
}

fn gram_unavailable(id: String) -> String {
    encode_error(
        id,
        "gram_unavailable",
        "gram requires the shared herdr server",
    )
}

/// Map a storage record to its wire shape. The audience-visible fields carry as
/// stored; `file` stays `None` in this build's text core.
fn gram_item_to_info(item: GramItem) -> GramMessageInfo {
    GramMessageInfo {
        id: item.id,
        direction: match item.direction {
            StoredDirection::AgentToOwner => GramDirection::AgentToOwner,
            StoredDirection::OwnerToAgent => GramDirection::OwnerToAgent,
        },
        from: item.from,
        to: item.to,
        text: item.text,
        grabbed_by: item.grabbed_by,
        grabbed_unix_ms: item.grabbed_unix_ms,
        created_unix_ms: item.created_unix_ms,
        read_by_owner: item.read_by_owner,
        file: item.file.map(|file| GramFileInfo {
            name: file.name,
            size: file.size,
            mime: file.mime,
            sha256: file.sha256,
        }),
        origin_id: item.origin_id,
    }
}

/// A shared owner->agent message nobody has claimed yet.
fn is_open_shared_queue(item: &GramItem) -> bool {
    item.direction == StoredDirection::OwnerToAgent
        && item.to.is_none()
        && item.grabbed_by.is_none()
}

/// Whether a message belongs in an agent's view: the shared ungrabbed queue, an
/// item addressed to it, one it grabbed, or one it sent. An agent may not list
/// another agent's direct message (which is how a secret is sent). The owner (no
/// caller pane) can see everything.
fn agent_can_see(item: &GramItem, identity: &str) -> bool {
    let addressed_to_me =
        item.direction == StoredDirection::OwnerToAgent && item.to.as_deref() == Some(identity);
    let grabbed_by_me = item.grabbed_by.as_deref() == Some(identity);
    let sent_by_me = item.direction == StoredDirection::AgentToOwner && item.from == identity;
    is_open_shared_queue(item) || addressed_to_me || grabbed_by_me || sent_by_me
}

/// Whether an agent identity may delete a message: it sent it, it is addressed to
/// it, or it grabbed it. The owner (no caller pane) bypasses this entirely.
/// The same "involved in it" relation the agent view uses for membership, minus
/// the shared open queue — an agent should not delete unclaimed work it never
/// touched out from under the owner.
fn agent_may_delete(item: &GramItem, identity: &str) -> bool {
    let sent_by_me = item.direction == StoredDirection::AgentToOwner && item.from == identity;
    let addressed_to_me =
        item.direction == StoredDirection::OwnerToAgent && item.to.as_deref() == Some(identity);
    let grabbed_by_me = item.grabbed_by.as_deref() == Some(identity);
    sent_by_me || addressed_to_me || grabbed_by_me
}

/// The agent's view: the shared ungrabbed queue, items addressed to it, items it
/// grabbed, and its own sent messages. `only_queue` narrows to just the shared,
/// still-open queue so an agent can quickly scan available work.
fn filter_agent_view(items: &[GramItem], identity: &str, only_queue: bool) -> Vec<GramItem> {
    items
        .iter()
        .filter(|item| {
            if only_queue {
                return is_open_shared_queue(item);
            }
            agent_can_see(item, identity)
        })
        .cloned()
        .collect()
}

/// The owner's view: the shared open queue (`only_queue`), just unread
/// agent->owner messages (`unread_only`), or everything.
fn filter_owner_view(items: &[GramItem], only_queue: bool, unread_only: bool) -> Vec<GramItem> {
    items
        .iter()
        .filter(|item| {
            if only_queue {
                return is_open_shared_queue(item);
            }
            if unread_only {
                return is_unread(item);
            }
            true
        })
        .cloned()
        .collect()
}

/// An agent->owner message the owner has not read yet — the thing the app's badge
/// counts. Shared by the `unread_only` filter and the whole-list `unread_count`.
fn is_unread(item: &GramItem) -> bool {
    item.direction == StoredDirection::AgentToOwner && !item.read_by_owner
}

/// Fingerprint of a `gram.list` answer, for conditional polling.
///
/// Hashed over the SERIALIZED payload rather than a hand-picked set of fields: the
/// digest then changes exactly when the reply would differ, and a new field on
/// `GramMessageInfo` cannot silently fall outside it. The store id is mixed in so a
/// client pointed at a different store is never told "unchanged" for a list it
/// does not hold.
fn list_digest(store_id: &str, messages: &[GramMessageInfo]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(store_id.as_bytes());
    hasher.update(b"\0");
    match serde_json::to_vec(messages) {
        Ok(bytes) => hasher.update(&bytes),
        // Cannot happen for these types, and must not be papered over with a
        // constant: mix in the error so the digest is at least unique per failure
        // rather than equal across unrelated answers.
        Err(err) => hasher.update(format!("serialize_error:{err}").as_bytes()),
    }
    format!("{:x}", hasher.finalize())
}

/// Decide and apply a delete against the in-memory list. `identity` is `None`
/// for the owner (may delete any message) or `Some(agent)` (may delete only a
/// message it is involved in). Returns the outcome plus whether the list
/// changed, matching [`crate::persist::gram::update_if_changed`]'s mutation
/// contract — the store is rewritten only on an actual removal. Pure over the
/// list so the find/authorize/remove logic is unit-tested without an App or the
/// store.
fn apply_delete(
    items: &mut Vec<GramItem>,
    id: &str,
    identity: Option<&str>,
) -> (DeleteOutcome, bool) {
    let Some(pos) = items.iter().position(|item| item.id == id) else {
        return (DeleteOutcome::NotFound, false);
    };
    if let Some(identity) = identity {
        if !agent_may_delete(&items[pos], identity) {
            return (DeleteOutcome::Forbidden, false);
        }
    }
    items.remove(pos);
    (DeleteOutcome::Deleted, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner_shared(id: &str) -> GramItem {
        GramItem {
            id: id.to_string(),
            direction: StoredDirection::OwnerToAgent,
            from: "owner".to_string(),
            to: None,
            text: "shared task".to_string(),
            grabbed_by: None,
            grabbed_unix_ms: None,
            created_unix_ms: 1000,
            read_by_owner: true,
            file: None,
            origin_id: "machine_test".to_string(),
            sender: None,
        }
    }

    fn direct_to(id: &str, agent: &str) -> GramItem {
        let mut item = owner_shared(id);
        item.to = Some(agent.to_string());
        item.text = "direct path".to_string();
        item
    }

    fn sent_by_agent(id: &str, agent: &str) -> GramItem {
        GramItem {
            id: id.to_string(),
            direction: StoredDirection::AgentToOwner,
            from: agent.to_string(),
            to: None,
            text: "status update".to_string(),
            grabbed_by: None,
            grabbed_unix_ms: None,
            created_unix_ms: 1001,
            read_by_owner: false,
            file: None,
            origin_id: "machine_test".to_string(),
            sender: None,
        }
    }

    #[test]
    fn agent_view_only_shows_its_own_membership() {
        let items = vec![
            owner_shared("shared"),
            direct_to("direct", "agent-a"),
            sent_by_agent("sent", "agent-b"),
        ];
        let view = filter_agent_view(&items, "agent-a", false);
        assert_eq!(view.len(), 2);
        assert!(view.iter().any(|item| item.id == "shared"));
        assert!(view.iter().any(|item| item.id == "direct"));
        assert!(!view.iter().any(|item| item.id == "sent"));

        let queue = filter_agent_view(&items, "agent-a", true);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].id, "shared");
    }

    #[test]
    fn owner_view_filtering_works() {
        let items = vec![owner_shared("shared"), sent_by_agent("unread", "agent-a")];
        assert_eq!(filter_owner_view(&items, false, false).len(), 2);
        let unread = filter_owner_view(&items, false, true);
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].id, "unread");
        assert_eq!(filter_owner_view(&items, true, false)[0].id, "shared");
        // only_queue wins over unread_only (the CLI rejects the combination
        // before reaching here anyway).
        assert_eq!(filter_owner_view(&items, true, true)[0].id, "shared");
    }

    #[test]
    fn unread_is_agent_to_owner_only() {
        assert!(is_unread(&sent_by_agent("a", "agent")));
        let mut owner_sent = owner_shared("b");
        owner_sent.read_by_owner = false;
        assert!(!is_unread(&owner_sent));
    }

    #[test]
    fn apply_delete_owner_removes_any_message() {
        let mut items = vec![owner_shared("one"), sent_by_agent("two", "agent")];
        assert_eq!(
            apply_delete(&mut items, "two", None),
            (DeleteOutcome::Deleted, true)
        );
        assert_eq!(items.len(), 1);
        assert_eq!(
            apply_delete(&mut items, "missing", None),
            (DeleteOutcome::NotFound, false)
        );
    }

    #[test]
    fn apply_delete_agent_only_its_own() {
        let mut items = vec![
            owner_shared("shared"),
            direct_to("direct", "agent"),
            sent_by_agent("sent", "agent"),
        ];
        // The open queue is the owner's to claim — an agent that never touched it
        // cannot delete it out from under the owner.
        assert_eq!(
            apply_delete(&mut items, "shared", Some("agent")),
            (DeleteOutcome::Forbidden, false)
        );
        assert_eq!(
            apply_delete(&mut items, "direct", Some("agent")),
            (DeleteOutcome::Deleted, true)
        );
        assert_eq!(
            apply_delete(&mut items, "sent", Some("agent")),
            (DeleteOutcome::Deleted, true)
        );
    }

    #[test]
    fn apply_delete_missing_id_is_not_found_and_no_change() {
        let mut items = vec![owner_shared("one")];
        assert_eq!(
            apply_delete(&mut items, "two", None),
            (DeleteOutcome::NotFound, false)
        );
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn validate_text_rejects_empty_and_oversized() {
        assert!(validate_text("id", "", false).is_some());
        assert!(validate_text("id", "", true).is_none());
        assert!(validate_text("id", "ok", false).is_none());
        assert!(validate_text("id", &"x".repeat(MAX_TEXT_BYTES + 1), false).is_some());
        assert!(validate_text("id", &"x".repeat(MAX_TEXT_BYTES), false).is_none());
    }

    #[test]
    fn validate_label_caps_length() {
        assert!(validate_label("id", "from", Some(&"x".repeat(MAX_LABEL_BYTES))).is_none());
        assert!(
            validate_label("id", "from", Some(&"x".repeat(MAX_LABEL_BYTES + 1))).is_some(),
            "over-limit label is rejected"
        );
        assert!(validate_label("id", "from", None).is_none());
    }

    #[test]
    fn gram_item_to_info_carries_origin_id_to_the_wire() {
        let item = sent_by_agent("one", "agent");
        let info = gram_item_to_info(item);
        assert_eq!(info.origin_id, "machine_test");
        assert!(info.file.is_none());
    }

    #[test]
    fn list_digest_is_stable_for_the_same_answer() {
        let a = vec![sent_by_agent("one", "agent")];
        let b = vec![sent_by_agent("one", "agent")];
        assert_eq!(
            list_digest("machine-a", &gram_infos(&a)),
            list_digest("machine-a", &gram_infos(&b))
        );
        assert_ne!(
            list_digest("machine-a", &gram_infos(&a)),
            list_digest("machine-b", &gram_infos(&b))
        );
    }

    #[test]
    fn list_digest_changes_with_content_and_with_the_store() {
        let a = vec![sent_by_agent("one", "agent")];
        let b = vec![sent_by_agent("two", "agent")];
        assert_ne!(
            list_digest("machine-a", &gram_infos(&a)),
            list_digest("machine-a", &gram_infos(&b))
        );
    }

    fn gram_infos(items: &[GramItem]) -> Vec<GramMessageInfo> {
        items.iter().cloned().map(gram_item_to_info).collect()
    }
}
