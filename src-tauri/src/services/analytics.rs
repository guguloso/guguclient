//! Product analytics (`analytics:track`). Port of the intake side of
//! `AnalyticsService`: events are accepted (returning `true`, like the
//! original), dropped when `allow-analytics-collection` is off, and queued in
//! a bounded in-memory buffer.
//!
//! Pending: the batched upload to the Lunar backend (exponential backoff,
//! installation/session ids, age gate).

use std::collections::VecDeque;

use parking_lot::Mutex;
use serde_json::{json, Value};

use crate::ipc::Router;

const MAX_QUEUED_EVENTS: usize = 1000;

#[derive(Default)]
pub struct AnalyticsQueue {
    events: Mutex<VecDeque<Value>>,
}

impl AnalyticsQueue {
    pub fn push(&self, event: String, payload: Value) {
        let mut events = self.events.lock();
        if events.len() == MAX_QUEUED_EVENTS {
            events.pop_front();
        }
        events.push_back(json!({ "type": event, "data": payload }));
    }
}

pub fn register(router: &mut Router) {
    router.handle("analytics:track", |ctx, mut args| async move {
        if ctx.state.settings.get_bool("allow-analytics-collection") {
            let event: String = args.get(0)?;
            let payload = args.take(1);
            ctx.state.analytics.push(event, payload);
        }
        Ok(true)
    });
}
