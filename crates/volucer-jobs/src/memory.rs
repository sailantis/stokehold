//! The in-memory driver: one process's queues, and nothing else.
//!
//! # What it is for
//!
//! A single server's workers are most of the queue traffic there is — an image to resize, an email to
//! send, a report to render — and this driver handles all of it with no broker installed. A second
//! server, or a job that has to survive a restart, needs a driver with a server behind it.
//!
//! # Its one honesty, stated where it is configured rather than discovered
//!
//! **A restart loses everything.** There is no disk and no broker, so an accepted push is gone when the
//! process ends — including jobs that were reserved and not yet acked. That is what an in-memory queue
//! means, and RoadRunner's own documentation says the same about its `memory` driver (in its own words:
//! "The memory driver loses jobs when RoadRunner stops. Auto-ack can acknowledge a job before PHP
//! processes it."). It is a queue for work that is regenerated or that an application can afford to lose,
//! and the alternative for anything else is a driver with a server behind it.
//!
//! # Ordering, and the one promise
//!
//! **FIFO within a pipeline**, and *not* priority order. RoadRunner has one process-wide priority queue
//! shared by every pipeline, with a configurable `pipeline_size`; this does not, and the reason is a
//! difference in what a queue per pipeline can promise rather than a shortcut. A priority queue reorders
//! a pipeline's jobs against each other, and the promise an application actually relies on is that two
//! jobs pushed in one order are handled in that order. Priority is a *pipeline's* business here: a
//! deployment that wants "urgent mail before bulk mail" configures two pipelines and two consumers, which
//! is more visible than a numeric field and needs no shared queue to starve.
//!
//! # Expiry is arithmetic on the way into a reserve, and that is a correction
//!
//! The first version swept expired reservations in a rate-limited background-style pass — the shape the KV
//! and lock drivers use — and it was **wrong here**, in a way a test found. A rate-limited sweep makes the
//! *availability* of a job depend on when the last sweep ran, so a reservation that expired one
//! millisecond after a sweep is not reclaimable for another second. For a cache that is invisible; for a
//! queue it is a consumer idling while a ready job sits there.
//!
//! So a reserve **scans the in-flight map** first, returns every job whose lease has passed to the front
//! of the queue, and then hands out work. The scan is cheap for the reason it is correct: the map holds one
//! record per *live* reservation, which is bounded by how many consumers this server has, and a record is
//! removed by the same scan that reclaims it. There is no timer, no interval, and no state in which a job
//! is ready but unreachable.
//!
//! This is also why there is no `pipeline_size` and no shared priority queue: a `VecDeque` per pipeline
//! has no bound to configure and cannot reorder a pipeline's work against itself.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use crate::{Driver, Error, Job, Pushed, Reserved, Result};

/// A queue in this process's memory.
#[derive(Debug)]
pub struct MemoryDriver {
    pipelines: BTreeMap<String, Mutex<Queue>>,
    ids: AtomicU64,
}

#[derive(Debug, Default)]
struct Queue {
    /// Ready jobs, oldest first. A `VecDeque` because both ends are used: a `push_back` and a
    /// `pop_front`.
    ready: VecDeque<Job>,
    /// Reservations in progress, by reservation id. `HashMap` rather than `BTreeMap` because nothing
    /// iterates it in an order anybody reads.
    in_flight: HashMap<String, Held>,
}

#[derive(Debug)]
struct Held {
    job: Job,
    deadline: Instant,
}

impl Default for MemoryDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryDriver {
    /// A driver over one pipeline called `default`, which is what a configuration that names none gets.
    pub fn new() -> Self {
        Self::single("default")
    }

    /// A driver over one pipeline name.
    pub fn single(pipeline: impl Into<String>) -> Self {
        Self::with_pipelines([pipeline])
    }

