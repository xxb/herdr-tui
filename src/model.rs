//! Shared Space/Tab document (`doc("herdr-app")`) and live terminal state model.
//! Bidirectionally compatible with official Herdr Desktop & Android clients.

use crate::core::CoreClient;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashSet};
use std::hash::{BuildHasher, Hasher};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AgentState {
    None = 0,
    Idle = 1,
    Working = 2,
    Done = 3,
    Blocked = 4,
}

impl AgentState {
    pub fn symbol(self) -> &'static str {
        match self {
            AgentState::Blocked => "◉",
            AgentState::Working => "●",
            AgentState::Done => "✓",
            AgentState::Idle => "○",
            AgentState::None => "·",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AgentState::Blocked => "blocked",
            AgentState::Working => "working",
            AgentState::Done => "done",
            AgentState::Idle => "idle",
            AgentState::None => "shell",
        }
    }
}

#[derive(Clone, Debug, Default)]
#[allow(dead_code)]
pub struct TerminalInfo {
    pub address: String,
    pub name: Option<String>,
    pub state: String,
    pub cwd: String,
    pub fg_cwd: Option<String>,
    pub fg_name: Option<String>,
    pub title: String,
    pub status_word: Option<String>,
    pub status_app: Option<String>,
    pub status_msg: Option<String>,
    pub turn: u64,
    pub seen_turn: Option<u64>,
    pub pinned: bool,
    pub parent: Option<String>,
}

impl TerminalInfo {
    pub fn from_json(v: &Value) -> Option<Self> {
        let address = v.get("address")?.as_str()?.to_string();
        let state = v
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("live")
            .to_string();
        let name = v.get("name").and_then(Value::as_str).map(String::from);
        let cwd = v
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let fg_cwd = v
            .get("foreground")
            .and_then(|f| f.get("cwd"))
            .and_then(Value::as_str)
            .map(String::from);
        let fg_name = v
            .get("foreground")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .map(String::from);
        let title = v
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let status_word = v
            .get("status")
            .and_then(|s| s.get("state"))
            .and_then(Value::as_str)
            .map(String::from);
        let status_app = v
            .get("status")
            .and_then(|s| s.get("app"))
            .and_then(Value::as_str)
            .map(String::from);
        let status_msg = v
            .get("status")
            .and_then(|s| s.get("msg"))
            .and_then(Value::as_str)
            .map(String::from);
        let turn = v.get("turn").and_then(Value::as_u64).unwrap_or(0);
        let seen_turn = v
            .get("labels")
            .and_then(|l| l.get("herdr.seen"))
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<u64>().ok());
        let pinned = v
            .get("labels")
            .and_then(|l| l.get("herdr.pinned"))
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty());
        let parent = v.get("parent").and_then(Value::as_str).map(String::from);

        Some(Self {
            address,
            name,
            state,
            cwd,
            fg_cwd,
            fg_name,
            title,
            status_word,
            status_app,
            status_msg,
            turn,
            seen_turn,
            pinned,
            parent,
        })
    }

    pub fn effective_cwd(&self) -> &str {
        self.fg_cwd
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.cwd)
    }

    pub fn agent_state(&self) -> AgentState {
        match self.status_word.as_deref() {
            Some("blocked" | "error") => AgentState::Blocked,
            Some("working") => AgentState::Working,
            Some(_) => {
                if self.turn > 0 && self.seen_turn.is_none_or(|seen| self.turn > seen) {
                    AgentState::Done
                } else {
                    AgentState::Idle
                }
            }
            None => {
                if self.turn > 0 && self.seen_turn.is_none_or(|seen| self.turn > seen) {
                    AgentState::Done
                } else {
                    AgentState::None
                }
            }
        }
    }

    pub fn short_label(&self) -> String {
        if let Some(name) = &self.name {
            if !name.is_empty() {
                return name.clone();
            }
        }
        if let Some(app) = &self.status_app {
            if !app.is_empty() {
                return app.clone();
            }
        }
        if let Some(fg) = &self.fg_name {
            if !fg.is_empty() {
                return fg.clone();
            }
        }
        if !self.title.is_empty() {
            return self.title.chars().take(20).collect();
        }
        self.address.clone()
    }
}

