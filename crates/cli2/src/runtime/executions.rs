//! Live executions owned by a runtime instance.
//!
//! An execution runs on its own thread, independent of any client connection.
//! Its events are buffered here (bounded) so clients can observe it with a
//! cursor and reconnect; the *durable* outcome is recorded in the journal by
//! the engine, never by this registry.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use cli2_core::{ExecutionResult, Step, StepResult, Verification};
use cli2_execution::{CancelToken, StreamKind};
use cli2_protocol::{Event, EventData, Lifecycle, Stream, EVENT_SCHEMA};

use crate::engine::ExecutionObserver;

/// Most output text kept in memory per execution for live observers. Older
/// output events are dropped (and reported as a gap); evidence keeps the rest.
pub const MAX_BUFFERED_OUTPUT: usize = 4 * 1024 * 1024;

pub struct ExecState {
    pub events: VecDeque<Event>,
    pub next_seq: u64,
    pub buffered_output: usize,
    /// Output events with `seq <= dropped_through` were dropped from memory.
    pub dropped_through: u64,
    pub lifecycle: Lifecycle,
    pub steps: Vec<StepResult>,
    pub verification: Option<Verification>,
    pub result: Option<ExecutionResult>,
    pub error: Option<String>,
    /// Per (step, stream): bytes already emitted, and an incomplete UTF-8 tail.
    offsets: BTreeMap<(usize, u8), (u64, Vec<u8>)>,
}

pub struct Execution {
    pub id: String,
    pub plan_id: String,
    pub request_id: String,
    pub approval_id: String,
    pub started_at: String,
    pub state: Mutex<ExecState>,
    pub changed: Condvar,
    cancel: Mutex<Option<CancelToken>>,
    cancel_requested: AtomicBool,
}

impl Execution {
    pub fn new(id: String, plan_id: String, request_id: String, approval_id: String) -> Arc<Self> {
        Arc::new(Self {
            id,
            plan_id,
            request_id,
            approval_id,
            started_at: cli2_core::now_rfc3339(),
            state: Mutex::new(ExecState {
                events: VecDeque::new(),
                next_seq: 1,
                buffered_output: 0,
                dropped_through: 0,
                lifecycle: Lifecycle::Running,
                steps: vec![],
                verification: None,
                result: None,
                error: None,
                offsets: BTreeMap::new(),
            }),
            changed: Condvar::new(),
            cancel: Mutex::new(None),
            cancel_requested: AtomicBool::new(false),
        })
    }

    /// Called by the executing thread once its engine exists.
    pub fn set_cancel_token(&self, token: CancelToken) {
        if self.cancel_requested.load(Ordering::SeqCst) {
            token.cancel();
        }
        *self.cancel.lock().unwrap() = Some(token);
    }

    /// Ask the execution to stop. Returns whether it was still running.
    pub fn request_cancel(&self) -> bool {
        let mut st = self.state.lock().unwrap();
        if st.lifecycle != Lifecycle::Running && st.lifecycle != Lifecycle::Cancelling {
            return false;
        }
        st.lifecycle = Lifecycle::Cancelling;
        drop(st);
        self.cancel_requested.store(true, Ordering::SeqCst);
        if let Some(t) = self.cancel.lock().unwrap().as_ref() {
            t.cancel();
        }
        self.changed.notify_all();
        true
    }

    pub fn push(&self, data: EventData) {
        let mut st = self.state.lock().unwrap();
        Self::push_locked(&mut st, &self.id, data);
        drop(st);
        self.changed.notify_all();
    }

    pub fn push_locked_pub(st: &mut ExecState, id: &str, data: EventData) {
        Self::push_locked(st, id, data)
    }

    fn push_locked(st: &mut ExecState, id: &str, data: EventData) {
        let seq = st.next_seq;
        st.next_seq += 1;
        if let EventData::Output { text, .. } = &data {
            st.buffered_output += text.len();
        }
        st.events.push_back(Event {
            schema_version: EVENT_SCHEMA.into(),
            execution_id: id.to_string(),
            seq,
            at: cli2_core::now_rfc3339(),
            data,
        });
        // Drop the oldest *output* events once over budget; everything else is small and kept.
        while st.buffered_output > MAX_BUFFERED_OUTPUT {
            let Some(pos) = st
                .events
                .iter()
                .position(|e| matches!(e.data, EventData::Output { .. }))
            else {
                break;
            };
            let e = st.events.remove(pos).expect("position is valid");
            if let EventData::Output { text, .. } = &e.data {
                st.buffered_output -= text.len();
            }
            st.dropped_through = st.dropped_through.max(e.seq);
        }
    }