    /// A driver over several pipeline names.
    ///
    /// Takes names rather than a configuration, because what a pipeline *is* to this driver is a queue and
    /// nothing else — the configuration a deployment writes is about which pipeline uses which driver, and
    /// that decision has already been made by the time a driver exists.
    pub fn with_pipelines<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        MemoryDriver {
            pipelines: names
                .into_iter()
                .map(|name| (name.into(), Mutex::new(Queue::default())))
                .collect(),
            ids: AtomicU64::new(0),
        }
    }

    /// A poisoned lock is one a previous call panicked while holding.
    ///
    /// Recovering is right for the same reason the KV and lock drivers do it: every mutation here is a
    /// single operation that either ran or did not, and refusing every later push for the rest of the
    /// process's life would turn one panicked consumer into a server that cannot accept work.
    fn queue<'a>(&'a self, name: &str) -> Result<std::sync::MutexGuard<'a, Queue>> {
        let queue = crate::pipeline(&self.pipelines, name)?;
        Ok(queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()))
    }

    /// The next job id.
    ///
    /// A counter and the process id, because an id has to be unique across the *queues* an operator looks
    /// at together and a bare counter would repeat in every process. Not a UUID: the ids are read in logs
    /// and typed into a diagnostic, and thirty-two characters of hex is worse at that than a short number
    /// that happens to be unique here.
    fn next_id(&self) -> String {
        let sequence = self.ids.fetch_add(1, Ordering::Relaxed);
        format!("{:x}-{:x}", std::process::id(), sequence)
    }

    /// Return every job whose reservation has expired to the front of the ready queue.
    ///
    /// Called on the way into a reserve, and it is what makes expiry a property of the *queue* rather than
    /// of a timer — see the module documentation.
    ///
    /// The records are removed as they are reclaimed, which is what bounds the map: it holds one entry per
    /// live reservation, so this scan is over the number of consumers this server has and nothing else.
    fn reclaim_expired(queue: &mut Queue, now: Instant) -> usize {
        let mut expired: Vec<(Instant, Job)> = Vec::new();
        queue.in_flight.retain(|_, held| {
            if held.deadline <= now {
                expired.push((held.deadline, held.job.clone()));
                false
            } else {
                true
            }
        });

        if expired.is_empty() {
            return 0;
        }
        // Oldest expiry first, then pushed to the *front* in reverse: a job whose consumer died long ago
        // is handled before one that died just now, and both before work that arrived in the meantime.
        expired.sort_by_key(|(deadline, _)| *deadline);
        let count = expired.len();
        for (_, job) in expired.into_iter().rev() {
            queue.ready.push_front(job);
        }
        tracing::debug!(reclaimed = count, "returned expired job reservations");
        count
    }

    /// Find a live reservation, or say why there is none.
    ///
    /// Takes `&Queue` rather than `&mut`, because a lookup is a lookup: the version that required a mutable
    /// borrow had to be called before the removal it guards, which is a shape that invites removing first.
    fn held<'a>(queue: &'a Queue, reservation: &str) -> Result<&'a Held> {
        let now = Instant::now();
        match queue.in_flight.get(reservation) {
            Some(held) if held.deadline > now => Ok(held),
            Some(held) => Err(Error::Lost {
                job: held.job.id.clone(),
                reservation: reservation.to_string(),
                reason: "its lease expired, so the job has been or will be handed to somebody else"
                    .to_string(),
            }),
            None => Err(Error::Lost {
                job: "?".to_string(),
                reservation: reservation.to_string(),
                reason: "it was already acked or nacked, or it belongs to another process"
                    .to_string(),
            }),
        }
    }
}

impl Driver for MemoryDriver {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn push(&self, pipeline: &str, job: Pushed) -> Result<String> {
        job.validate(pipeline)?;

        let id = self.next_id();
        let stored = Job {
            id: id.clone(),
            task: job.task,
            payload: job.payload,
            headers: job.headers,
            // **Zero, and incremented on the way out of a reserve.** The field counts reservations, so a
            // job that has never been handed to a worker has zero attempts — storing one here and
            // incrementing again on the way out would make every job's first reservation report as its
            // second, which is a number an application's retry policy reads.
            attempts: 0,
            pushed_at: SystemTime::now(),
        };