/// Official `doc("herdr-app")` JSON structure
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AppDocument {
    #[serde(default)]
    pub spaces: Vec<SharedSpace>,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SharedSpace {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub folder: String,
    #[serde(default)]
    pub tabs: Vec<SharedTab>,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SharedTab {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub root: Value,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

pub fn fresh_id(prefix: &str) -> String {
    let r1 = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    let r2 = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    format!("{prefix}{r1:016x}{r2:016x}")
}

pub fn make_single_terminal_tab(tab_id: &str, name: &str, terminal_addr: &str) -> SharedTab {
    let pane_id = fresh_id("p");
    SharedTab {
        id: tab_id.to_string(),
        name: name.to_string(),
        root: json!({
            "id": tab_id,
            "type": "row",
            "children": [
                {
                    "id": pane_id,
                    "type": "block",
                    "content": {
                        "terminal": terminal_addr
                    }
                }
            ]
        }),
        rest: Map::new(),
    }
}

/// Extract all terminal addresses referenced inside a `SharedTab.root` layout node.
pub fn extract_terminals(node: &Value, out: &mut Vec<String>) {
    if let Some(term) = node
        .get("content")
        .and_then(|c| c.get("terminal"))
        .and_then(Value::as_str)
    {
        if !term.is_empty() && !out.iter().any(|t| t == term) {
            out.push(term.to_string());
        }
    }
    if let Some(children) = node.get("children").and_then(Value::as_array) {
        for child in children {
            extract_terminals(child, out);
        }
    }
}

/// Remove dead terminal blocks from a `root` node while keeping live ones.
pub fn prune_dead_terminals_in_node(node: &mut Value, live: &BTreeMap<String, TerminalInfo>) {
    if let Some(children) = node.get_mut("children").and_then(Value::as_array_mut) {
        for child in children.iter_mut() {
            prune_dead_terminals_in_node(child, live);
        }
        children.retain(|child| {
            if let Some(term) = child
                .get("content")
                .and_then(|c| c.get("terminal"))
                .and_then(Value::as_str)
            {
                live.contains_key(term)
            } else if let Some(grand) = child.get("children").and_then(Value::as_array) {
                !grand.is_empty()
            } else {
                true
            }
        });
    }
}

pub fn folder_basename(folder: &str) -> String {
    let trimmed = folder.trim_end_matches('/');
    if trimmed.is_empty() {
        return "root".to_string();
    }
    trimmed
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(trimmed)
        .to_string()
}

pub struct World {
    pub doc: AppDocument,
    pub doc_version: u64,
    pub terminals: BTreeMap<String, TerminalInfo>,
    pub active_space_idx: usize,
    /// Per-space active tab ID
    pub active_tab_by_space: BTreeMap<String, String>,
    /// Per-tab selected terminal address (when a tab from desktop app has multiple panes)
    pub active_term_by_tab: BTreeMap<String, String>,
}

impl Default for World {
    fn default() -> Self {
        Self {
            doc: AppDocument::default(),
            doc_version: 0,
            terminals: BTreeMap::new(),
            active_space_idx: 0,
            active_tab_by_space: BTreeMap::new(),
            active_term_by_tab: BTreeMap::new(),
        }
    }
}

impl World {
    pub fn load_initial(&mut self, core: &CoreClient, snapshot: &Value) -> Result<()> {
        self.apply_terminals_snapshot(snapshot);
        self.fetch_doc(core)?;
        self.reconcile_and_sync(core)?;
        Ok(())
    }

    pub fn apply_terminals_snapshot(&mut self, snapshot: &Value) {
        if let Some(arr) = snapshot.get("terminals").and_then(Value::as_array) {
            self.terminals.clear();
            for item in arr {
                if let Some(info) = TerminalInfo::from_json(item) {
                    if info.state == "live" {
                        self.terminals.insert(info.address.clone(), info);
                    }
                }
            }
        }
    }

    pub fn refresh_terminals(&mut self, core: &CoreClient) -> Result<()> {
        let list = core.call("terminal.list", json!({}))?;
        self.apply_terminals_snapshot(&list);
        Ok(())
    }

    pub fn fetch_doc(&mut self, core: &CoreClient) -> Result<()> {
        let res = core.call("doc.get", json!({ "name": "herdr-app" }))?;
        self.doc_version = res.get("version").and_then(Value::as_u64).unwrap_or(0);
        if let Some(val) = res.get("value") {
            if !val.is_null() {
                if let Ok(parsed) = serde_json::from_value::<AppDocument>(val.clone()) {
                    self.doc = parsed;
                }
            } else {
                self.doc = AppDocument::default();
            }
        }
        self.clamp_selection();
        Ok(())
    }

    /// Apply a local document mutation and persist via `doc.set` with CAS retry.
    /// On conflict, fetches the latest remote document (`self.doc` + `self.doc_version`)
    /// and reapplies `mutate` onto the refreshed document so other clients' edits are preserved.
    pub fn mutate_doc<F>(&mut self, core: &CoreClient, mut mutate: F) -> Result<()>
    where
        F: FnMut(&mut Self),
    {
        let mut last_err = None;
        for _ in 0..3 {
            mutate(self);
            self.clamp_selection();
            let val = serde_json::to_value(&self.doc)?;
            match core.call(
                "doc.set",
                json!({
                    "name": "herdr-app",
                    "version": self.doc_version,
                    "value": val,
                }),
            ) {
                Ok(res) => {
                    if let Some(v) = res.get("version").and_then(Value::as_u64) {
                        self.doc_version = v;
                    }
                    return Ok(());
                }
                Err(e) if e.to_string().starts_with("conflict:") => {
                    last_err = Some(e);
                    self.fetch_doc(core)?;
                }
                Err(e) => return Err(e),
            }
        }
        if let Some(err) = last_err {
            return Err(err);
        }
        Ok(())
    }

    /// Pure in-memory reconciliation of `self.doc` against `self.terminals`:
    /// 1. Prunes dead terminals from tabs and drops tabs that have zero live terminals.
    /// 2. Adopts any live orphan terminals into a matching Space (by cwd) or a default Space.
    /// 3. Removes empty spaces when non-empty spaces exist.
    pub fn reconcile_doc_in_place(&mut self) -> bool {
        let mut changed = false;

        // 1. Prune dead terminals from existing tabs
        for space in &mut self.doc.spaces {
            for tab in &mut space.tabs {
                let mut before = Vec::new();
                extract_terminals(&tab.root, &mut before);
                if before.iter().any(|t| !self.terminals.contains_key(t)) {
                    prune_dead_terminals_in_node(&mut tab.root, &self.terminals);
                    changed = true;
                }
            }
            let prev_len = space.tabs.len();
            space.tabs.retain(|tab| {
                let mut terms = Vec::new();
                extract_terminals(&tab.root, &mut terms);
                terms.iter().any(|t| self.terminals.contains_key(t))
            });
            if space.tabs.len() != prev_len {
                changed = true;
            }
        }

        // Collect all terminal addresses currently in `doc.spaces`
        let mut assigned: HashSet<String> = HashSet::new();
        for space in &self.doc.spaces {
            for tab in &space.tabs {
                let mut terms = Vec::new();
                extract_terminals(&tab.root, &mut terms);
                for t in terms {
                    assigned.insert(t);
                }
            }
        }

        // 2. Adopt live terminals that are not yet in any tab
        let mut orphans: Vec<TerminalInfo> = self
            .terminals
            .values()
            .filter(|t| !assigned.contains(&t.address))
            .cloned()
            .collect();
        // Sort by numeric terminal ID (`t1`, `t2`, ...)
        orphans.sort_by_key(|t| {
            t.address
                .strip_prefix('t')
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(u64::MAX)
        });

        for orphan in orphans {
            let tab_id = fresh_id("tb");
            let tab_name = orphan.name.clone().unwrap_or_default();
            let tab = make_single_terminal_tab(&tab_id, &tab_name, &orphan.address);

            // Find a space with matching folder, or use the first space, or create one
            let space_idx = self
                .doc
                .spaces
                .iter()
                .position(|s| !s.folder.is_empty() && s.folder == orphan.cwd)
                .or_else(|| (!self.doc.spaces.is_empty()).then_some(0));

            if let Some(idx) = space_idx {
                self.doc.spaces[idx].tabs.push(tab);
            } else {
                let cwd = if orphan.cwd.is_empty() {
                    std::env::current_dir()
                        .ok()
                        .and_then(|p| p.to_str().map(String::from))
                        .unwrap_or_else(|| "/".to_string())
                } else {
                    orphan.cwd.clone()
                };
                self.doc.spaces.push(SharedSpace {
                    id: fresh_id("sp"),
                    name: String::new(),
                    folder: cwd,
                    tabs: vec![tab],
                    rest: Map::new(),
                });
            }
            changed = true;
        }

        // 3. Remove empty spaces when non-empty spaces exist
        if self.doc.spaces.len() > 1 && self.doc.spaces.iter().any(|s| !s.tabs.is_empty()) {
            let before = self.doc.spaces.len();
            self.doc.spaces.retain(|s| !s.tabs.is_empty());
            if self.doc.spaces.len() != before {
                changed = true;
            }
        }

        self.clamp_selection();
        changed
    }

    /// Reconcile `doc("herdr-app")` with live terminals in `herdr-core` and persist with CAS rebase.
    pub fn reconcile_and_sync(&mut self, core: &CoreClient) -> Result<bool> {
        let mut any_changed = self.reconcile_doc_in_place();

        // Ensure at least one live terminal exists if all spaces/tabs are empty
        let needs_spawn =
            self.doc.spaces.is_empty() || self.doc.spaces.iter().all(|s| s.tabs.is_empty());
        if needs_spawn {
            let cwd = self
                .doc
                .spaces
                .first()
                .map(|s| s.folder.clone())
                .filter(|f| !f.is_empty())
                .unwrap_or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .and_then(|p| p.to_str().map(String::from))
                        .unwrap_or_else(|| "/".to_string())
                });
            let run_res = core.call("terminal.run", json!({ "cwd": cwd, "bg": true }))?;
            if let Some(info) = TerminalInfo::from_json(&run_res) {
                self.terminals.insert(info.address.clone(), info);
                if self.reconcile_doc_in_place() {
                    any_changed = true;
                }
            }
        }

        if !any_changed {
            return Ok(false);
        }

        // Save with CAS conflict rebase: if another client updated `doc("herdr-app")`,
        // fetch the latest remote document and re-run `reconcile_doc_in_place` on top of it.
        for attempt in 0..3 {
            if attempt > 0 {
                let _ = self.reconcile_doc_in_place();
            }
            let val = serde_json::to_value(&self.doc)?;
            match core.call(
                "doc.set",
                json!({
                    "name": "herdr-app",
                    "version": self.doc_version,
                    "value": val,
                }),
            ) {
                Ok(res) => {
                    if let Some(v) = res.get("version").and_then(Value::as_u64) {
                        self.doc_version = v;
                    }
                    return Ok(true);
                }
                Err(e) if e.to_string().starts_with("conflict:") => {
                    self.fetch_doc(core)?;
                }
                Err(e) => return Err(e),
            }
        }

        Ok(any_changed)
    }

    pub fn clamp_selection(&mut self) {
        if self.doc.spaces.is_empty() {
            self.active_space_idx = 0;
            return;
        }
        if self.active_space_idx >= self.doc.spaces.len() {
            self.active_space_idx = self.doc.spaces.len() - 1;
        }
        for space in &self.doc.spaces {
            let valid = self
                .active_tab_by_space
                .get(&space.id)
                .is_some_and(|tid| space.tabs.iter().any(|t| &t.id == tid));
            if !valid {
                if let Some(first) = space.tabs.first() {
                    self.active_tab_by_space
                        .insert(space.id.clone(), first.id.clone());
                }
            }
        }
    }

    pub fn active_space(&self) -> Option<&SharedSpace> {
        self.doc.spaces.get(self.active_space_idx)
    }

    pub fn active_tab_index(&self) -> usize {
        let Some(space) = self.active_space() else {
            return 0;
        };
        let Some(tid) = self.active_tab_by_space.get(&space.id) else {
            return 0;
        };
        space.tabs.iter().position(|t| &t.id == tid).unwrap_or(0)
    }

    pub fn active_tab(&self) -> Option<&SharedTab> {
        let space = self.active_space()?;
        let idx = self.active_tab_index();
        space.tabs.get(idx)
    }

    pub fn tab_terminals(&self, tab: &SharedTab) -> Vec<String> {
        let mut terms = Vec::new();
        extract_terminals(&tab.root, &mut terms);
        terms.retain(|t| self.terminals.contains_key(t));
        terms
    }

    pub fn active_terminal_for_tab(&self, tab: &SharedTab) -> Option<String> {
        let terms = self.tab_terminals(tab);
        if terms.is_empty() {
            return None;
        }
        if let Some(chosen) = self.active_term_by_tab.get(&tab.id) {
            if terms.iter().any(|t| t == chosen) {
                return Some(chosen.clone());
            }
        }
        terms.into_iter().next()
    }

    pub fn active_terminal_address(&self) -> Option<String> {
        let tab = self.active_tab()?;
        self.active_terminal_for_tab(tab)
    }

    pub fn space_display_name(&self, space: &SharedSpace) -> String {
        if !space.name.is_empty() {
            return space.name.clone();
        }
        if !space.folder.is_empty() {
            return folder_basename(&space.folder);
        }
        if let Some(first_tab) = space.tabs.first() {
            if let Some(addr) = self.active_terminal_for_tab(first_tab) {
                if let Some(info) = self.terminals.get(&addr) {
                    return folder_basename(info.effective_cwd());
                }
            }
        }
        "space".to_string()
    }

    pub fn tab_display_name(&self, space: &SharedSpace, tab_idx: usize) -> String {
        let Some(tab) = space.tabs.get(tab_idx) else {
            return (tab_idx + 1).to_string();
        };
        if !tab.name.is_empty() {
            return tab.name.clone();
        }
        if let Some(addr) = self.active_terminal_for_tab(tab) {
            if let Some(info) = self.terminals.get(&addr) {
                return info.short_label();
            }
        }
        (tab_idx + 1).to_string()
    }

    pub fn tab_state(&self, tab: &SharedTab) -> AgentState {
        self.tab_terminals(tab)
            .into_iter()
            .filter_map(|addr| self.terminals.get(&addr))
            .map(TerminalInfo::agent_state)
            .max()
            .unwrap_or(AgentState::None)
    }

    pub fn space_state(&self, space: &SharedSpace) -> AgentState {
        space
            .tabs
            .iter()
            .map(|t| self.tab_state(t))
            .max()
            .unwrap_or(AgentState::None)
    }

    /// Mark the currently focused terminal as seen (`herdr.seen=<turn>`) if needed.
    pub fn mark_active_seen(&mut self, core: &CoreClient) {
        let Some(addr) = self.active_terminal_address() else {
            return;
        };
        let Some(info) = self.terminals.get_mut(&addr) else {
            return;
        };
        if info.turn > 0 && info.seen_turn.is_none_or(|seen| info.turn > seen) {
            let turn = info.turn;
            info.seen_turn = Some(turn);
            let _ = core.call(
                "terminal.label",
                json!({
                    "terminal": addr,
                    "labels": [format!("herdr.seen={turn}")],
                }),
            );
        }
    }

    pub fn select_tab_index(&mut self, idx: usize) {
        let Some(space) = self.doc.spaces.get(self.active_space_idx) else {
            return;
        };
        if let Some(tab) = space.tabs.get(idx) {
            self.active_tab_by_space
                .insert(space.id.clone(), tab.id.clone());
        }
    }

    pub fn next_tab(&mut self) {
        let Some(space) = self.active_space() else {
            return;
        };
        if space.tabs.is_empty() {
            return;
        }
        let next = (self.active_tab_index() + 1) % space.tabs.len();
        self.select_tab_index(next);
    }

    pub fn prev_tab(&mut self) {
        let Some(space) = self.active_space() else {
            return;
        };
        if space.tabs.is_empty() {
            return;
        }
        let cur = self.active_tab_index();
        let prev = if cur == 0 {
            space.tabs.len() - 1
        } else {
            cur - 1
        };
        self.select_tab_index(prev);
    }

    pub fn next_space(&mut self) {
        if self.doc.spaces.is_empty() {
            return;
        }
        self.active_space_idx = (self.active_space_idx + 1) % self.doc.spaces.len();
    }

    pub fn prev_space(&mut self) {
        if self.doc.spaces.is_empty() {
            return;
        }
        self.active_space_idx = if self.active_space_idx == 0 {
            self.doc.spaces.len() - 1
        } else {
            self.active_space_idx - 1
        };
    }

    /// Cycle through terminals inside the current tab (if a desktop tab was split into multiple panes).
    pub fn cycle_pane_in_tab(&mut self) {
        let Some(tab) = self.active_tab().cloned() else {
            return;
        };
        let terms = self.tab_terminals(&tab);
        if terms.len() <= 1 {
            return;
        }
        let cur = self
            .active_terminal_for_tab(&tab)
            .and_then(|c| terms.iter().position(|t| t == &c))
            .unwrap_or(0);
        let next = &terms[(cur + 1) % terms.len()];
        self.active_term_by_tab.insert(tab.id, next.clone());
    }

    /// Create a new Tab in the active Space, inheriting the current terminal's CWD (or Space folder).
    pub fn create_new_tab(&mut self, core: &CoreClient) -> Result<()> {
        let cwd = self
            .active_terminal_address()
            .and_then(|addr| self.terminals.get(&addr))
            .map(|info| info.effective_cwd().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                self.active_space()
                    .map(|s| s.folder.clone())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .ok()
                    .and_then(|p| p.to_str().map(String::from))
                    .unwrap_or_else(|| "/".to_string())
            });

        let run_res = core.call(
            "terminal.run",
            json!({
                "cwd": cwd,
                "bg": true,
                "labels": ["herdr.seen=0"],
            }),
        )?;
        let Some(info) = TerminalInfo::from_json(&run_res) else {
            return Ok(());
        };
        let addr = info.address.clone();
        self.terminals.insert(addr.clone(), info);

        let tab_id = fresh_id("tb");
        let tab = make_single_terminal_tab(&tab_id, "", &addr);
        let cur_tab_idx = self.active_tab_index();
        let target_space_id = self.active_space().map(|s| s.id.clone());

        self.mutate_doc(core, |world| {
            let idx = target_space_id
                .as_ref()
                .and_then(|sid| world.doc.spaces.iter().position(|s| &s.id == sid))
                .unwrap_or(world.active_space_idx);
            if let Some(space) = world.doc.spaces.get_mut(idx) {
                if !space.tabs.iter().any(|t| t.id == tab_id) {
                    let insert_at = (cur_tab_idx + 1).min(space.tabs.len());
                    space.tabs.insert(insert_at, tab.clone());
                }
                let space_id = space.id.clone();
                world.active_tab_by_space.insert(space_id, tab_id.clone());
            }
        })?;
        Ok(())
    }

    /// Create a new Space with a name and folder, plus its first Tab.
    pub fn create_new_space(&mut self, core: &CoreClient, name: &str) -> Result<()> {
        let cwd = self
            .active_terminal_address()
            .and_then(|addr| self.terminals.get(&addr))
            .map(|info| info.effective_cwd().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .ok()
                    .and_then(|p| p.to_str().map(String::from))
                    .unwrap_or_else(|| "/".to_string())
            });

        let run_res = core.call(
            "terminal.run",
            json!({
                "cwd": cwd,
                "bg": true,
                "labels": ["herdr.seen=0"],
            }),
        )?;
        let Some(info) = TerminalInfo::from_json(&run_res) else {
            return Ok(());
        };
        let addr = info.address.clone();
        self.terminals.insert(addr.clone(), info);

        let tab_id = fresh_id("tb");
        let tab = make_single_terminal_tab(&tab_id, "", &addr);
        let space_id = fresh_id("sp");
        let space_name = name.trim().to_string();

        self.mutate_doc(core, |world| {
            if !world.doc.spaces.iter().any(|s| s.id == space_id) {
                world.doc.spaces.push(SharedSpace {
                    id: space_id.clone(),
                    name: space_name.clone(),
                    folder: cwd.clone(),
                    tabs: vec![tab.clone()],
                    rest: Map::new(),
                });
            }
            if let Some(pos) = world.doc.spaces.iter().position(|s| s.id == space_id) {
                world.active_space_idx = pos;
            }
            world
                .active_tab_by_space
                .insert(space_id.clone(), tab_id.clone());
        })?;
        Ok(())
    }

    /// Close the currently active Tab (and kill its underlying terminal(s)).
    pub fn close_active_tab(&mut self, core: &CoreClient) -> Result<()> {
        let Some(space) = self.doc.spaces.get(self.active_space_idx) else {
            return Ok(());
        };
        let tab_idx = self.active_tab_index();
        let Some(tab) = space.tabs.get(tab_idx).cloned() else {
            return Ok(());
        };
        let closed_tab_id = tab.id.clone();
        let terms = self.tab_terminals(&tab);
        for addr in terms {
            let _ = core.call("terminal.kill", json!({ "terminal": addr }));
            self.terminals.remove(&addr);
        }
        self.mutate_doc(core, |world| {
            for s in &mut world.doc.spaces {
                s.tabs.retain(|t| t.id != closed_tab_id);
            }
        })?;
        self.reconcile_and_sync(core)?;
        Ok(())
    }

    pub fn rename_active_tab(&mut self, core: &CoreClient, new_name: &str) -> Result<()> {
        let Some(tab_id) = self.active_tab().map(|t| t.id.clone()) else {
            return Ok(());
        };
        let trimmed = new_name.trim().to_string();
        self.mutate_doc(core, |world| {
            for space in &mut world.doc.spaces {
                if let Some(tab) = space.tabs.iter_mut().find(|t| t.id == tab_id) {
                    tab.name = trimmed.clone();
                }
            }
        })?;
        Ok(())
    }

    pub fn rename_active_space(&mut self, core: &CoreClient, new_name: &str) -> Result<()> {
        let Some(space_id) = self.active_space().map(|s| s.id.clone()) else {
            return Ok(());
        };
        let trimmed = new_name.trim().to_string();
        self.mutate_doc(core, |world| {
            if let Some(space) = world.doc.spaces.iter_mut().find(|s| s.id == space_id) {
                space.name = trimmed.clone();
            }
        })?;
        Ok(())
    }

    /// Jump to the next Tab across all Spaces that has an Agent in `Blocked` or `Done` state.
    pub fn jump_next_alert_agent(&mut self) -> bool {
        if self.doc.spaces.is_empty() {
            return false;
        }
        let mut flat: Vec<(usize, String, AgentState)> = Vec::new();
        for (si, space) in self.doc.spaces.iter().enumerate() {
            for tab in &space.tabs {
                flat.push((si, tab.id.clone(), self.tab_state(tab)));
            }
        }
        if flat.is_empty() {
            return false;
        }
        let cur_space = self.active_space_idx;
        let cur_tab_id = self.active_tab().map(|t| t.id.clone()).unwrap_or_default();
        let start = flat
            .iter()
            .position(|(si, tid, _)| *si == cur_space && *tid == cur_tab_id)
            .unwrap_or(0);

        for offset in 1..=flat.len() {
            let (si, ref tid, state) = flat[(start + offset) % flat.len()];
            if matches!(state, AgentState::Blocked | AgentState::Done) {
                self.active_space_idx = si;
                let space_id = self.doc.spaces[si].id.clone();
                self.active_tab_by_space.insert(space_id, tid.clone());
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_and_prune_terminals_in_layout_tree() {
        let mut root = json!({
            "id": "tb1",
            "type": "row",
            "children": [
                { "id": "p1", "type": "block", "content": { "terminal": "t1" } },
                { "id": "p2", "type": "block", "content": { "terminal": "t2" } }
            ]
        });
        let mut terms = Vec::new();
        extract_terminals(&root, &mut terms);
        assert_eq!(terms, vec!["t1".to_string(), "t2".to_string()]);

        let mut live = BTreeMap::new();
        live.insert(
            "t2".to_string(),
            TerminalInfo {
                address: "t2".to_string(),
                state: "live".to_string(),
                ..Default::default()
            },
        );
        prune_dead_terminals_in_node(&mut root, &live);
        let mut after = Vec::new();
        extract_terminals(&root, &mut after);
        assert_eq!(after, vec!["t2".to_string()]);
    }

    #[test]
    fn seen_turn_controls_done_vs_idle_state() {
        let mut info = TerminalInfo {
            address: "t3".to_string(),
            state: "live".to_string(),
            status_word: Some("idle".to_string()),
            turn: 2,
            seen_turn: Some(1),
            ..Default::default()
        };
        assert_eq!(info.agent_state(), AgentState::Done);
        info.seen_turn = Some(2);
        assert_eq!(info.agent_state(), AgentState::Idle);
    }

    #[test]
    fn preserves_unknown_fields_in_app_document() {
        let raw = json!({
            "spaces": [
                {
                    "id": "sp1",
                    "name": "workspace",
                    "folder": "/srv/app",
                    "future_space_field": 42,
                    "tabs": [
                        {
                            "id": "tb1",
                            "name": "main",
                            "future_tab_field": true,
                            "root": { "id": "tb1", "type": "row", "children": [] }
                        }
                    ]
                }
            ],
            "future_doc_field": "kept"
        });
        let doc: AppDocument = serde_json::from_value(raw.clone()).unwrap();
        let round_trip = serde_json::to_value(&doc).unwrap();
        assert_eq!(round_trip["future_doc_field"], "kept");
        assert_eq!(round_trip["spaces"][0]["future_space_field"], 42);
        assert_eq!(round_trip["spaces"][0]["tabs"][0]["future_tab_field"], true);
    }
}