    fn output(&self, index: usize, stream: StreamKind, chunk: &[u8]) {
        let mut st = self.state.lock().unwrap();
        let key = (index, matches!(stream, StreamKind::Stderr) as u8);
        let (offset, carry) = st.offsets.entry(key).or_insert((0, Vec::new())).clone();
        let mut bytes = carry;
        bytes.extend_from_slice(chunk);
        // Hold back an incomplete trailing UTF-8 sequence for the next chunk.
        let keep = incomplete_utf8_tail(&bytes);
        let split = bytes.len() - keep;
        let tail = bytes.split_off(split);
        let consumed = bytes.len() as u64;
        st.offsets.insert(key, (offset + consumed, tail));
        if bytes.is_empty() {
            return;
        }
        let (text, lossy) = match String::from_utf8(bytes.clone()) {
            Ok(t) => (t, false),
            Err(_) => (String::from_utf8_lossy(&bytes).into_owned(), true),
        };
        let data = EventData::Output {
            index,
            stream: if key.1 == 0 {
                Stream::Stdout
            } else {
                Stream::Stderr
            },
            text,
            lossy,
            offset,
            bytes: bytes.len(),
        };
        Self::push_locked(&mut st, &self.id, data);
        drop(st);
        self.changed.notify_all();
    }

    /// Emit any held-back bytes of a step (a stream that ended mid-character).
    fn flush(&self, index: usize) {
        let mut pending = Vec::new();
        {
            let mut st = self.state.lock().unwrap();
            for s in [0u8, 1u8] {
                if let Some((off, tail)) = st.offsets.get_mut(&(index, s)) {
                    if !tail.is_empty() {
                        let t = std::mem::take(tail);
                        pending.push((s, *off, t.clone()));
                        *off += t.len() as u64;
                    }
                }
            }
            for (s, off, t) in &pending {
                let data = EventData::Output {
                    index,
                    stream: if *s == 0 {
                        Stream::Stdout
                    } else {
                        Stream::Stderr
                    },
                    text: String::from_utf8_lossy(t).into_owned(),
                    lossy: true,
                    offset: *off,
                    bytes: t.len(),
                };
                Self::push_locked(&mut st, &self.id, data);
            }
        }
        if !pending.is_empty() {
            self.changed.notify_all();
        }
    }

