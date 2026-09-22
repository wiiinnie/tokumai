//! How messages cross a transport that carries small, unreliable, unordered messages — the
//! mixnet. Independent of what the messages say: they are sealed already (see `wire`), so
//! this layer only moves opaque bytes, and a relay that sees the frames learns nothing more
//! than their sizes.
//!
//! Every frame names its exchange by a random 16-byte id. A message up to [`CHUNK`] bytes
//! goes in one frame; a bigger one in parts, each acknowledged. A reply up to [`CHUNK`]
//! comes back whole; a bigger one (a generated picture), or any reply to a message sent in
//! parts, is kept here and fetched chunk by chunk. Everything is individually retryable:
//! a frame sent twice gets the same answer, and an exchange whose request is still being
//! answered gets no answer at all (the app waits and asks again), so the enclave is never
//! asked the same question twice at once.
//!
//! App → enclave:
//! - `1 id msg` — a whole message.
//! - `2 id seq:u16 n:u16 part` — part `seq` of `n`.
//! - `3 id seq:u16` — fetch chunk `seq` of the kept reply.
//!
//! Enclave → app:
//! - `11 id reply` — the whole reply.
//! - `12 id seq:u16` — part `seq` arrived; others are still missing.
//! - `13 id n:u16 bytes:u32` — the reply is kept here, in `n` chunks.
//! - `14 id seq:u16 chunk` — one chunk of it.
//! - `15 id text` — this exchange cannot go on (too big, forgotten); start over.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Payload bytes per frame. ~64 KB is ~32 Sphinx packets: small enough that one lost
/// packet costs little, big enough that a question fits in one frame.
pub const CHUNK: usize = 64 * 1024;
/// How many pieces are asked for at once. Everything in flight is in the mixnet at the
/// same time, and a burst too large is not delivered faster — it is delivered late, its
/// acknowledgements come late with it, and the sender begins repeating itself.
/// Pieces on their way at once. Measured against the live mixnet (2026-09-23, a 2 MB
/// reply): 8, 16 and 24 all land between 86 and 108 KB/s, so the number matters far less
/// than not asking twice for what is already on its way. Twelve sits in the middle and
/// keeps the reply-SURB budget of one round modest.
const WINDOW_DEFAULT: usize = 12;

/// The window, with a way to try other values against the real mixnet
/// (`TOKUMAI_CHUNK_WINDOW`) rather than guess at them.
pub fn window() -> usize {
    std::env::var("TOKUMAI_CHUNK_WINDOW").ok().and_then(|v| v.parse().ok()).filter(|n| (1..=64).contains(n)).unwrap_or(WINDOW_DEFAULT)
}
/// Largest message accepted in parts (a request with attachments), and so the most parts.
/// The enclave's own limit on a request (`policy::MAX_REQUEST_BYTES`) must not exceed it.
pub const MAX_MESSAGE: usize = 48 * 1024 * 1024;
/// How long a half-sent message, or a reply not yet fetched, is kept.
const KEEP: Duration = Duration::from_secs(30 * 60);
/// Bytes kept across all exchanges; beyond it the oldest go first.
const MAX_KEPT: usize = 256 * 1024 * 1024;

const WHOLE: u8 = 1;
const PART: u8 = 2;
const FETCH: u8 = 3;
const R_WHOLE: u8 = 11;
const R_ACK: u8 = 12;
const R_KEPT: u8 = 13;
const R_CHUNK: u8 = 14;
const R_FAIL: u8 = 15;

pub type Id = [u8; 16];

