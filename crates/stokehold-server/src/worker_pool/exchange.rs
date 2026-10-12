//! One worker's channel and the thread that owns it, for the worker's whole life:
//! a request is posted to the thread and the answer comes back on a channel, so a
//! read with no portable timeout still has one. This is the fix for the
//! per-request thread cost; see [`ChannelExchange`].


use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration};
use stokehold_worker::codec::RequestStage;
use stokehold_worker::codec::WorkerResponse;
use stokehold_worker::{BoundedChannel, MessageType, WorkerError};


/// Both directions of one worker's pipes, buffered.
///
/// Buffered because every frame is written as a header and then a payload and read the same
/// way: over an unbuffered `ChildStdin`/`ChildStdout` each of those calls is its own syscall,
/// and each read its own wakeup of the exchange thread. See [`BoundedChannel`].
pub(super) type Channel = BoundedChannel;

/// What the exchange thread is asked to do, and what the caller is told.
///
/// `WorkerChannel::request` and `send_frame` answer one exchange each, so the
/// `request_id` that matches a response lives inside the channel and the caller
/// only has to say which call it made. Both answer `None` for a response, because
/// the only exchange that produces one is [`Exchange::Request`] — the handshake is
/// already done by the time this thread starts.
pub(super) enum Exchange {
    /// Send one request and collect its whole response.
    ///
    /// The head arrives **already encoded**. The pool's translation borrows the CGI environment —
    /// that is what `borrowed_request` is for — and a borrow cannot travel to this thread, because
    /// on a timeout the thread outlives the call that made it. So the pool serializes the head and
    /// sends the bytes, and the worker cannot tell which side of the boundary encoded them.
    Request { head: Vec<u8>, body: Vec<u8> },    /// Send one frame from an already-encoded payload.
    Send {
        message_type: MessageType,
        request_id: u32,
        payload: Vec<u8>,
        extra_flags: u16,
    },
}

impl Exchange {
    /// Run this exchange on a live channel.
    ///
    /// `on_stage` is called as the request moves from writing to reading, so a
    /// failure can be attributed to the half it happened in. Only
    /// [`Exchange::Request`] has two phases; every other exchange reports `Reading`
    /// once, because a control or a frame write has nothing to read and its result
    /// is not retried.
    fn run(
        self,
        channel: &mut Channel,
        on_stage: &mut dyn FnMut(RequestStage),
    ) -> std::result::Result<Option<WorkerResponse>, WorkerError> {
        match self {
            Exchange::Request { head, body } => {
                // The stage is reported by the channel as it moves from writing to reading, which
                // is the only place that can know where a failure happened.
                let result = channel.request_encoded(head, &body, &mut *on_stage);
                result.map(Some)
            }
            Exchange::Send {
                message_type,
                request_id,
                payload,
                extra_flags,
            } => {
                on_stage(RequestStage::Reading);
                channel.send_frame(message_type, request_id, &payload, extra_flags)?;
                Ok(None)
            }
        }
    }
}

/// A worker's pipes, and the thread that owns them.
///
/// **This is the fix for the per-request thread cost.** A pipe read has no
/// portable timeout on any platform stokehold runs on, so the deadline has to be
/// enforced by something outside the read: the worker process is killed, which is
/// what makes the read return. A thread is what can then be given up on — but it
/// does *not* have to be a new thread per request. One thread per worker, alive
/// for the worker's whole life, does the same job and is created once: a request
/// is posted to it on a channel, and the deadline is a `recv_timeout` on that
/// channel. Measured on the trivial handler this was worth about half the arm's
/// throughput (worker 20,422 rps vs `spawn_cgi` 29,950 at concurrency 32).
///
/// Two consequences, both deliberate:
///
/// - **A timeout still leaks this thread and its pipes.** The thread is blocked
///   in `read` and will not return, so nothing may be sent to it again — the
///   caller drops the whole exchange and kills the worker, and the process exit
///   is what finally ends the read. Sending a second request to a worker whose
///   first one timed out is the one thing that must not happen, and dropping the
///   exchange is what makes it impossible.
/// - **A write error means the thread has ended.** The sender is kept for the
///   whole call rather than re-sent, so nothing is delivered to a stale request's
///   answer.
#[derive(Debug)]
pub(super) struct ChannelExchange {
    work: Option<Sender<Exchange>>,
    /// The thread's answers, exactly one per exchange.
    answers: mpsc::Receiver<Answer>,
    /// Set once the thread has ended, so a write to a dead thread is reported as
    /// "closed" rather than as an unexplained send failure.
    ended: bool,
}

/// The thread's answers, exactly one per exchange.
///
/// A failure carries the [`RequestStage`] it happened in, because that is what says
/// whether the request could have been delivered.
type Answer = std::result::Result<Option<WorkerResponse>, (RequestStage, WorkerError)>;

impl ChannelExchange {
    /// Start the exchange thread that owns a freshly handshaken worker's pipes.
    ///
    /// Takes ownership of the channel and never gives it back: the thread ends
    /// when the channel is dropped, which is what `close` arranges.
    pub fn start(mut channel: Channel) -> Self {
        let (work_tx, work_rx) = mpsc::channel::<Exchange>();
        let (answer_tx, answers) = mpsc::channel();

        std::thread::spawn(move || {
            while let Ok(exchange) = work_rx.recv() {
                // The stage is tracked here, in the only place that can know it: a
                // failure is stamped with the half of the request it happened in.
                // The default is the send phase, so a failure before the callback
                // fires is still attributed to the write.
                let mut stage = RequestStage::Sending;
                let result = exchange.run(&mut channel, &mut |reported| stage = reported);
                // The receiver is gone only if the caller dropped this exchange,
                // which happens on a timeout. Then the answer is dropped with the
                // thread — the caller has already decided not to wait.
                if answer_tx.send(result.map_err(|source| (stage, source))).is_err() {
                    break;
                }
            }
        });

        ChannelExchange {
            work: Some(work_tx),
            answers,
            ended: false,
        }
    }