        let mut queue = self.queue(pipeline)?;
        queue.ready.push_back(stored);
        Ok(id)
    }

    fn reserve(&self, pipeline: &str, max: usize, lease: Duration) -> Result<Vec<Reserved>> {
        let mut queue = self.queue(pipeline)?;
        let now = Instant::now();
        // Before anything is handed out, so a reclaimed job is available on this very call rather than on
        // whatever the next one happens to be. This is the correction the module documentation describes.
        Self::reclaim_expired(&mut queue, now);

        let deadline = now + lease;
        let mut out = Vec::with_capacity(max.min(queue.ready.len()));
        for _ in 0..max {
            let Some(mut job) = queue.ready.pop_front() else {
                break;
            };
            // The attempt count is incremented **here, and only here** — on the way out. A count
            // incremented on a nack as well would miss exactly the crashes that matter most: a worker
            // killed mid-job never runs its nack.
            job.attempts = job.attempts.saturating_add(1);
            let reservation = self.next_id();
            queue.in_flight.insert(
                reservation.clone(),
                Held {
                    job: job.clone(),
                    deadline,
                },
            );
            out.push(Reserved {
                job,
                reservation,
                expires_at: SystemTime::now() + lease,
            });
        }
        Ok(out)
    }

    fn ack(&self, pipeline: &str, reservation: &str) -> Result<()> {
        let mut queue = self.queue(pipeline)?;
        // The lookup is what makes a late ack a `Lost` rather than a silent success, and it happens
        // *before* the removal so that an expired or foreign reservation is never removed.
        Self::held(&queue, reservation)?;
        queue.in_flight.remove(reservation);
        Ok(())
    }

    fn nack(&self, pipeline: &str, reservation: &str) -> Result<()> {
        let mut queue = self.queue(pipeline)?;
        let held = Self::held(&queue, reservation)?;
        let job = held.job.clone();
        queue.in_flight.remove(reservation);
        // Back to the **back** of the queue, not the front: a job that just failed should not jump ahead
        // of work that has been waiting, and a job that fails deterministically would otherwise starve
        // everything behind it — a hot loop rather than a retry.
        queue.ready.push_back(job);
        Ok(())
    }

    fn size(&self, pipeline: &str) -> Result<usize> {
        let mut queue = self.queue(pipeline)?;
        // Reclaimed first, so the number includes jobs that are ready *now* — reporting the queue as
        // smaller than it is would corrupt the one figure an operator scales on. Computed rather than
        // read off a counter, which is only possible because reclaiming is arithmetic.
        Self::reclaim_expired(&mut queue, Instant::now());
        Ok(queue.ready.len())
    }

    fn in_flight(&self, pipeline: &str) -> Result<usize> {
        let queue = self.queue(pipeline)?;
        let now = Instant::now();
        // Expired reservations are not in flight — they are ready again, whatever the records still say.
        Ok(queue
            .in_flight
            .values()
            .filter(|held| held.deadline > now)
            .count())
    }

