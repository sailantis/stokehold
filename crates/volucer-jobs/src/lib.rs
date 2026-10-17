//! Queues: the driver trait, the job a driver stores, and the in-memory driver.
//!
//! # What this is
//!
//! The storage half of a job queue. An application **pushes** a job — a name, an opaque payload, and
//! headers — and a worker **reserves** one, runs it, and then acks or nacks it. Everything between those
//! two moments is this crate's business; what a job *means* is the application's, which is why the
//! payload is opaque bytes here exactly as a KV value is.
//!
//! # The reserved/ack split, and why the trait is shaped around it
//!
//! A queue that hands a job out and forgets it loses the job when the worker dies. So a reservation is
//! **not** a removal: the driver remembers that job as held, by whom, and until when, and a reservation
//! whose lease expires comes back. That is what makes a crashed consumer a recoverable event rather than
//! a lost message, and it is why [`Driver::reserve`] returns a [`Reserved`] that must be
//! [`ack`](Driver::ack)ed or [`nack`](Driver::nack)ed.
//!
//! The lease needs a sweeper in every driver, and each driver's own documentation says how it does it.
//! What the trait cannot leave to a driver is the *rule*: **an expired reservation is available again**,
//! which is checked on the way into a reserve rather than by a timer alone — a queue nobody polls is a
//! queue whose expired reservations nobody has to notice.
//!
//! # At-least-once, and stated where it cannot be missed
//!
//! This is an **at-least-once** delivery model. A driver that loses a reservation record — a process that
//! died, a `DEL` that landed but whose reply did not — will hand the job out again, and an application
//! that is not idempotent will do its work twice. There is no way to make a queue exactly-once without
//! the consumer's cooperation, and the honest thing is to say so once, loudly, rather than to imply a
//! guarantee by not mentioning it.
//!
//! # What a `nack` means, and what it deliberately does not
//!
//! [`Driver::nack`] puts a job back, and that is all it does: **no retry counter, no backoff, no
//! dead-lettering.** RoadRunner leaves retry policy, backoff and dead-lettering to the application, and
//! this copies that deliberately — a server-side retry policy would make two applications' semantics
//! fight over one queue, and neither could win. An application that wants a dead-letter queue gets one by
//! pushing to a different pipeline in its own failure handler. What a driver must do is make the *number*
//! of attempts observable, which is why [`Job::attempts`] exists and is incremented by the driver rather
//! than sent by the producer.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

pub mod memory;

pub use memory::MemoryDriver;

/// A job, as a driver stores it.
///
/// # Why the payload is `Vec<u8>` and not a generic
///
/// The same reason `volucer_kv`'s value is: the queue is shared by applications, the encoding belongs to
/// whichever one pushed the job, and a server that parsed the payload could not be replaced without
/// agreeing with every producer about the format. RoadRunner's own documentation says the payload must be
/// a string and that serialization is the application's job; this makes that structural rather than
/// advisory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// An identifier unique within its pipeline, assigned by the driver on push.
    ///
    /// Assigned by the *driver* and not the producer, because it has to be unique across processes that
    /// never talk to each other, and because a producer-supplied id would be one more thing for two
    /// applications to collide over.
    pub id: String,
    /// The application's task name, e.g. `App\Jobs\SendEmail`.
    ///
    /// Opaque to the server, and named `task` rather than `name` because `name` is already the call
    /// method's own field — see `docs/plugins.md` §4.4. A worker that switches on it is doing what
    /// RoadRunner's `$task->getName()` does.
    pub task: String,
    /// The application's payload. Opaque, exactly as a KV value is.
    pub payload: Vec<u8>,
    /// Application metadata. **String values only**, which is what PSR-7-shaped headers mean and what
    /// every driver can store.
    pub headers: BTreeMap<String, String>,
    /// How many times this job has been reserved, counting the one in progress.
    ///
    /// **Zero until a driver hands it to a consumer**, and incremented by the **driver** on every
    /// reservation. Zero rather than one, because storing one and incrementing on the way out makes every
    /// job's first reservation report as its second — a number an application's retry policy reads.
    ///
    /// A producer cannot set it, because a producer that could would be one that could claim a job had
    /// been tried when it had not. This is the number an application needs to decide "this has failed three
    /// times, give up" — a policy the server deliberately does not impose (see the module documentation).
    pub attempts: u32,
    /// When the job was pushed, as the server's clock saw it.
    ///
    /// Wall-clock rather than an `Instant`, because it is worth reporting to an operator — "this job has
    /// been in the queue since Tuesday" is the first question about a stuck queue — and an `Instant` has
    /// no meaning outside the process that made it.
    pub pushed_at: SystemTime,
}