fn frame(kind: u8, id: &Id, fields: &[&[u8]]) -> Vec<u8> {
    let mut f = Vec::with_capacity(17 + fields.iter().map(|x| x.len()).sum::<usize>());
    f.push(kind);
    f.extend_from_slice(id);
    for x in fields {
        f.extend_from_slice(x);
    }
    f
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn id_of(b: &[u8]) -> Option<Id> {
    b.get(1..17)?.try_into().ok()
}

/// What is kept of one exchange.
enum Held {
    /// Parts of a message still arriving.
    Arriving { parts: Vec<Option<Vec<u8>>>, bytes: usize, at: Instant },
    /// Being answered right now.
    Answering,
    /// The answer, whole (short replies) or in chunks.
    Answered { chunks: Vec<Vec<u8>>, bytes: usize, whole: bool, at: Instant },
}

impl Held {
    fn bytes(&self) -> usize {
        match self {
            Held::Arriving { bytes, .. } | Held::Answered { bytes, .. } => *bytes,
            Held::Answering => 0,
        }
    }
    fn at(&self) -> Option<Instant> {
        match self {
            Held::Arriving { at, .. } | Held::Answered { at, .. } => Some(*at),
            Held::Answering => None,
        }
    }
}

/// The enclave's side: turns frames into whole messages for `answer`, and its replies back
/// into frames.
#[derive(Default)]
pub struct Frames {
    held: Mutex<HashMap<Id, Held>>,
}

impl Frames {
    /// One frame in; the frame to send back, or `None` when there is nothing to say yet.
    pub async fn handle<F, Fut>(&self, f: &[u8], answer: F) -> Option<Vec<u8>>
    where
        F: FnOnce(Vec<u8>) -> Fut,
        Fut: Future<Output = Vec<u8>>,
    {
        let id = id_of(f)?;
        let (message, parted) = match f[0] {
            WHOLE => match self.state(&id) {
                Some(Seen::Answered) => return self.reply_again(&id),
                Some(Seen::Busy) => return None,
                _ => (f[17..].to_vec(), false),
            },
            PART => match self.part(&id, f) {
                Ok(Some(message)) => (message, true),
                Ok(None) => return self.reply_again(&id).or_else(|| Some(frame(R_ACK, &id, &[f.get(17..19)?]))),
                Err(e) => return Some(frame(R_FAIL, &id, &[e.as_bytes()])),
            },
            FETCH => return Some(self.fetch(&id, u16_at(f, 17)? as usize)),
            _ => return None,
        };
        if !self.claim(&id) {
            return None;
        }
        let reply = answer(message).await;
        Some(self.keep(&id, reply, parted))
    }

    fn state(&self, id: &Id) -> Option<Seen> {
        match self.held.lock().ok()?.get(id)? {
            Held::Answered { .. } => Some(Seen::Answered),
            Held::Answering => Some(Seen::Busy),
            Held::Arriving { .. } => Some(Seen::Arriving),
        }
    }

    /// Take one part; the whole message once the last missing part is in.
    fn part(&self, id: &Id, f: &[u8]) -> Result<Option<Vec<u8>>, String> {
        let (Some(seq), Some(n)) = (u16_at(f, 17), u16_at(f, 19)) else { return Err("malformed part".into()) };
        let (seq, n) = (seq as usize, n as usize);
        let data = &f[21..];
        if n < 2 || seq >= n || n > MAX_MESSAGE.div_ceil(CHUNK) || data.len() > CHUNK || data.is_empty() {
            return Err("this message is too big, or its parts do not fit together".into());
        }
        let mut held = self.held.lock().map_err(|_| "unavailable")?;
        let entry = held.entry(*id).or_insert_with(|| Held::Arriving { parts: vec![None; n], bytes: 0, at: Instant::now() });
        let Held::Arriving { parts, bytes, .. } = entry else { return Ok(None) };
        if parts.len() != n {
            return Err("this message's parts do not fit together".into());
        }
        if parts[seq].is_none() {
            *bytes += data.len();
            parts[seq] = Some(data.to_vec());
        }
        if parts.iter().any(|p| p.is_none()) {
            drop(held);
            self.trim();
            return Ok(None);
        }
        let message: Vec<u8> = parts.iter_mut().flat_map(|p| p.take().unwrap_or_default()).collect();
        held.remove(id);
        Ok(Some(message))
    }

    fn claim(&self, id: &Id) -> bool {
        let Ok(mut held) = self.held.lock() else { return false };
        if matches!(held.get(id), Some(Held::Answering | Held::Answered { .. })) {
            return false;
        }
        held.insert(*id, Held::Answering);
        true
    }

    fn keep(&self, id: &Id, reply: Vec<u8>, parted: bool) -> Vec<u8> {
        let whole = !parted && reply.len() <= CHUNK;
        let bytes = reply.len();
        let mut chunks: Vec<Vec<u8>> = if whole { vec![reply] } else { reply.chunks(CHUNK).map(|c| c.to_vec()).collect() };
        if chunks.is_empty() {
            chunks.push(Vec::new());
        }
        let out = if whole { frame(R_WHOLE, id, &[&chunks[0]]) } else { kept_frame(id, chunks.len(), bytes) };
        if let Ok(mut held) = self.held.lock() {
            held.insert(*id, Held::Answered { chunks, bytes, whole, at: Instant::now() });
        }
        self.trim();
        out
    }

    fn reply_again(&self, id: &Id) -> Option<Vec<u8>> {
        let held = self.held.lock().ok()?;
        match held.get(id)? {
            Held::Answered { chunks, whole: true, .. } => Some(frame(R_WHOLE, id, &[&chunks[0]])),
            Held::Answered { chunks, bytes, .. } => Some(kept_frame(id, chunks.len(), *bytes)),
            _ => None,
        }
    }

    fn fetch(&self, id: &Id, seq: usize) -> Vec<u8> {
        let held = self.held.lock();
        match held.as_ref().ok().and_then(|h| h.get(id)) {
            Some(Held::Answered { chunks, .. }) if seq < chunks.len() => frame(R_CHUNK, id, &[&(seq as u16).to_be_bytes(), &chunks[seq]]),
            _ => frame(R_FAIL, id, &[b"this reply is no longer kept"]),
        }
    }

    /// Forget what is old, then the oldest until what is kept fits.
    fn trim(&self) {
        let Ok(mut held) = self.held.lock() else { return };
        held.retain(|_, h| h.at().map(|at| at.elapsed() < KEEP).unwrap_or(true));
        let mut total: usize = held.values().map(|h| h.bytes()).sum();
        while total > MAX_KEPT {
            let Some(oldest) = held.iter().filter_map(|(k, h)| h.at().map(|at| (*k, at))).min_by_key(|(_, at)| *at).map(|(k, _)| k) else { break };
            total -= held.remove(&oldest).map(|h| h.bytes()).unwrap_or(0);
        }
    }
}

enum Seen {
    Arriving,
    Busy,
    Answered,
}

fn kept_frame(id: &Id, n: usize, bytes: usize) -> Vec<u8> {
    frame(R_KEPT, id, &[&(n as u16).to_be_bytes(), &(bytes as u32).to_be_bytes()])
}

/// The app's side of one exchange: which frames still need sending, and the reply once it
/// is complete. The transport sends [`Exchange::due`], feeds every answer to
/// [`Exchange::accept`], and calls `due` again whenever something arrives — it hands back
/// only what is still missing AND is not already on its way.
///
/// That last part is the whole of the flow control. Asking again for a piece that is
/// already in the mixnet does not make it come sooner; it doubles the traffic, delays the
/// acknowledgements, and makes the sender repeat itself in turn. A piece is asked for
/// again only once [`RETRY`] has passed without it.
pub struct Exchange {
    pub id: Id,
    message: Vec<u8>,
    /// Parts not yet acknowledged (for a message sent in parts).
    unacked: HashSet<usize>,
    parts: usize,
    /// The kept reply's chunks, once the enclave said how many.
    chunks: Option<Vec<Option<Vec<u8>>>>,
    /// When each piece was last asked for, so one in flight is not asked for again.
    asked: Vec<Option<Instant>>,
    total: usize,
}

/// How long a piece may be in flight before it is asked for again. Long enough that a
/// mixnet doing its work is not mistaken for a mixnet that lost something.
const RETRY: Duration = Duration::from_secs(25);

/// Where an exchange stands after one answer.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// More to send or fetch: call `due` again.
    Going,
    /// The whole reply.
    Done(Vec<u8>),
}