    fn pipelines(&self) -> Vec<&str> {
        self.pipelines.keys().map(String::as_str).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn driver() -> MemoryDriver {
        MemoryDriver::single("emails")
    }

    /// A lease long enough that a slow machine cannot expire it mid-test.
    fn long() -> Duration {
        Duration::from_secs(60)
    }

    #[test]
    fn a_pushed_job_comes_back_with_everything_that_went_in() {
        let driver = driver();
        let pushed = Pushed::new("App\\Jobs\\SendEmail")
            .with_payload(b"{\"to\":\"a@b\"}".to_vec())
            .with_header("traceparent", "00-abc-def-01");

        let id = driver.push("emails", pushed.clone()).expect("pushed");
        assert!(!id.is_empty());

        let reserved = driver.reserve("emails", 1, long()).expect("reserved");
        assert_eq!(reserved.len(), 1);
        let job = &reserved[0].job;
        assert_eq!(
            job.id, id,
            "the id a push answers with is the id a reserve reports"
        );
        assert_eq!(job.task, "App\\Jobs\\SendEmail");
        assert_eq!(
            job.payload,
            b"{\"to\":\"a@b\"}".to_vec(),
            "the payload is opaque bytes"
        );
        assert_eq!(job.header("traceparent"), Some("00-abc-def-01"));
        assert_eq!(
            job.attempts, 1,
            "the first reservation is attempt one, and the *driver* decides that rather than the producer"
        );
    }

    #[test]
    fn jobs_are_handed_out_in_the_order_they_were_pushed() {
        let driver = driver();
        for index in 0..5 {
            driver
                .push("emails", Pushed::new(format!("job-{index}")))
                .expect("pushed");
        }

        let reserved = driver.reserve("emails", 5, long()).expect("reserved");
        let tasks: Vec<&str> = reserved.iter().map(|held| held.job.task.as_str()).collect();
        assert_eq!(
            tasks,
            vec!["job-0", "job-1", "job-2", "job-3", "job-4"],
            "FIFO within a pipeline is the promise an application relies on"
        );
    }

    #[test]
    fn a_reserved_job_is_not_handed_out_again_while_its_lease_holds() {
        let driver = driver();
        driver.push("emails", Pushed::new("only")).expect("pushed");

        let first = driver.reserve("emails", 1, long()).expect("reserved");
        assert_eq!(first.len(), 1);

        assert!(
            driver
                .reserve("emails", 1, long())
                .expect("reserved")
                .is_empty(),
            "a job in flight must not be handed to a second consumer"
        );
        assert_eq!(driver.size("emails").expect("size"), 0);
        assert_eq!(driver.in_flight("emails").expect("in flight"), 1);

        driver.ack("emails", &first[0].reservation).expect("acked");
        assert_eq!(driver.in_flight("emails").expect("in flight"), 0);
        assert!(
            driver
                .reserve("emails", 1, long())
                .expect("reserved")
                .is_empty(),
            "and after an ack it is gone rather than available again"
        );
    }

    #[test]
    fn a_reservation_whose_lease_expires_comes_back_on_the_next_reserve() {
        let driver = driver();
        driver.push("emails", Pushed::new("slow")).expect("pushed");

        let first = driver
            .reserve("emails", 1, Duration::from_millis(1))
            .expect("reserved");
        assert_eq!(first.len(), 1);
        std::thread::sleep(Duration::from_millis(20));

        let second = driver
            .reserve("emails", 1, long())
            .expect("reserved")
            .pop()
            .expect("the expired reservation must have put the job back");
        assert_eq!(second.job.task, "slow");
        assert_eq!(
            second.job.attempts, 2,
            "and the second attempt is visible, which is the number an application's retry policy needs"
        );
    }

    #[test]
    fn an_expired_job_is_ready_for_the_very_next_reserve_and_for_the_size() {
        // The bug the first version of this driver had, kept as a test. A rate-limited sweep made
        // availability depend on when the last sweep ran, so a consumer could idle with a ready job
        // sitting there and `size` could report zero while a job was in fact available. Reclaiming is now
        // arithmetic on the way into a reserve, so both are true on the very next call.
        let driver = driver();
        driver.push("emails", Pushed::new("slow")).expect("pushed");

        driver
            .reserve("emails", 1, Duration::from_millis(1))
            .expect("reserved");
        std::thread::sleep(Duration::from_millis(20));

        assert_eq!(
            driver.in_flight("emails").expect("in flight"),
            0,
            "an expired reservation is not in flight"
        );
        assert_eq!(
            driver.size("emails").expect("size"),
            1,
            "and the job it held is ready again, which is the number an operator's scaling decision \
             needs to be right ? reported before any reserve has run"
        );
        let second = driver
            .reserve("emails", 1, long())
            .expect("reserved")
            .pop()
            .expect("a ready job must be reserveable");
        assert_eq!(second.job.task, "slow");
        assert_eq!(second.job.attempts, 2);
    }

    #[test]
    fn a_reclaimed_job_is_handed_out_before_new_work() {
        // The ordering the reclaimed list exists for: a job whose consumer died is the oldest work in the
        // pipeline, and a reserve that took the ready queue first would starve it behind everything that
        // arrived while it was held.
        let driver = driver();
        driver.push("emails", Pushed::new("held")).expect("pushed");
        let first = driver
            .reserve("emails", 1, Duration::from_millis(1))
            .expect("reserved");
        assert_eq!(first[0].job.task, "held");

        driver.push("emails", Pushed::new("new")).expect("pushed");
        std::thread::sleep(Duration::from_millis(20));

        let next = driver.reserve("emails", 2, long()).expect("reserved");
        assert_eq!(
            next.iter()
                .map(|held| held.job.task.as_str())
                .collect::<Vec<_>>(),
            vec!["held", "new"],
            "the reclaimed job goes first"
        );
        assert_eq!(
            next[0].job.attempts, 2,
            "and a reclaimed job's attempt count is incremented like any other reservation"
        );
    }

    #[test]
    fn a_nack_returns_the_job_to_the_back_of_the_queue() {
        let driver = driver();
        for task in ["first", "second"] {
            driver.push("emails", Pushed::new(task)).expect("pushed");
        }

        let reserved = driver.reserve("emails", 1, long()).expect("reserved");
        assert_eq!(reserved[0].job.task, "first");
        driver
            .nack("emails", &reserved[0].reservation)
            .expect("nacked");

        // The failed job goes to the back rather than the front: a job that fails deterministically must
        // not starve everything behind it.
        let next = driver.reserve("emails", 2, long()).expect("reserved");
        assert_eq!(
            next.iter()
                .map(|held| held.job.task.as_str())
                .collect::<Vec<_>>(),
            vec!["second", "first"],
            "the requeued job waits behind work that was already there"
        );
        assert_eq!(
            next[1].job.attempts, 2,
            "and it carries its attempt count with it, because a requeue is the same job rather than a \
             new one — so its second reservation is attempt two"
        );
    }

    #[test]
    fn acking_or_nacking_a_reservation_twice_is_a_lost_reservation_rather_than_a_silent_success() {
        let driver = driver();
        driver.push("emails", Pushed::new("once")).expect("pushed");
        let reserved = driver.reserve("emails", 1, long()).expect("reserved");
        let reservation = reserved[0].reservation.clone();

        driver
            .ack("emails", &reservation)
            .expect("the first ack is the ack");
        let err = driver
            .ack("emails", &reservation)
            .expect_err("a second ack is not a success");
        assert!(matches!(err, Error::Lost { .. }), "{err}");
        assert!(
            err.to_string().contains("already acked"),
            "and it says which of the two cases it is: {err}"
        );

        // The same for a reservation nobody ever handed out.
        let err = driver
            .nack("emails", "never-existed")
            .expect_err("a reservation this driver never issued");
        assert!(matches!(err, Error::Lost { .. }), "{err}");
    }

    #[test]
    fn acking_a_reservation_whose_lease_expired_is_lost_rather_than_completing_it() {
        // The case the reservation id exists for. A worker that was paused past its lease must not be
        // able to mark a job done that another worker may now be running — reporting success here is how
        // an application comes to believe work happened twice, or not at all.
        let driver = driver();
        driver.push("emails", Pushed::new("slow")).expect("pushed");
        let reserved = driver
            .reserve("emails", 1, Duration::from_millis(1))
            .expect("reserved");
        std::thread::sleep(Duration::from_millis(20));

        let err = driver
            .ack("emails", &reserved[0].reservation)
            .expect_err("a late ack must not mark the job done");
        assert!(matches!(err, Error::Lost { .. }), "{err}");
        assert!(err.to_string().contains("expired"), "{err}");

        // And the job is still there for whoever holds it now, which is what makes the refusal matter.
        assert_eq!(
            driver.in_flight("emails").expect("in flight"),
            0,
            "the expired reservation is not in flight"
        );
        assert_eq!(driver.size("emails").expect("size"), 1);
    }

    #[test]
    fn the_pipelines_are_independent_so_work_in_one_does_not_appear_in_another() {
        let driver = MemoryDriver::with_pipelines(["emails", "reports"]);
        driver.push("emails", Pushed::new("mail")).expect("pushed");
        driver
            .push("reports", Pushed::new("report"))
            .expect("pushed");

        let emails = driver.reserve("emails", 10, long()).expect("reserved");
        assert_eq!(emails.len(), 1);
        assert_eq!(emails[0].job.task, "mail");

        let reports = driver.reserve("reports", 10, long()).expect("reserved");
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].job.task, "report");