impl Job {
    /// The pipeline this job was pushed to. Set by the driver on the way out.
    ///
    /// Carried out-of-band rather than in `headers`, because headers are the *application's* and a
    /// driver that wrote a reserved key into them would be one rename away from corrupting an
    /// application that used the same one.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

/// A job that has been taken out of the queue and is now somebody's responsibility.
///
/// Dropping one without acking or nacking is what the lease exists to survive: the worker died, the
/// process was killed, a panic unwound — and the driver hands the job out again when the lease runs out.
/// There is deliberately no `Drop` impl that nacks, because a nack on a panic would put a job that
/// crashes its consumer straight back, which is a hot loop rather than a recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reserved {
    /// The job itself.
    pub job: Job,
    /// **Who holds it**, in a form the driver can check a release against.
    ///
    /// Not a secret and not a capability: it is the identifier a driver uses to decide whether the
    /// reservation being acked is the one it handed out, so that a worker whose lease expired and was
    /// re-handed to somebody else cannot ack the new holder's job out from under it.
    pub reservation: String,
    /// When the reservation stops being valid, on the server's clock.
    pub expires_at: SystemTime,
}

impl Reserved {
    /// How long is left of this reservation, or zero if it has passed.
    pub fn remaining(&self) -> Duration {
        self.expires_at
            .duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO)
    }
}

/// What can go wrong.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No pipeline by that name is configured.
    ///
    /// A separate variant rather than a generic failure because it is a *configuration* mistake and
    /// every other arm is a runtime one: an operator who mistyped a pipeline name needs to be told that,
    /// not told that the queue is unavailable.
    #[error("there is no pipeline named {name:?}; this deployment has {known}")]
    NoSuchPipeline { name: String, known: String },

    /// The pipeline's own name is not one a driver will accept.
    #[error("the pipeline name {reason}")]
    BadPipeline { reason: String },

    /// A job was pushed or reserved with something a driver will not store.
    #[error("the job {reason}")]
    BadJob { reason: String },

    /// The reservation is not the one the driver handed out — it expired and was re-issued, or it was
    /// already acked or nacked.
    ///
    /// **The outcome is known and it is not "the job completed"**: whatever the caller was doing, the
    /// queue no longer considers it this caller's job, and a second worker may be running it right now.
    /// A caller that treated this as success would report work as done that may be being done twice.
    #[error("the reservation {reservation:?} for job {job:?} is no longer valid: {reason}")]
    Lost {
        job: String,
        reservation: String,
        reason: String,
    },

    /// The backing store failed. Never produced by the memory driver.
    #[error("the {driver} queue failed: {reason}")]
    Backend {
        driver: &'static str,
        reason: String,
    },
}

/// The result of a queue operation.
pub type Result<T> = std::result::Result<T, Error>;
/// What a queue driver can do.
///
/// # Why `reserve` takes a lease
///
/// Because the alternative — the driver choosing one — is a number the driver cannot know: a deployment
/// whose jobs take ninety seconds needs a lease longer than ninety seconds, and one whose jobs take
/// milliseconds wants a short lease so a crashed worker's job comes back quickly. RoadRunner's equivalent
/// is the pipeline's `timeout` plus the broker's own visibility timeout; here it is one argument.
///
/// # Why the acks are by reservation and not by job id
///
/// A job id is stable across reservations and a reservation id is not, so acking by job id would let a
/// worker whose lease expired — and whose job a second worker is now running — mark it done. The
/// reservation id is the check that makes that impossible, and [`Error::Lost`] is what it reports.
pub trait Driver: Send + Sync + std::fmt::Debug {
    /// The driver's name, as it is written in configuration.
    fn name(&self) -> &'static str;

    /// Add a job to a pipeline, answering the id the driver assigned.
    ///
    /// **Nothing here is fallible for a reason**, and the reason worth stating: a push is either
    /// accepted or it is an error, and there is no third outcome. A driver that could say "maybe" would
    /// be one whose caller has to reconcile, which is a different API.
    fn push(&self, pipeline: &str, job: Pushed) -> Result<String>;

    /// Take up to `max` jobs from a pipeline, reserving each for `lease`.
    ///
    /// Fewer than `max` is normal and is not an error: a queue that is empty, or has fewer ready jobs
    /// than were asked for. An empty answer is the common case and is why this returns a `Vec` rather
    /// than an `Option`.
    fn reserve(&self, pipeline: &str, max: usize, lease: Duration) -> Result<Vec<Reserved>>;

