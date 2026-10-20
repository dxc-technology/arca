//! Group commit for the single SQLite writer.
//!
//! With `synchronous=FULL` every COMMIT is an fsync of the WAL, and the writer
//! runs one transaction at a time, so one write per transaction caps the
//! write rate at one per fsync whatever the client concurrency. The group
//! committer runs the writes that queued up while the previous commit was in
//! flight in one transaction, each under its own savepoint, and answers each
//! caller only after that transaction's COMMIT: one fsync for the whole group.
//!
//! - **No linger.** A group is whatever is already queued (at most
//!   [`MAX_GROUP`] writes): a lone write commits at once, as before.
//! - **Serial order.** The writes of a group run one after another in
//!   submission order, each seeing the ones before it, exactly as separate
//!   transactions on the one writer would.
//! - **Per-write failure.** A write that returns an error or panics has its
//!   savepoint rolled back and fails alone; the rest of the group commits.
//! - **Group failure.** When the group cannot commit (COMMIT fails, or SQLite
//!   aborted the whole transaction after a write's error), nothing of it is
//!   durable: every write that had succeeded is answered with the group's
//!   error, a write that failed with its own.
//!
//! Writes that must not share a transaction (migrations, VACUUM, bulk copies)
//! keep calling the writer connection directly; tokio-rusqlite runs them
//! between groups, so there is still exactly one writer.

use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};

use rusqlite::Savepoint;
use tokio::sync::{mpsc, oneshot};

/// Most writes committed in one transaction. Bounds how long the first write
/// of a group waits for the others' statements (tens of microseconds each)
/// and the size of the transaction.
pub(crate) const MAX_GROUP: usize = 64;

/// Failure of a grouped write.
#[derive(Debug)]
pub(crate) enum WriteError<E> {
    /// The write's own error: its savepoint was rolled back.
    Write(E),
    /// The write succeeded but its group did not commit, or the writer is gone.
    Group(String),
}

impl<E: fmt::Display> fmt::Display for WriteError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WriteError::Write(e) => e.fmt(f),
            WriteError::Group(msg) => f.write_str(msg),
        }
    }
}