    /// Wait for events after `after_seq` (up to `wait`), returning at most `limit`.
    pub fn events_after(
        &self,
        after_seq: u64,
        wait: Duration,
        limit: usize,
    ) -> (Vec<Event>, u64, bool, Option<u64>) {
        let deadline = Instant::now() + wait;
        let mut st = self.state.lock().unwrap();
        loop {
            let finished = matches!(
                st.lifecycle,
                Lifecycle::Finished | Lifecycle::InterruptedUnknown
            );
            let has_new = st.events.iter().any(|e| e.seq > after_seq);
            if has_new || finished || wait.is_zero() {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            st = self.changed.wait_timeout(st, deadline - now).unwrap().0;
        }
        let events: Vec<Event> = st
            .events
            .iter()
            .filter(|e| e.seq > after_seq)
            .take(limit)
            .cloned()
            .collect();
        let last_seq = st.next_seq - 1;
        let delivered_all = events.last().is_none_or(|e| e.seq == last_seq);
        let finished = matches!(
            st.lifecycle,
            Lifecycle::Finished | Lifecycle::InterruptedUnknown
        ) && delivered_all;
        let gap = (st.dropped_through > after_seq).then_some(st.dropped_through);
        (events, last_seq, finished, gap)
    }

    /// The executing thread could not complete the execution. No outcome is claimed.
    pub fn fail(&self, message: String) {
        let mut st = self.state.lock().unwrap();
        st.error = Some(message);
        st.lifecycle = Lifecycle::InterruptedUnknown;
        drop(st);
        self.changed.notify_all();
    }
}

/// Length of an incomplete UTF-8 sequence at the end of `b` (0–3 bytes).
fn incomplete_utf8_tail(b: &[u8]) -> usize {
    for back in 1..=3.min(b.len()) {
        let byte = b[b.len() - back];
        if byte & 0b1100_0000 == 0b1000_0000 {
            continue; // continuation byte: keep looking for the lead
        }
        // Only a valid lead byte can start a character that is still incomplete.
        let need = match byte {
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            _ => return 0,
        };
        return if need > back { back } else { 0 };
    }
    0
}

/// Feeds an execution's events from the engine's observer callbacks.
pub struct EventSink(pub Arc<Execution>);

impl ExecutionObserver for EventSink {
    fn step_started(&self, step: &Step, pid: u32) {
        let Some(step) = step.as_process() else {
            return;
        };
        self.0.push(EventData::StepStarted {
            index: step.index,
            capability_id: step.capability_id.clone(),
            argv: step.argv(),
            working_directory: step.working_directory.clone(),
            pid,
        });
    }
    fn output(&self, index: usize, stream: StreamKind, chunk: &[u8]) {
        self.0.output(index, stream, chunk);
    }
    fn step_finished(&self, result: &StepResult) {
        self.0.flush(result.index);
        let mut st = self.0.state.lock().unwrap();
        st.steps.push(result.clone());
        drop(st);
        self.0.push(EventData::StepFinished {
            result: Box::new(result.clone()),
        });
    }
    fn verification(&self, v: &Verification) {
        self.0.state.lock().unwrap().verification = Some(v.clone());
        self.0.push(EventData::Verification {
            verification: v.clone(),
        });
    }
    fn finished(&self, result: &ExecutionResult) {
        let mut st = self.0.state.lock().unwrap();
        st.result = Some(result.clone());
        st.lifecycle = Lifecycle::Finished;
        Execution::push_locked(
            &mut st,
            &self.0.id,
            EventData::ExecutionFinished {
                result: Box::new(result.clone()),
            },
        );
        drop(st);
        self.0.changed.notify_all();
    }
}

/// All live executions of this runtime instance.
#[derive(Default)]
pub struct Registry {
    pub by_id: Mutex<HashMap<String, Arc<Execution>>>,
    /// approval_id → execution_id: an approval authorizes at most one execution.
    pub by_approval: Mutex<HashMap<String, String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_tail_detection() {
        assert_eq!(incomplete_utf8_tail("héllo".as_bytes()), 0);
        let e = "é".as_bytes(); // 2 bytes
        assert_eq!(incomplete_utf8_tail(&e[..1]), 1);
        let euro = "€".as_bytes(); // 3 bytes
        assert_eq!(incomplete_utf8_tail(&euro[..2]), 2);
        assert_eq!(incomplete_utf8_tail(&euro[..1]), 1);
        assert_eq!(incomplete_utf8_tail(euro), 0);
        assert_eq!(incomplete_utf8_tail(b"abc"), 0);
        assert_eq!(incomplete_utf8_tail(&[0xff]), 0);
    }

    #[test]
    fn multibyte_characters_split_across_chunks_are_reassembled() {
        let x = Execution::new("run_t".into(), "p".into(), "r".into(), "a".into());
        let sink = EventSink(x.clone());
        let euro = "€".as_bytes();
        sink.output(0, StreamKind::Stdout, &euro[..2]);
        sink.output(0, StreamKind::Stdout, &euro[2..]);
        sink.output(0, StreamKind::Stderr, b"err");
        let (events, last, finished, gap) = x.events_after(0, Duration::ZERO, 100);
        let texts: Vec<(Stream, String, bool, u64)> = events
            .iter()
            .filter_map(|e| match &e.data {
                EventData::Output {
                    stream,
                    text,
                    lossy,
                    offset,
                    ..
                } => Some((*stream, text.clone(), *lossy, *offset)),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            vec![
                (Stream::Stdout, "€".into(), false, 0),
                (Stream::Stderr, "err".into(), false, 0)
            ]
        );
        assert_eq!((last, finished, gap), (2, false, None));
    }

    #[test]
    fn output_beyond_the_live_buffer_is_reported_as_a_gap_not_silently_lost() {
        let x = Execution::new("run_t".into(), "p".into(), "r".into(), "a".into());
        let chunk = vec![b'x'; 1024 * 1024];
        for _ in 0..6 {
            x.output(0, StreamKind::Stdout, &chunk);
        }
        let (events, _, _, gap) = x.events_after(0, Duration::ZERO, 100);
        assert!(events.len() < 6);
        assert!(gap.is_some(), "the dropped range is reported");
        assert!(events.iter().all(|e| e.seq > gap.unwrap()));
    }
}