    /// Run one request, giving up after `timeout`.
    ///
    /// One copy of the request body per request, and it is worth being honest about why:
    /// the work item is moved to the exchange thread, so it must own its bytes, and a
    /// channel of borrowed data would need a lifetime the standard library's channels
    /// cannot express. Removing that copy means `Request::body` becoming something cheap to
    /// share (`Arc<[u8]>`), which is a change to the FastCGI layer every backend shares —
    /// not worth it while the cost is one `memcpy` the previous design also paid. What is
    /// gone is the thread, which was the measured cost.
    ///
    /// The *head* is not copied by the channel either: it arrives already encoded, and the channel
    /// writes it straight out. What it costs is one `Vec` per request, and that is the price of
    /// crossing this thread boundary at all — the body copy below pays the same price for the same
    /// reason. Building the JSON is what the pool used to do, but *borrowing* the header for the
    /// worker is what it does now, and nine `String`s became one buffer.
    pub fn request(
        &mut self,
        head: Vec<u8>,
        body: &[u8],
        timeout: Duration,
    ) -> std::result::Result<WorkerResponse, (RequestStage, WorkerError)> {
        let exchange = Exchange::Request {
            head,
            body: body.to_vec(),
        };

        match self.round_trip(exchange, timeout) {
            Ok(Some(response)) => Ok(response),
            // `request` always answers with a response, so a `None` here would be
            // the thread misbehaving rather than a protocol condition.
            Ok(None) => Err((
                RequestStage::Reading,
                WorkerError::Closed(
                    "the exchange thread answered without a response".to_string(),
                ),
            )),
            Err(failure) => Err(failure),
        }
    }

    /// Write one frame, without waiting for anything to come back.
    ///
    /// The payload arrives already encoded — `CONTROL drain` is a fixed byte
    /// string — so this needs no serializer and does not force one on this crate.
    /// A control frame is unsolicited, so its id is not a request id and is not
    /// correlated: the answer is drained because the thread produces one per
    /// exchange, and leaving it in the channel would make the *next* request read
    /// it and fail for a reason that has nothing to do with the request.
    pub fn send(
        &mut self,
        message_type: MessageType,
        request_id: u32,
        payload: &[u8],
        extra_flags: u16,
    ) -> std::result::Result<(), WorkerError> {
        let exchange = Exchange::Send {
            message_type,
            request_id,
            payload: payload.to_vec(),
            extra_flags,
        };

        match self.round_trip(exchange, Duration::from_secs(1)).map_err(|(_, source)| source)? {
            // `Send` writes a frame and reads nothing, so its answer carries no
            // response and never can.
            None => Ok(()),
            Some(_) => Err(WorkerError::Closed(
                "the exchange thread answered a frame write with a response".to_string(),
            )),
        }
    }
    /// End the exchange thread by closing the worker's stdin.
    ///
    /// Dropping the sender ends the thread's `recv`, and dropping the channel then
    /// closes the pipes. This is the shutdown signal that cannot fail, and it is
    /// deliberately not a method on `WorkerChannel`: the worker sees end-of-file on
    /// its next read and exits under its own control.
    pub fn close(self) {
        let ChannelExchange { work, answers, .. } = self;
        drop(work);
        drop(answers);
    }

    /// Post one exchange and wait for its answer.
    ///
    /// Exactly one exchange is in flight per worker at a time, so the single
    /// answer on `answers` is necessarily this one's and no request id needs to
    /// travel with it to say so. That is a property of the slot's lock, and it is
    /// why this is not a general-purpose multiplexer.
    fn round_trip(
        &mut self,
        exchange: Exchange,
        timeout: Duration,
    ) -> std::result::Result<Option<WorkerResponse>, (RequestStage, WorkerError)> {
        // Fail fast once the thread is known to be gone. Without this the send below
        // would succeed — the sender is still alive, only the answer end is closed —
        // and the call would block until the timeout for a worker that cannot answer.
        if self.ended {
            return Err((
                RequestStage::Reading,
                WorkerError::Closed("the worker's exchange thread has ended".to_string()),
            ));
        }

        let Some(work) = self.work.as_ref() else {
            return Err((
                RequestStage::Sending,
                WorkerError::Closed("the worker's exchange thread has already ended".to_string()),
            ));
        };

        work.send(exchange).map_err(|_| {
            self.ended = true;
            (
                RequestStage::Sending,
                WorkerError::Closed("the worker's exchange thread has ended".to_string()),
            )
        })?;

        match self.answers.recv_timeout(timeout) {
            Ok(result) => result,
            // Timed out. The thread is blocked in a read on this worker, so the
            // caller has to kill the worker: that is what makes the read return so
            // the thread can exit and drop the pipes.
            Err(RecvTimeoutError::Timeout) => Err((RequestStage::Reading, WorkerError::Timeout(timeout))),
            Err(RecvTimeoutError::Disconnected) => {
                self.ended = true;
                Err((
                    RequestStage::Reading,
                    WorkerError::Closed(
                        "the exchange thread ended without answering".to_string(),
                    ),
                ))
            }
        }
    }
}