    /// Mark a job done, removing it.
    fn ack(&self, pipeline: &str, reservation: &str) -> Result<()>;

    /// Return a job to the queue, to be reserved again.
    ///
    /// **The attempt count is kept**, and that is the point of it being a `nack` rather than a push:
    /// a job that fails repeatedly must be visible as one job that failed repeatedly, not as a stream of
    /// fresh jobs. Backoff is the caller's — see the module documentation.
    fn nack(&self, pipeline: &str, reservation: &str) -> Result<()>;

    /// How many jobs are ready to be reserved, ignoring those held by a live lease.
    ///
    /// A count rather than a boolean, because the number is what an operator watches — a queue that
    /// grows is the first symptom of a consumer that cannot keep up — and because a driver that can
    /// answer this cheaply should not have to be asked twice to say "yes".
    fn size(&self, pipeline: &str) -> Result<usize>;

    /// How many jobs are held by a live reservation.
    fn in_flight(&self, pipeline: &str) -> Result<usize>;

    /// Every configured pipeline's name, sorted.
    ///
    /// For `config.describe`, for `jobs.list`, and for the refusal an unknown pipeline name produces —
    /// which is why it is a trait method rather than something the caller assembles from configuration:
    /// the answer and the check that produced it have to be the same list.
    fn pipelines(&self) -> Vec<&str>;
}

/// A job a caller is pushing, before the driver has named it.
///
/// Separate from [`Job`] because the id and the attempt count are the driver's: a producer that supplied
/// an id would be one more thing for two applications to collide over, and one that supplied an attempt
/// count could claim a job had been tried when it had not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pushed {
    /// The application's task name. Required and non-empty.
    pub task: String,
    /// The application's payload. Opaque, and an empty payload is a payload.
    pub payload: Vec<u8>,
    /// Application metadata, string values only.
    pub headers: BTreeMap<String, String>,
}

impl Pushed {
    /// A job with a task name and no payload.
    pub fn new(task: impl Into<String>) -> Self {
        Pushed {
            task: task.into(),
            ..Pushed::default()
        }
    }

    /// One header's value, if it is set.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    /// The same job with a payload.
    #[must_use]
    pub fn with_payload(mut self, payload: Vec<u8>) -> Self {
        self.payload = payload;
        self
    }

    /// The same job with one header.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    /// The task name and the pipeline, checked before a driver sees them.
    pub fn validate(&self, pipeline: &str) -> Result<()> {
        validate_pipeline(pipeline)?;
        if self.task.is_empty() {
            return Err(Error::BadJob {
                reason: "has no task name, so a consumer could not tell what it is".to_string(),
            });
        }
        if self.task.len() > MAX_TASK_BYTES {
            return Err(Error::BadJob {
                reason: format!("has a task name longer than {MAX_TASK_BYTES} bytes"),
            });
        }
        Ok(())
    }
}

/// How long a task name may be.
///
/// A bound rather than a policy: a task name is a class name in every application that has one, and a
/// name long enough to matter is a payload that was put in the wrong field — which would then be
/// duplicated into every log line and every metric label that names the task.
pub const MAX_TASK_BYTES: usize = 512;

/// Whether a pipeline name is one a driver will accept.
///
/// Checked once, here, so that two drivers cannot disagree about what a pipeline may be called — which
/// would make an application's configuration depend on a driver line, the same argument
/// `volucer_lock::validate` makes for resource names.
pub fn validate_pipeline(pipeline: &str) -> Result<()> {
    if pipeline.is_empty() {
        return Err(Error::BadPipeline {
            reason: "is empty, so no caller could name it".to_string(),
        });
    }
    if pipeline.len() > MAX_PIPELINE_BYTES {
        return Err(Error::BadPipeline {
            reason: format!(
                "is longer than {MAX_PIPELINE_BYTES} bytes, which is not a pipeline name"
            ),
        });
    }
    if pipeline.chars().any(char::is_control) {
        return Err(Error::BadPipeline {
            reason: "contains a control character, which would be invisible in a key listing"
                .to_string(),
        });
    }
    Ok(())
}

/// How long a pipeline name may be.
pub const MAX_PIPELINE_BYTES: usize = 256;

