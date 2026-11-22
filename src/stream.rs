use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::transport::{Rest, TransportError};

pub const STOP_CALLBACK: &str = "stop";
pub const RECEIVED_REACTION: &str = "👀";
pub const DONE_REACTION: &str = "👍";
pub const CURSOR: &str = " ▉";
pub const SLOT_SECS: f64 = 1.0;
pub const PREVIEW_LIMIT: usize = crate::transport::MAX_MESSAGE_LEN - 64;

const TYPING_SECS: f64 = 4.0;
const TICK_MILLIS: u64 = 250;
const MAX_ROWS: usize = 64;
const FEED_BUDGET: usize = 2600;
const SEND_ATTEMPTS: usize = 3;
const FLOOD_WAIT_CAP: f64 = 10.0;

pub fn quiet(config: &Value) -> bool {
    config
        .get("quiet")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

pub fn enabled(config: &Value) -> bool {
    config
        .get("stream_replies")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| !quiet(config))
}

pub fn status_line_enabled(config: &Value) -> bool {
    config
        .get("activity_indicator")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| !quiet(config))
}

pub fn reactions_enabled(config: &Value) -> bool {
    config
        .get("reactions")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

fn typing_enabled(config: &Value) -> bool {
    config
        .get("typing_indicator")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Draft,
    Edit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Done,
    Failed(String),
    Stopped,
    Retried,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finish {
    Landed,
    Durable,
}

struct Screen {
    surface: Surface,
    started: f64,
    draft_id: i64,
    rows: Vec<Value>,
    prose: String,
    tail: String,
    segment: u64,
    status_id: Option<i64>,
    reply_id: Option<i64>,
    status_shown: String,
    reply_shown: String,
}

enum Turn {
    Opening(Screen),
    Live(Screen),
    Done,
}

impl Turn {
    fn take(self) -> Option<Screen> {
        match self {
            Self::Opening(screen) | Self::Live(screen) => Some(screen),
            Self::Done => None,
        }
    }

    fn screen_mut(&mut self) -> Option<&mut Screen> {
        match self {
            Self::Opening(screen) | Self::Live(screen) => Some(screen),
            Self::Done => None,
        }
    }
}

impl Screen {
    fn absorb(&mut self, row: &Value) {
        match row.get("phase").and_then(Value::as_str) {
            Some("text") => {
                let segment = row.get("segment").and_then(Value::as_u64).unwrap_or(0);
                if segment != self.segment && !self.prose.is_empty() {
                    self.prose.push_str("\n\n");
                }
                self.segment = segment;
                if let Some(delta) = row.get("delta").and_then(Value::as_str) {
                    self.prose.push_str(delta);
                }
                let tail = row.get("tail").and_then(Value::as_str).unwrap_or("");
                let done = row.get("done").and_then(Value::as_bool).unwrap_or(false);
                self.tail.clear();
                if !done {
                    self.tail.push_str(tail);
                }
            }
            Some("retract") => {}
            _ => {
                if self.rows.len() >= MAX_ROWS {
                    self.rows.remove(0);
                }
                self.rows.push(row.clone());
            }
        }
    }

    fn prose(&self) -> String {
        format!("{}{}", self.prose, self.tail)
    }

    fn elapsed(&self, now: f64) -> f64 {
        (now - self.started).max(0.0)
    }
}

#[derive(Default)]
struct Inner {
    turns: HashMap<String, Turn>,
    slots: HashMap<String, Slot>,
    stops: HashMap<String, String>,
    reactions: HashMap<String, std::collections::VecDeque<(String, i64)>>,
}

#[derive(Debug, Clone, Copy, Default)]
struct Slot {
    next_at: f64,
    typed_at: f64,
}

pub struct LiveView {
    config: Value,
    rest: Option<Rest>,
    inner: Mutex<Inner>,
}

enum Op {
    PostStatus { text: String },
    EditStatus { id: i64, text: String },
    Draft { draft_id: i64, text: String },
    PostReply { text: String },
    EditReply { id: i64, text: String },
}

fn stop_keyboard() -> Value {
    json!({"inline_keyboard": [[{"text": "■ Stop", "callback_data": STOP_CALLBACK}]]})
}

fn plain_keyboard() -> Value {
    json!({"inline_keyboard": []})
}

fn key(chat: &str, message_id: i64) -> String {
    format!("{chat}:{message_id}")
}

fn log(what: &str, chat: &str, error: &TransportError) {
    let detail = match error {
        TransportError::Http(code, detail) => format!("HTTP {code}: {detail}"),
        TransportError::Net(detail) => format!("network: {detail}"),
        TransportError::RateLimited(after) => format!("rate limited, retry after {after:?}s"),
        TransportError::Invalid(detail) => format!("not sent: {detail}"),
        TransportError::Auth(_) => "unauthorized".to_string(),
        TransportError::Forbidden(_) => "forbidden".to_string(),
    };
    eprintln!("[telegram] {what} failed in chat {chat}: {detail}");
}

fn duration(secs: f64) -> String {
    if secs < 60.0 {
        return format!("{secs:.1}s");
    }
    let whole = secs as u64;
    format!("{}m {:02}s", whole / 60, whole % 60)
}

fn draft_id(seed: f64) -> i64 {
    let id = (seed * 1000.0) as i64;
    if id == 0 {
        1
    } else {
        id.rem_euclid(1_000_000_000) + 1
    }
}

fn status_markdown(config: &Value, screen: &Screen, now: f64) -> String {
    let mut text = format!("**Working** · {}", duration(screen.elapsed(now)));
    let summary = crate::activity::summary(&screen.rows);
    if !summary.is_empty() {
        text.push_str(" · ");
        text.push_str(&summary);
    }
    if crate::activity::enabled(config) {
        let feed = crate::activity::feed(&screen.rows, FEED_BUDGET);
        if !feed.is_empty() {
            text.push('\n');
            text.push_str(&feed);
        }
    }
    text
}

fn reply_markdown(screen: &Screen) -> String {
    let prose = crate::media_tags::strip_for_display(&screen.prose());
    let prose = crate::text::sanitize(&prose);
    let prose = prose.trim_end();
    if prose.trim().is_empty() {
        return String::new();
    }
    crate::render::split_markdown(prose, PREVIEW_LIMIT)
        .first()
        .map(|first| format!("{first}{CURSOR}"))
        .unwrap_or_default()
}

fn first_chunk(markdown: &str) -> String {
    crate::render::split_markdown(markdown, PREVIEW_LIMIT)
        .first()
        .cloned()
        .unwrap_or_default()
}

fn ending_markdown(outcome: &Outcome, screen: &Screen, now: f64) -> String {
    let elapsed = screen.elapsed(now);
    let mut text = match outcome {
        Outcome::Done => format!("**Done** after {}", duration(elapsed)),
        Outcome::Stopped => format!("**Stopped** after {}", duration(elapsed)),
        Outcome::Retried => format!("**Queued again** after {}", duration(elapsed)),
        Outcome::Failed(reason) => {
            if reason.trim().is_empty() {
                format!("**Failed** after {}", duration(elapsed))
            } else {
                format!(
                    "**Failed**: {} after {}",
                    crate::text::escape_html(reason),
                    duration(elapsed)
                )
            }
        }
    };
    let summary = crate::activity::summary(&screen.rows);
    if !summary.is_empty() {
        text.push_str(" · ");
        text.push_str(&summary);
    }
    if matches!(outcome, Outcome::Failed(_) | Outcome::Stopped) {
        text.push_str(" · some steps may already have happened");
    }
    text
}

impl LiveView {
    pub fn new(config: Value, rest: Option<Rest>) -> Arc<Self> {
        Arc::new(Self {
            config,
            rest,
            inner: Mutex::new(Inner::default()),
        })
    }

    fn now(&self) -> f64 {
        crate::durable::now_secs()
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn live_turn(&self, conversation: &str) -> bool {
        matches!(
            self.inner().turns.get(conversation),
            Some(Turn::Opening(_) | Turn::Live(_))
        )
    }

    pub async fn received(&self, chat: &str, message_id: i64, conversation: &str) {
        if !reactions_enabled(&self.config) || message_id <= 0 {
            return;
        }
        {
            let mut inner = self.inner();
            let queue = inner.reactions.entry(conversation.to_string()).or_default();
            if queue.len() >= MAX_ROWS {
                queue.pop_front();
            }
            queue.push_back((chat.to_string(), message_id));
        }
        if let Some(rest) = self.rest.as_ref() {
            if let Err(error) = rest
                .set_reaction(chat, message_id, Some(RECEIVED_REACTION))
                .await
            {
                log("reaction", chat, &error);
            }
        }
    }

    pub async fn concluded(&self, conversation: &str, done: bool) {
        if !reactions_enabled(&self.config) {
            return;
        }
        let target = {
            let mut inner = self.inner();
            let Some(queue) = inner.reactions.get_mut(conversation) else {
                return;
            };
            let held = queue.pop_front();
            if queue.is_empty() {
                inner.reactions.remove(conversation);
            }
            held
        };
        let Some((chat, message_id)) = target else {
            return;
        };
        if let Some(rest) = self.rest.as_ref() {
            let emoji = done.then_some(DONE_REACTION);
            if let Err(error) = rest.set_reaction(&chat, message_id, emoji).await {
                log("reaction", &chat, &error);
            }
        }
    }

    pub fn begin(&self, conversation: &str) {
        if !enabled(&self.config) && !status_line_enabled(&self.config) {
            return;
        }
        let now = self.now();
        let surface = if crate::session::private_conversation(conversation) {
            Surface::Draft
        } else {
            Surface::Edit
        };
        let screen = Screen {
            surface,
            started: now,
            draft_id: draft_id(now),
            rows: Vec::new(),
            prose: String::new(),
            tail: String::new(),
            segment: 0,
            status_id: None,
            reply_id: None,
            status_shown: String::new(),
            reply_shown: String::new(),
        };
        self.inner()
            .turns
            .insert(conversation.to_string(), Turn::Opening(screen));
    }

    pub fn observe(&self, conversation: &str, row: &Value) {
        let mut inner = self.inner();
        if let Some(screen) = inner.turns.get_mut(conversation).and_then(Turn::screen_mut) {
            screen.absorb(row);
        }
    }

    fn next_op(&self, conversation: &str) -> Option<Op> {
        let mut inner = self.inner();
        let screen = inner.turns.get_mut(conversation)?.screen_mut()?;
        let now = self.now();
        let status_wanted = status_line_enabled(&self.config);
        let status_text = if status_wanted {
            first_chunk(&status_markdown(&self.config, screen, now))
        } else {
            String::new()
        };
        if !status_text.is_empty() && screen.status_id.is_none() {
            return Some(Op::PostStatus { text: status_text });
        }
        if enabled(&self.config) {
            let reply = reply_markdown(screen);
            if !reply.is_empty() && reply != screen.reply_shown {
                return Some(match screen.surface {
                    Surface::Draft => Op::Draft {
                        draft_id: screen.draft_id,
                        text: reply,
                    },
                    Surface::Edit => match screen.reply_id {
                        Some(id) => Op::EditReply { id, text: reply },
                        None => Op::PostReply { text: reply },
                    },
                });
            }
        }
        if let Some(id) = screen.status_id {
            if status_text != screen.status_shown {
                return Some(Op::EditStatus {
                    id,
                    text: status_text,
                });
            }
        }
        None
    }

    fn flood(&self, chat: &str, after: Option<f64>) {
        let now = self.now();
        let mut inner = self.inner();
        let slot = inner.slots.entry(chat.to_string()).or_default();
        let wait = after.unwrap_or(SLOT_SECS).clamp(0.5, 120.0);
        slot.next_at = slot.next_at.max(now + wait);
    }

    fn note(&self, chat: &str, what: &str, error: &TransportError) {
        log(what, chat, error);
        if let TransportError::RateLimited(after) = error {
            self.flood(chat, *after);
        }
    }

    fn claim(&self, chat: &str, now: f64) -> bool {
        let mut inner = self.inner();
        let slot = inner.slots.entry(chat.to_string()).or_default();
        if now < slot.next_at {
            return false;
        }
        slot.next_at = now + SLOT_SECS;
        true
    }

    fn record(&self, conversation: &str, change: impl FnOnce(&mut Screen)) {
        let mut inner = self.inner();
        {
            if let Some(screen) = inner.turns.get_mut(conversation).and_then(Turn::screen_mut) {
                change(screen);
            }
        }
        if matches!(inner.turns.get(conversation), Some(Turn::Opening(_))) {
            if let Some(Turn::Opening(screen)) = inner.turns.remove(conversation) {
                inner
                    .turns
                    .insert(conversation.to_string(), Turn::Live(screen));
            }
        }
    }

    pub async fn publish(&self, chat: &str, conversation: &str) {
        let Some(op) = self.next_op(conversation) else {
            return;
        };
        let now = self.now();
        if !self.claim(chat, now) {
            return;
        }
        let Some(rest) = self.rest.as_ref() else {
            return;
        };
        match op {
            Op::PostStatus { text } => {
                match rest.send_markup(chat, &text, &stop_keyboard()).await {
                    Ok(value) => {
                        let id = value
                            .get("message_id")
                            .and_then(Value::as_i64)
                            .filter(|id| *id > 0);
                        if let Some(id) = id {
                            self.inner()
                                .stops
                                .insert(key(chat, id), conversation.to_string());
                        }
                        self.record(conversation, |screen| {
                            screen.status_id = id;
                            screen.status_shown = text;
                        });
                    }
                    Err(error) => self.note(chat, "status message", &error),
                }
            }
            Op::EditStatus { id, text } => {
                match rest
                    .edit_message(chat, id, &text, Some(&stop_keyboard()))
                    .await
                {
                    Ok(_) => self.record(conversation, |screen| {
                        screen.status_shown = text;
                    }),
                    Err(error) => self.note(chat, "status update", &error),
                }
            }
            Op::Draft { draft_id, text } => match rest.send_draft(chat, draft_id, &text).await {
                Ok(()) => self.record(conversation, |screen| {
                    screen.reply_shown = text;
                }),
                Err(error @ TransportError::RateLimited(_)) => {
                    self.note(chat, "reply draft", &error)
                }
                Err(error) => {
                    self.note(chat, "reply draft", &error);
                    self.record(conversation, |screen| screen.surface = Surface::Edit);
                }
            },
            Op::PostReply { text } => {
                match rest.send_markup(chat, &text, &plain_keyboard()).await {
                    Ok(value) => {
                        let id = value
                            .get("message_id")
                            .and_then(Value::as_i64)
                            .filter(|id| *id > 0);
                        self.record(conversation, |screen| {
                            screen.reply_id = id;
                            screen.reply_shown = text;
                        });
                    }
                    Err(error) => self.note(chat, "reply message", &error),
                }
            }
            Op::EditReply { id, text } => {
                match rest
                    .edit_message(chat, id, &text, Some(&plain_keyboard()))
                    .await
                {
                    Ok(_) => self.record(conversation, |screen| {
                        screen.reply_shown = text;
                    }),
                    Err(error) => log("reply update", chat, &error),
                }
            }
        }
    }

    pub async fn typing(&self, chat: &str) {
        if !typing_enabled(&self.config) {
            return;
        }
        let now = self.now();
        {
            let mut inner = self.inner();
            let slot = inner.slots.entry(chat.to_string()).or_default();
            if now < slot.next_at || now - slot.typed_at < TYPING_SECS {
                return;
            }
            slot.typed_at = now;
        }
        if let Some(rest) = self.rest.as_ref() {
            let _ = rest.typing(chat).await;
        }
    }

    pub async fn hold(&self, chat: &str) {
        loop {
            let now = self.now();
            let wait = {
                let mut inner = self.inner();
                let slot = inner.slots.entry(chat.to_string()).or_default();
                if now >= slot.next_at {
                    slot.next_at = now + SLOT_SECS;
                    0.0
                } else {
                    slot.next_at - now
                }
            };
            if wait <= 0.0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_secs_f64(wait.min(SLOT_SECS))).await;
        }
    }

    pub fn take_stop(&self, chat: &str, message_id: i64) -> Option<String> {
        self.inner().stops.remove(&key(chat, message_id))
    }

    async fn send_chunk(&self, chat: &str, text: &str) -> Result<(), TransportError> {
        let Some(rest) = self.rest.as_ref() else {
            return Err(TransportError::Net("no transport".to_string()));
        };
        for attempt in 1..=SEND_ATTEMPTS {
            self.hold(chat).await;
            match rest.send_markup(chat, text, &plain_keyboard()).await {
                Ok(_) => return Ok(()),
                Err(TransportError::RateLimited(after)) if attempt < SEND_ATTEMPTS => {
                    let wait = after.unwrap_or(3.0).clamp(1.0, FLOOD_WAIT_CAP);
                    tokio::time::sleep(std::time::Duration::from_secs_f64(wait)).await;
                }
                Err(error) => return Err(error),
            }
        }
        Err(TransportError::Net("send failed".to_string()))
    }

    pub async fn finish(
        &self,
        chat: &str,
        conversation: &str,
        outcome: Outcome,
        answer: Option<&str>,
    ) -> Finish {
        let screen = {
            let mut inner = self.inner();
            let Some(screen) = inner.turns.remove(conversation).and_then(Turn::take) else {
                return Finish::Durable;
            };
            inner.turns.insert(conversation.to_string(), Turn::Done);
            screen
        };
        if let Some(id) = screen.status_id {
            self.inner().stops.remove(&key(chat, id));
            if let Some(rest) = self.rest.as_ref() {
                self.hold(chat).await;
                match outcome {
                    Outcome::Done | Outcome::Retried => {
                        if let Err(error) = rest.delete_message(chat, id).await {
                            log("status cleanup", chat, &error);
                        }
                    }
                    _ => {
                        let text = first_chunk(&ending_markdown(&outcome, &screen, self.now()));
                        if let Err(error) = rest
                            .edit_message(chat, id, &text, Some(&plain_keyboard()))
                            .await
                        {
                            log("status finale", chat, &error);
                        }
                    }
                }
            }
        }
        let settled = self.deliver_answer(chat, &screen, answer).await;
        self.retire(conversation);
        settled
    }

    fn retire(&self, conversation: &str) {
        let mut inner = self.inner();
        if matches!(inner.turns.get(conversation), Some(Turn::Done)) {
            inner.turns.remove(conversation);
        }
    }

    async fn deliver_answer(&self, chat: &str, screen: &Screen, answer: Option<&str>) -> Finish {
        let Some(answer) = answer.filter(|text| !text.trim().is_empty()) else {
            return Finish::Durable;
        };
        let chunks = crate::render::split_markdown(answer, PREVIEW_LIMIT);
        if chunks.is_empty() {
            return Finish::Durable;
        }
        let mut landed = 0usize;
        if let (Some(id), Some(rest)) = (screen.reply_id, self.rest.as_ref()) {
            self.hold(chat).await;
            match rest
                .edit_message(chat, id, &chunks[0], Some(&plain_keyboard()))
                .await
            {
                Ok(_) => landed = 1,
                Err(error) => {
                    log("reply finale", chat, &error);
                    return Finish::Durable;
                }
            }
        }
        for chunk in chunks.iter().skip(landed) {
            match self.send_chunk(chat, chunk).await {
                Ok(()) => landed += 1,
                Err(error) => {
                    log("reply", chat, &error);
                    return if landed > 0 {
                        Finish::Landed
                    } else {
                        Finish::Durable
                    };
                }
            }
        }
        Finish::Landed
    }
}

pub fn drive(
    live: Arc<LiveView>,
    chat: String,
    conversation: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(TICK_MILLIS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if !live.live_turn(&conversation) {
                return;
            }
            live.publish(&chat, &conversation).await;
            live.typing(&chat).await;
        }
    })
}

pub fn callback_for<'a>(
    live: Arc<LiveView>,
    conversation: String,
) -> crate::runner::ProgressFn<'a> {
    Box::new(move |row: &Value| live.observe(&conversation, row))
}