impl Exchange {
    pub fn new(message: Vec<u8>) -> Exchange {
        let parts = if message.len() <= CHUNK { 1 } else { message.len().div_ceil(CHUNK) };
        Exchange { id: rand::random(), unacked: (0..parts).collect(), parts, message, chunks: None, asked: vec![None; parts], total: 0 }
    }

    /// The frames to send now: enough to keep `window` of them on their way, and nothing
    /// that is already on its way (see the note on the struct).
    pub fn due(&mut self, window: usize) -> Vec<Vec<u8>> {
        self.due_at(window, Instant::now())
    }

    /// `due`, with the clock given — so the flow control can be tested without waiting.
    pub fn due_at(&mut self, window: usize, now: Instant) -> Vec<Vec<u8>> {
        if self.chunks.is_some() {
            let missing: Vec<usize> = (0..self.chunks.as_ref().expect("checked").len()).filter(|&i| self.chunks.as_ref().expect("checked")[i].is_none()).collect();
            let wanted = self.to_send(&missing, window, now);
            return wanted.into_iter().map(|i| frame(FETCH, &self.id, &[&(i as u16).to_be_bytes()])).collect();
        }
        if self.parts == 1 {
            self.asked[0] = Some(now);
            return vec![frame(WHOLE, &self.id, &[&self.message])];
        }
        let mut seqs: Vec<usize> = self.unacked.iter().copied().collect();
        seqs.sort_unstable();
        // Every part is acknowledged but the reply has not come: ask again with the last
        // part — the enclave answers a part of a finished message with where its reply is.
        if seqs.is_empty() {
            seqs.push(self.parts - 1);
        }
        self.to_send(&seqs, window, now)
            .into_iter()
            .map(|i| {
                let end = ((i + 1) * CHUNK).min(self.message.len());
                frame(PART, &self.id, &[&(i as u16).to_be_bytes(), &(self.parts as u16).to_be_bytes(), &self.message[i * CHUNK..end]])
            })
            .collect()
    }