/// A driver's pipelines, by name, with the check every method starts from.
///
/// Every implementation of [`Driver`] begins each method by finding its pipeline, and each would write
/// the same refusal. This is that refusal, once — and it is generic over what the pipeline *is*, because
/// a memory driver's pipeline is a `Vec` and a Redis driver's is a key prefix plus a client.
pub(crate) fn pipeline<'a, T>(pipelines: &'a BTreeMap<String, T>, name: &str) -> Result<&'a T> {
    validate_pipeline(name)?;
    pipelines.get(name).ok_or_else(|| Error::NoSuchPipeline {
        name: name.to_string(),
        known: if pipelines.is_empty() {
            "no pipeline configured at all".to_string()
        } else {
            format!(
                "these pipelines: {}",
                pipelines
                    .keys()
                    .map(|name| format!("{name:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pipelines(names: &[&str]) -> BTreeMap<String, usize> {
        names
            .iter()
            .enumerate()
            .map(|(index, name)| ((*name).to_string(), index))
            .collect()
    }

    #[test]
    fn a_pipeline_that_is_not_configured_is_refused_with_the_ones_that_are() {
        let known = pipelines(&["emails", "reports"]);

        let err = pipeline(&known, "emials").expect_err("a typo is not a pipeline");
        let message = err.to_string();
        assert!(
            message.contains("emials"),
            "it names what was asked for: {message}"
        );
        assert!(
            message.contains("\"emails\""),
            "and what exists, so the fix is a rename rather than a search: {message}"
        );

        // The empty case says something different on purpose: there is nothing to rename *to*, so the
        // answer is that no pipeline is configured rather than a list of none.
        let err = pipeline(&pipelines(&[]), "emails").expect_err("nothing is configured");
        assert!(
            err.to_string().contains("no pipeline configured at all"),
            "{err}"
        );
    }

    #[test]
    fn a_pipeline_name_is_checked_for_shape_before_it_is_looked_up() {
        // The order matters: a control character is refused as a *bad name* rather than reported as an
        // unknown pipeline, because those are different mistakes with different fixes.
        let err = pipeline(&pipelines(&["emails"]), "em\u{1}ails")
            .expect_err("a control character is never a pipeline name");
        assert!(matches!(err, Error::BadPipeline { .. }), "{err}");

        assert!(matches!(
            pipeline(&pipelines(&["emails"]), ""),
            Err(Error::BadPipeline { .. })
        ));

        let long = "x".repeat(MAX_PIPELINE_BYTES + 1);
        assert!(matches!(
            pipeline(&pipelines(&["emails"]), &long),
            Err(Error::BadPipeline { .. })
        ));
    }

    #[test]
    fn a_pushed_job_without_a_task_name_is_refused() {
        assert!(matches!(
            Pushed::default().validate("emails"),
            Err(Error::BadJob { .. })
        ));

        let long = Pushed::new("x".repeat(MAX_TASK_BYTES + 1));
        assert!(matches!(long.validate("emails"), Err(Error::BadJob { .. })));

        // And an empty *payload* is a payload, not a mistake: a job with no arguments is ordinary.
        Pushed::new("App\\Jobs\\Ping")
            .with_payload(Vec::new())
            .validate("emails")
            .expect("a job with an empty payload is a job");
    }

    #[test]
    fn a_reservations_remaining_time_is_zero_once_it_has_passed() {
        // Saturing rather than panicking, and `ZERO` rather than a negative duration, because the caller
        // comparing against zero is asking "may I still ack this" and the answer is no.
        let past = Reserved {
            job: Job {
                id: "1".to_string(),
                task: "t".to_string(),
                payload: Vec::new(),
                headers: BTreeMap::new(),
                attempts: 1,
                pushed_at: SystemTime::now(),
            },
            reservation: "r".to_string(),
            expires_at: SystemTime::now() - Duration::from_secs(1),
        };
        assert_eq!(past.remaining(), Duration::ZERO);
    }

    #[test]
    fn a_pushed_job_is_built_with_the_shape_a_caller_writes() {
        let job = Pushed::new("App\\Jobs\\SendEmail")
            .with_payload(b"{\"to\":\"a@b\"}".to_vec())
            .with_header("traceparent", "00-abc-def-01");

        assert_eq!(job.task, "App\\Jobs\\SendEmail");
        assert_eq!(job.payload, b"{\"to\":\"a@b\"}".to_vec());
        assert_eq!(job.header("traceparent"), Some("00-abc-def-01"));
        assert_eq!(job.header("absent"), None);
        job.validate("emails").expect("which is a valid job");
    }
}