/// A queued write, type-erased so one queue carries every result type.
trait Job: Send {
    /// Runs the write under `sp`; returns whether it succeeded.
    fn run(&mut self, sp: &mut Savepoint<'_>) -> bool;
    /// Answers the caller: `group` is the outcome of the group's COMMIT.
    fn finish(self: Box<Self>, group: Result<(), String>);
}

struct TypedJob<F, R, E> {
    write: Option<F>,
    outcome: Outcome<R, E>,
    reply: oneshot::Sender<Result<R, WriteError<E>>>,
}

enum Outcome<R, E> {
    /// Not run: its group failed before reaching it.
    Pending,
    Done(Result<R, E>),
    Panicked,
}

impl<F, R, E> Job for TypedJob<F, R, E>
where
    F: FnOnce(&mut Savepoint<'_>) -> Result<R, E> + Send,
    R: Send,
    E: Send,
{
    fn run(&mut self, sp: &mut Savepoint<'_>) -> bool {
        let write = self.write.take().expect("a job runs once");
        match catch_unwind(AssertUnwindSafe(|| write(sp))) {
            Ok(outcome) => {
                let ok = outcome.is_ok();
                self.outcome = Outcome::Done(outcome);
                ok
            }
            Err(_) => {
                self.outcome = Outcome::Panicked;
                false
            }
        }
    }

    fn finish(self: Box<Self>, group: Result<(), String>) {
        let answer = match (self.outcome, group) {
            (Outcome::Done(Err(e)), _) => Err(WriteError::Write(e)),
            (Outcome::Done(Ok(r)), Ok(())) => Ok(r),
            (Outcome::Panicked, _) => Err(WriteError::Group("the write panicked".to_string())),
            (_, Err(msg)) => Err(WriteError::Group(msg)),
            (Outcome::Pending, Ok(())) => unreachable!("a committed group ran all its writes"),
        };
        // The caller may have gone away (request cancelled): nothing to answer.
        let _ = self.reply.send(answer);
    }
}

/// Handle to the group committer task of one writer connection.
pub(crate) struct GroupCommitter {
    queue: mpsc::UnboundedSender<Box<dyn Job>>,
}

impl GroupCommitter {
    /// Starts the committer task for `writer`. The task ends when the
    /// committer is dropped.
    pub(crate) fn spawn(writer: tokio_rusqlite::Connection) -> Self {
        let (queue, jobs) = mpsc::unbounded_channel();
        tokio::spawn(run_groups(writer, jobs));
        Self { queue }
    }

    /// Runs `write` in the next group and returns its result once the group
    /// has committed. `write` must not begin or commit a transaction of its
    /// own: it runs inside the group's, under a savepoint it may nest.
    pub(crate) async fn write<F, R, E>(&self, write: F) -> Result<R, WriteError<E>>
    where
        F: FnOnce(&mut Savepoint<'_>) -> Result<R, E> + Send + 'static,
        R: Send + 'static,
        E: Send + 'static,
    {
        let (reply, answer) = oneshot::channel();
        let job = TypedJob { write: Some(write), outcome: Outcome::Pending, reply };
        self.queue
            .send(Box::new(job))
            .map_err(|_| WriteError::Group("the group committer has stopped".to_string()))?;
        answer.await.unwrap_or_else(|_| {
            Err(WriteError::Group("the write group was dropped: writer connection closed".to_string()))
        })
    }
}

async fn run_groups(
    writer: tokio_rusqlite::Connection,
    mut queue: mpsc::UnboundedReceiver<Box<dyn Job>>,
) {
    while let Some(first) = queue.recv().await {
        let mut jobs = vec![first];
        while jobs.len() < MAX_GROUP {
            match queue.try_recv() {
                Ok(job) => jobs.push(job),
                Err(_) => break,
            }
        }
        // On a closed connection the jobs are dropped unanswered and their
        // callers get the "writer connection closed" error.
        let _ = writer
            .call_raw(move |conn| {
                let group = commit_group(conn, &mut jobs);
                if let Err(ref msg) = group {
                    tracing::error!(writes = jobs.len(), error = %msg, "SQLite write group failed");
                }
                for job in jobs {
                    job.finish(group.clone());
                }
            })
            .await;
    }
}

/// Runs `jobs` in one transaction, each under its own savepoint, and commits.
fn commit_group(conn: &mut rusqlite::Connection, jobs: &mut [Box<dyn Job>]) -> Result<(), String> {
    let mut tx = conn.transaction().map_err(|e| format!("BEGIN: {e}"))?;
    for job in jobs.iter_mut() {
        let mut sp = tx.savepoint().map_err(|e| format!("SAVEPOINT: {e}"))?;
        let ok = job.run(&mut sp);
        if sp.is_autocommit() {
            // SQLite rolled back the whole transaction (an I/O error, a full
            // disk, or a write that ended it): what ran before is gone.
            return Err("the transaction was rolled back by a failed write".to_string());
        }
        // Explicit, not the savepoint's drop: a RELEASE that fails on drop
        // falls back to a rollback, which would lose a write reported as done.
        if ok {
            sp.commit().map_err(|e| format!("RELEASE: {e}"))?;
        } else {
            sp.rollback().map_err(|e| format!("ROLLBACK TO: {e}"))?;
            sp.commit().map_err(|e| format!("RELEASE: {e}"))?;
        }
    }
    tx.commit().map_err(|e| format!("COMMIT: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::test_support::{commits, count_commits, hold_writer};
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    /// A file-backed writer (WAL, IMMEDIATE, as the store opens it) with a
    /// `t (v UNIQUE)` table, and a counter of its commits.
    async fn writer(dir: &tempfile::TempDir) -> (tokio_rusqlite::Connection, Arc<AtomicUsize>) {
        let conn = tokio_rusqlite::Connection::open(dir.path().join("t.db")).await.unwrap();
        conn.call(|c| {
            c.set_transaction_behavior(rusqlite::TransactionBehavior::Immediate);
            c.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t (v INTEGER UNIQUE);")
        })
        .await
        .unwrap();
        let count = count_commits(&conn).await;
        (conn, count)
    }

    async fn rows(conn: &tokio_rusqlite::Connection) -> Vec<i64> {
        conn.call(|c| {
            let mut stmt = c.prepare("SELECT v FROM t ORDER BY v")?;
            let rows = stmt.query_map([], |r| r.get(0))?.collect::<Result<Vec<i64>, _>>()?;
            Ok::<_, rusqlite::Error>(rows)
        })
        .await
        .unwrap()
    }

    fn insert(v: i64) -> impl FnOnce(&mut Savepoint<'_>) -> Result<i64, rusqlite::Error> + Send + 'static {
        move |sp| {
            sp.execute("INSERT INTO t (v) VALUES (?1)", [v])?;
            Ok(v)
        }
    }

    #[tokio::test]
    async fn a_lone_write_commits_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let (conn, count) = writer(&dir).await;
        let gc = GroupCommitter::spawn(conn.clone());
        assert_eq!(gc.write(insert(1)).await.unwrap(), 1);
        assert_eq!(commits(&count), 1);
        assert_eq!(rows(&conn).await, vec![1]);
    }

    #[tokio::test]
    async fn queued_writes_share_one_commit() {
        let dir = tempfile::tempdir().unwrap();
        let (conn, count) = writer(&dir).await;
        let gc = Arc::new(GroupCommitter::spawn(conn.clone()));
        let held = hold_writer(&conn).await;
        let writes: Vec<_> = (0..20)
            .map(|v| {
                let gc = gc.clone();
                tokio::spawn(async move { gc.write(insert(v)).await })
            })
            .collect();
        for w in writes {
            w.await.unwrap().unwrap();
        }
        held.await.unwrap();
        // All 20 queued behind the held writer: one group, one commit.
        assert_eq!(commits(&count), 1);
        assert_eq!(rows(&conn).await, (0..20).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn a_group_holds_at_most_max_group_writes() {
        let dir = tempfile::tempdir().unwrap();
        let (conn, count) = writer(&dir).await;
        let gc = Arc::new(GroupCommitter::spawn(conn.clone()));
        let held = hold_writer(&conn).await;
        let n = MAX_GROUP as i64 + 1;
        let writes: Vec<_> = (0..n)
            .map(|v| {
                let gc = gc.clone();
                tokio::spawn(async move { gc.write(insert(v)).await })
            })
            .collect();
        for w in writes {
            w.await.unwrap().unwrap();
        }
        held.await.unwrap();
        assert_eq!(commits(&count), 2);
        assert_eq!(rows(&conn).await.len(), n as usize);
    }

    #[tokio::test]
    async fn a_failed_write_fails_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (conn, count) = writer(&dir).await;
        let gc = Arc::new(GroupCommitter::spawn(conn.clone()));
        let held = hold_writer(&conn).await;
        // The middle write inserts 1 and then violates the unique index:
        // its savepoint takes both statements back.
        let failing = |sp: &mut Savepoint<'_>| -> Result<i64, rusqlite::Error> {
            sp.execute("INSERT INTO t (v) VALUES (100)", [])?;
            sp.execute("INSERT INTO t (v) VALUES (1)", [])?;
            Ok(0)
        };
        let a = tokio::spawn({
            let gc = gc.clone();
            async move { gc.write(insert(1)).await }
        });
        let b = tokio::spawn({
            let gc = gc.clone();
            async move { gc.write(failing).await }
        });
        let c = tokio::spawn({
            let gc = gc.clone();
            async move { gc.write(insert(2)).await }
        });
        assert_eq!(a.await.unwrap().unwrap(), 1);
        assert!(matches!(b.await.unwrap(), Err(WriteError::Write(_))));
        assert_eq!(c.await.unwrap().unwrap(), 2);
        held.await.unwrap();
        assert_eq!(commits(&count), 1);
        assert_eq!(rows(&conn).await, vec![1, 2]);
    }

    #[tokio::test]
    async fn writes_of_a_group_see_the_earlier_ones() {
        let dir = tempfile::tempdir().unwrap();
        let (conn, _) = writer(&dir).await;
        let gc = Arc::new(GroupCommitter::spawn(conn.clone()));
        let held = hold_writer(&conn).await;
        // Two "create if absent" writes on the same value: exactly one wins.
        let create_if_absent = |sp: &mut Savepoint<'_>| -> Result<bool, rusqlite::Error> {
            let exists: bool = sp.query_row("SELECT EXISTS(SELECT 1 FROM t WHERE v = 7)", [], |r| r.get(0))?;
            if !exists {
                sp.execute("INSERT INTO t (v) VALUES (7)", [])?;
            }
            Ok(!exists)
        };
        let a = tokio::spawn({
            let gc = gc.clone();
            async move { gc.write(create_if_absent).await }
        });
        let b = tokio::spawn({
            let gc = gc.clone();
            async move { gc.write(create_if_absent).await }
        });
        let won = [a.await.unwrap().unwrap(), b.await.unwrap().unwrap()];
        held.await.unwrap();
        assert_eq!(won.iter().filter(|w| **w).count(), 1);
        assert_eq!(rows(&conn).await, vec![7]);
    }

    #[tokio::test]
    async fn a_panicking_write_fails_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (conn, _) = writer(&dir).await;
        let gc = Arc::new(GroupCommitter::spawn(conn.clone()));
        let held = hold_writer(&conn).await;
        let panicking = |sp: &mut Savepoint<'_>| -> Result<(), rusqlite::Error> {
            sp.execute("INSERT INTO t (v) VALUES (50)", [])?;
            panic!("boom");
        };
        let a = tokio::spawn({
            let gc = gc.clone();
            async move { gc.write(panicking).await }
        });
        let b = tokio::spawn({
            let gc = gc.clone();
            async move { gc.write(insert(1)).await }
        });
        assert!(matches!(a.await.unwrap(), Err(WriteError::Group(_))));
        assert_eq!(b.await.unwrap().unwrap(), 1);
        held.await.unwrap();
        assert_eq!(rows(&conn).await, vec![1]);
        // The committer is still serving.
        assert_eq!(gc.write(insert(2)).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn a_write_that_ends_the_transaction_fails_the_whole_group() {
        let dir = tempfile::tempdir().unwrap();
        let (conn, _) = writer(&dir).await;
        let gc = Arc::new(GroupCommitter::spawn(conn.clone()));
        let held = hold_writer(&conn).await;
        // Stands for SQLite aborting the transaction on an I/O error.
        let ender = |sp: &mut Savepoint<'_>| -> Result<(), rusqlite::Error> {
            sp.execute_batch("ROLLBACK")?;
            Ok(())
        };
        let a = tokio::spawn({
            let gc = gc.clone();
            async move { gc.write(insert(1)).await }
        });
        let b = tokio::spawn({
            let gc = gc.clone();
            async move { gc.write(ender).await }
        });
        assert!(matches!(a.await.unwrap(), Err(WriteError::Group(_))));
        assert!(matches!(b.await.unwrap(), Err(WriteError::Group(_))));
        held.await.unwrap();
        assert!(rows(&conn).await.is_empty());
        assert_eq!(gc.write(insert(2)).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn a_failed_commit_fails_every_write_of_the_group() {
        let dir = tempfile::tempdir().unwrap();
        let (conn, _) = writer(&dir).await;
        conn.call(|c| {
            c.commit_hook(Some(|| true)); // turns every COMMIT into a rollback
            Ok::<_, rusqlite::Error>(())
        })
        .await
        .unwrap();
        let gc = GroupCommitter::spawn(conn.clone());
        assert!(matches!(gc.write(insert(1)).await, Err(WriteError::Group(_))));
        conn.call(|c| {
            c.commit_hook(None::<fn() -> bool>);
            Ok::<_, rusqlite::Error>(())
        })
        .await
        .unwrap();
        assert!(rows(&conn).await.is_empty());
    }

    #[tokio::test]
    async fn a_closed_writer_fails_the_writes() {
        let dir = tempfile::tempdir().unwrap();
        let (conn, _) = writer(&dir).await;
        let gc = GroupCommitter::spawn(conn.clone());
        conn.close().await.unwrap();
        assert!(matches!(gc.write(insert(1)).await, Err(WriteError::Group(_))));
    }
}