        assert_eq!(
            driver.pipelines(),
            vec!["emails", "reports"],
            "sorted and complete, because this is the list a refusal quotes and a describe reports"
        );
    }

    #[test]
    fn an_unknown_pipeline_is_refused_by_name_on_every_method() {
        let driver = driver();
        for outcome in [
            driver.push("emials", Pushed::new("t")).err(),
            driver.reserve("emials", 1, long()).err(),
            driver.ack("emials", "r").err(),
            driver.nack("emials", "r").err(),
            driver.size("emials").err(),
            driver.in_flight("emials").err(),
        ] {
            let err = outcome.unwrap_or_else(|| panic!("an unknown pipeline must be refused"));
            assert!(matches!(err, Error::NoSuchPipeline { .. }), "{err}");
            assert!(err.to_string().contains("emials"), "{err}");
        }
    }

    #[test]
    fn a_reserve_asking_for_more_than_there_is_answers_with_what_there_is() {
        let driver = driver();
        driver.push("emails", Pushed::new("only")).expect("pushed");

        // Fewer than asked for is normal rather than an error, and an empty queue answers with nothing.
        assert_eq!(
            driver
                .reserve("emails", 10, long())
                .expect("reserved")
                .len(),
            1
        );
        assert!(driver
            .reserve("emails", 10, long())
            .expect("reserved")
            .is_empty());
        assert!(
            driver
                .reserve("emails", 0, long())
                .expect("reserved")
                .is_empty(),
            "asking for none is asking for none"
        );
    }

    #[test]
    fn a_job_id_is_unique_across_a_drivers_lifetime() {
        let driver = driver();
        let mut ids = std::collections::BTreeSet::new();
        for index in 0..50 {
            let id = driver
                .push("emails", Pushed::new(format!("job-{index}")))
                .expect("pushed");
            assert!(ids.insert(id.clone()), "{id} was handed out twice");
        }
    }

    #[test]
    fn a_task_name_a_driver_will_not_store_is_refused_before_anything_is_queued() {
        let driver = driver();
        assert!(matches!(
            driver.push("emails", Pushed::default()),
            Err(Error::BadJob { .. })
        ));
        assert_eq!(
            driver.size("emails").expect("size"),
            0,
            "and a refused push must not have queued anything"
        );

        // The payload, by contrast, may be anything at all — including empty, and including bytes that
        // are not text. That is the whole reason it is a `Vec<u8>`.
        driver
            .push(
                "emails",
                Pushed::new("t").with_payload(vec![0xff, 0x00, 0xfe]),
            )
            .expect("a payload with bytes above 0x7f is a payload");
        let job = driver.reserve("emails", 1, long()).expect("reserved");
        assert_eq!(job[0].job.payload, vec![0xff, 0x00, 0xfe]);
    }

    #[test]
    fn a_reservation_reports_what_is_left_of_its_lease() {
        let driver = driver();
        driver.push("emails", Pushed::new("t")).expect("pushed");

        let reserved = driver
            .reserve("emails", 1, Duration::from_secs(60))
            .expect("reserved");
        assert_eq!(reserved.len(), 1);
        let remaining = reserved[0].remaining();
        assert!(
            remaining <= Duration::from_secs(60) && remaining > Duration::from_secs(59),
            "close to a minute and never above it: {remaining:?}"
        );
    }
}