    /// Which of `missing` to put on their way now: those never asked for, or asked for
    /// long enough ago to count as lost, up to a `window` in flight altogether.
    fn to_send(&mut self, missing: &[usize], window: usize, now: Instant) -> Vec<usize> {
        if self.asked.len() < missing.iter().copied().max().map(|m| m + 1).unwrap_or(0) {
            self.asked.resize(missing.iter().copied().max().unwrap_or(0) + 1, None);
        }
        let in_flight = missing.iter().filter(|&&i| self.asked[i].is_some_and(|t| now.duration_since(t) < RETRY)).count();
        let room = window.saturating_sub(in_flight);
        let send: Vec<usize> = missing
            .iter()
            .copied()
            .filter(|&i| self.asked[i].is_none_or(|t| now.duration_since(t) >= RETRY))
            .take(room)
            .collect();
        for &i in &send {
            self.asked[i] = Some(now);
        }
        send
    }

    /// Whether this answer is about this exchange.
    pub fn owns(&self, f: &[u8]) -> bool {
        id_of(f) == Some(self.id)
    }

    /// One answer in. A frame of another exchange is ignored.
    pub fn accept(&mut self, f: &[u8]) -> Result<Step, String> {
        if !self.owns(f) {
            return Ok(Step::Going);
        }
        match f[0] {
            R_WHOLE => Ok(Step::Done(f[17..].to_vec())),
            R_ACK => {
                if let Some(seq) = u16_at(f, 17) {
                    self.unacked.remove(&(seq as usize));
                }
                Ok(Step::Going)
            }
            R_KEPT => {
                let (Some(n), Some(bytes)) = (u16_at(f, 17), u32_at(f, 19)) else { return Err("malformed answer".into()) };
                if n == 0 || bytes as usize > MAX_REPLY || n as usize != (bytes as usize).div_ceil(CHUNK).max(1) {
                    return Err("malformed answer".into());
                }
                self.unacked.clear();
                if self.chunks.is_none() {
                    self.chunks = Some(vec![None; n as usize]);
                    // Nothing of the reply has been asked for yet; the numbers from here
                    // on count its pieces, not the request's.
                    self.asked = vec![None; n as usize];
                    self.total = bytes as usize;
                }
                Ok(Step::Going)
            }
            R_CHUNK => {
                let seq = u16_at(f, 17).ok_or("malformed answer")? as usize;
                let Some(chunks) = self.chunks.as_mut() else { return Ok(Step::Going) };
                if seq < chunks.len() && chunks[seq].is_none() {
                    chunks[seq] = Some(f[19..].to_vec());
                }
                if chunks.iter().any(|c| c.is_none()) {
                    return Ok(Step::Going);
                }
                let reply: Vec<u8> = chunks.iter_mut().flat_map(|c| c.take().unwrap_or_default()).collect();
                if reply.len() != self.total {
                    return Err("the reply did not add up".into());
                }
                Ok(Step::Done(reply))
            }
            R_FAIL => Err(String::from_utf8_lossy(&f[17..]).into_owned()),
            _ => Ok(Step::Going),
        }
    }

    /// Whether the reply is being fetched in chunks (for progress: `(have, of)`).
    pub fn progress(&self) -> Option<(usize, usize)> {
        self.chunks.as_ref().map(|c| (c.iter().filter(|x| x.is_some()).count(), c.len()))
    }
}

