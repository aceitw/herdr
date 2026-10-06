use serde::{Deserialize, Serialize};

/// Direction of a gram message on the wire. Mirrors
/// [`crate::persist::gram::GramDirection`]; the handler maps between them so the
/// storage record and the public contract can evolve independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GramDirection {
    AgentToOwner,
    OwnerToAgent,
}

/// `gram.send` — an agent sends the owner a push-notified message.
///
/// The sender identity is `from` when provided; otherwise it is resolved
/// server-side from `caller_pane_id` (the agent's `HERDR_PANE_ID`) to the agent's
/// name (else the pane's public id). The agent name is the durable identity — it
/// survives a restart or live-handoff — but it is a name: it can be renamed,
/// cleared, or reused, so attribution and the "sent by me" view follow the
/// identity as it stands, not a fixed token. An explicit `from` overrides
/// attribution entirely. `text` is capped server-side (~8 KiB); send large
/// content as a file, not a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GramSendParams {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_pane_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

/// `gram.post` — the owner posts a message to agents (from the app).
///
/// `to: Some(agent)` addresses one agent directly by its unique agent name (not
/// grabbable); the name must match a live agent or the call is rejected. `to:
/// None` posts to the shared grab-queue any agent can claim. `text` is capped
/// server-side (~8 KiB).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GramPostParams {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    pub from: Option<String>,
}

/// `gram.list` — read messages. The audience is chosen by `caller_pane_id`:
/// omit it for the owner view (everything); supply it for that pane's agent view
/// (its direct items, the shared ungrabbed queue, its own grabs, and its own sent
/// items). A `caller_pane_id` that names no live pane is an error, not a
/// fall-through to the owner view. `unread_only` is an owner-view filter and is
/// rejected when `caller_pane_id` is present.
///
/// `limit`/`before_id` page the answer; omitting both returns the whole filtered
/// list, exactly as before they existed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct GramListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_pane_id: Option<String>,
    /// Restrict to the shared, still-ungrabbed queue (either audience).
    #[serde(default)]
    pub only_queue: bool,
    /// Owner view only: restrict to unread agent->owner messages.
    #[serde(default)]
    pub unread_only: bool,
    /// Conditional fetch: the `digest` from a previous `gram.list` answer. When it
    /// still matches, the reply is `gram_list_unchanged` — store id and digest only,
    /// no messages — so a polling client pays a few hundred bytes instead of the whole
    /// store. The app polls every 6s over one SSH channel, where a full owner view is
    /// ~900 KB for ~870 messages; re-sending that unchanged payload is what made the
    /// inbox slow to open and starved the channel everything else shares.
    ///
    /// The digest answers a HEAD poll and is computed over the whole filtered list,
    /// so it stays cheap and meaningful for a paging client too. It is ignored when
    /// `before_id` is present: an older page is requested explicitly, so there is
    /// nothing the client could already hold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_unchanged_digest: Option<String>,
    /// Maximum messages to return, counted from the NEWEST end of the filtered
    /// newest-first list. Absent means no limit. Clamped server-side to
    /// `GRAM_LIST_MAX_LIMIT`; `Some(0)` is rejected rather than answered with an
    /// empty page, because a client asking for nothing is a bug, not a request.
    ///
    /// Paging exists for the initial open: the owner view is ~900 KB for ~870
    /// messages over the one SSH channel everything else shares, and the reader
    /// only ever sees the newest screenful first. Search, Read-all and the unread
    /// badge survive a windowed client because `unread_count` is reported over the
    /// full filtered list, not the page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// Cursor: return only messages strictly OLDER than this id in the newest-first
    /// order. An id absent from the filtered list is rejected rather than treated as
    /// "start at the head" — a stale cursor that silently fell back would re-deliver
    /// page 1 forever while the reader scrolled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_id: Option<String>,
}

/// `gram.grab` — an agent claims a shared-queue item. The claim is first-wins and
/// atomic at the storage layer, so no two agents can ever hold the same item —
/// this holds regardless of identity. The claimant label is `grabbed_by` when
/// provided, otherwise resolved from `caller_pane_id` to the agent's identity;
/// that label carries the same name-semantics as `gram.send`'s `from` (a rename
/// or reuse moves which items show as "mine", but never lets a second agent
/// claim one). Fails if the item is missing, not a shared item, or already
/// grabbed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GramGrabParams {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_pane_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grabbed_by: Option<String>,
}

/// `gram.mark_read` — the owner marks agent->owner messages read: `id`, `ids`
/// or both. An unknown id marks nothing and answers `not_found`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GramMarkReadParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ids: Vec<String>,
}

impl GramMarkReadParams {
    /// Every id named, `id` first.
    pub fn targets(&self) -> impl Iterator<Item = &str> {
        self.id.iter().chain(&self.ids).map(String::as_str)
    }
}

/// `gram.delete` — remove a message from the store for good.
///
/// Deletion is deliberately destructive: the record is gone, which is what makes
/// gram safe for a short-lived secret like a temporary API key — send it, use it,
/// delete it. Authority follows the caller: the owner's app sends no
/// `caller_pane_id` and may delete any message; an agent supplies its
/// `caller_pane_id` and may delete only a message it is involved in (one it sent,
/// one addressed to it, or one it grabbed), else the call is rejected. A
/// `caller_pane_id` that names no live pane is an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GramDeleteParams {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_pane_id: Option<String>,
}

/// A file attached to a gram message, as returned to clients. Optional in this
/// fork's text-core scope: no upload path exists yet, so every stored message
/// has `file: None`, but the field and shape stay part of the wire contract so
/// the HerdrUp app (which always expects it) decodes cleanly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GramFileInfo {
    pub name: String,
    pub size: u64,
    pub mime: String,
    pub sha256: String,
}

/// A gram message as returned to clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GramMessageInfo {
    pub id: String,
    pub direction: GramDirection,
    pub from: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grabbed_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grabbed_unix_ms: Option<u64>,
    pub created_unix_ms: u64,
    #[serde(default)]
    pub read_by_owner: bool,
    /// Metadata for an attached file, or absent. Fetch the bytes with
    /// `gram.get_file`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<GramFileInfo>,
    /// Install-stable identity of the daemon/store that wrote this message. In
    /// this fork's scope there is exactly one store, so the value is stable
    /// across restarts.
    #[serde(default)]
    pub origin_id: String,
}