/// The largest reply an app accepts (a 4K picture is ~5 MB of base64, sealed).
pub const MAX_REPLY: usize = 32 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    async fn echo(m: Vec<u8>) -> Vec<u8> {
        let mut r = b"re:".to_vec();
        r.extend(m);
        r
    }

    /// Run one exchange to its end, the transport losing `loss_pct` of all frames, both ways.
    async fn run(frames: &Frames, message: Vec<u8>, loss_pct: u64) -> Vec<u8> {
        let mut ex = Exchange::new(message);
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut lost = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % 100 < loss_pct
        };
        // A clock of its own: the retry is measured in seconds, and a test must not wait
        // them out. Each turn of the loop is a second.
        let start = Instant::now();
        for turn in 0..10_000 {
            let now = start + Duration::from_secs(turn);
            for f in ex.due_at(window(), now) {
                if lost() {
                    continue;
                }
                let Some(answer) = frames.handle(&f, echo).await else { continue };
                if lost() {
                    continue;
                }
                if let Step::Done(reply) = ex.accept(&answer).unwrap() {
                    return reply;
                }
            }
        }
        panic!("the exchange never finished");
    }

    #[tokio::test]
    async fn short_and_long_messages_arrive_whole_even_when_frames_are_lost() {
        let frames = Frames::default();
        assert_eq!(run(&frames, b"hello".to_vec(), 0).await, b"re:hello");
        let big: Vec<u8> = (0..(5 * CHUNK + 123)).map(|i| (i % 251) as u8).collect();
        let mut want = b"re:".to_vec();
        want.extend(&big);
        assert_eq!(run(&frames, big.clone(), 0).await, want);
        assert_eq!(run(&frames, big, 30).await, want);
        assert_eq!(run(&frames, b"short".to_vec(), 50).await, b"re:short");
    }

    #[tokio::test]
    async fn a_resent_frame_gets_the_same_answer_and_the_question_is_asked_once() {
        let frames = Frames::default();
        let asked = std::sync::atomic::AtomicUsize::new(0);
        let mut ex = Exchange::new(b"once".to_vec());
        let f = ex.due(1).remove(0);
        let count = |m: Vec<u8>| {
            asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            echo(m)
        };
        let a = frames.handle(&f, count).await.unwrap();
        let b = frames.handle(&f, count).await.unwrap();
        assert_eq!(a, b);
        assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_question_still_being_answered_gets_no_second_answer() {
        let frames = Frames::default();
        let mut ex = Exchange::new(b"slow".to_vec());
        let f = ex.due(1).remove(0);
        assert!(frames.claim(&ex.id));
        assert!(frames.handle(&f, echo).await.is_none(), "the resend waits for the first answer");
    }

    #[tokio::test]
    async fn parts_that_do_not_fit_together_are_refused() {
        let frames = Frames::default();
        let id: Id = [7; 16];
        let bad = frame(PART, &id, &[&0u16.to_be_bytes(), &60_000u16.to_be_bytes(), b"x"]);
        let answer = frames.handle(&bad, echo).await.unwrap();
        assert_eq!(answer[0], R_FAIL);
        let fetch = frame(FETCH, &[8; 16], &[&0u16.to_be_bytes()]);
        assert_eq!(frames.handle(&fetch, echo).await.unwrap()[0], R_FAIL);
        assert!(frames.handle(b"", echo).await.is_none());
    }

    /// The heart of it: a piece already on its way is not asked for again, however often
    /// the transport asks what to send — until it is old enough to count as lost.
    #[tokio::test]
    async fn a_piece_already_on_its_way_is_not_asked_for_twice() {
        let frames = Frames::default();
        // A reply of eight pieces, so there is something to fetch.
        let mut ex = Exchange::new(b"give me a long answer".to_vec());
        let f = ex.due(4).remove(0);
        let long = |_m: Vec<u8>| async { vec![7u8; CHUNK * 8] };
        let kept = frames.handle(&f, long).await.unwrap();
        assert_eq!(ex.accept(&kept).unwrap(), Step::Going);

        let now = Instant::now();
        let first = ex.due_at(4, now);
        assert_eq!(first.len(), 4, "four on their way");
        // Asked again straight away: nothing, they are all still in flight.
        assert!(ex.due_at(4, now).is_empty(), "nothing is asked for twice");
        assert!(ex.due_at(4, now + Duration::from_secs(5)).is_empty());

        // One arrives: room for exactly one more.
        let answer = frames.handle(&first[0], long).await.unwrap();
        assert_eq!(ex.accept(&answer).unwrap(), Step::Going);
        assert_eq!(ex.due_at(4, now + Duration::from_secs(6)).len(), 1);

        // The ones that never came back are asked for again once they are old enough.
        assert_eq!(ex.due_at(4, now + RETRY + Duration::from_secs(1)).len(), 3);
    }

}
