// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

//! Scheduler which schedules the execution of `storage::Command`s.
//!
//! There is one scheduler for each store. It receives commands from clients, executes them against
//! the MVCC layer storage engine.
//!
//! Logically, the data organization hierarchy from bottom to top is row -> region -> store ->
//! database. But each region is replicated onto N stores for reliability, the replicas form a Raft
//! group, one of which acts as the leader. When the client read or write a row, the command is
//! sent to the scheduler which is on the region leader's store.
//!
//! Scheduler runs in a single-thread event loop, but command executions are delegated to a pool of
//! worker thread.
//!
//! Scheduler keeps track of all the running commands and uses latches to ensure serialized access
//! to the overlapping rows involved in concurrent commands. But note that scheduler only ensures
//! serialized access to the overlapping rows at command level, but a transaction may consist of
//! multiple commands, therefore conflicts may happen at transaction level. Transaction semantics
//! is ensured by the transaction protocol implemented in the client library, which is transparent
//! to the scheduler.

use std::fmt::{self, Debug, Display, Formatter};
use std::u64;

use kvproto::kvrpcpb::CommandPri;

use storage::engine::{CbContext, Result as EngineResult};
use storage::Key;
use storage::{Command, Engine, Error as StorageError, StorageCb};
use util::collections::HashMap;
use util::threadpool::{ThreadPool, ThreadPoolBuilder};
use util::worker::{self, Runnable};

use super::super::metrics::*;
use super::latch::{Latches, Lock};
use super::process::{execute_callback, ProcessResult, SchedContext, SchedContextFactory, Task};
use super::Error;

pub const CMD_BATCH_SIZE: usize = 256;

/// Message types for the scheduler event loop.
pub enum Msg {
    Quit,
    RawCmd {
        cmd: Command,
        cb: StorageCb,
    },
    ReadFinished {
        task: Task,
        pr: ProcessResult,
    },
    WriteFinished {
        task: Task,
        pr: ProcessResult,
        cb_ctx: CbContext,
        result: EngineResult<()>,
    },
    FinishedWithErr {
        cid: u64,
        err: Error,
    },
}

/// Debug for messages.
impl Debug for Msg {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "{}", self)
    }
}

/// Display for messages.
impl Display for Msg {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match *self {
            Msg::Quit => write!(f, "Quit"),
            Msg::RawCmd { ref cmd, .. } => write!(f, "RawCmd {:?}", cmd),
            Msg::ReadFinished { ref task, .. } => write!(f, "ReadFinished [cid={}]", task.cid),
            Msg::WriteFinished { ref task, .. } => write!(f, "WriteFinished [cid={}]", task.cid),
            Msg::FinishedWithErr { cid, .. } => write!(f, "WriteFinished [cid={}]", cid),
        }
    }
}

struct TaskCallback {
    lock: Lock,
    cb: StorageCb,
    write_bytes: usize,

    tag: &'static str,
}

impl TaskCallback {
    fn new(lock: Lock, cb: StorageCb, write_bytes: usize, tag: &'static str) -> TaskCallback {
        TaskCallback {
            lock,
            cb,
            write_bytes,
            tag,
        }
    }
}

/// Scheduler which schedules the execution of `storage::Command`s.
pub struct Scheduler<E: Engine> {
    engine: E,

    // cid -> Task
    pub cmd_ctxs: HashMap<u64, Task>,

    // cid -> TaskCallback
    pending_taskcbs: HashMap<u64, TaskCallback>,

    // actual scheduler to schedule the execution of commands
    pub scheduler: worker::Scheduler<Msg>,

    // cmd id generator
    id_alloc: u64,

    // write concurrency control
    latches: Latches,

    // TODO: Dynamically calculate this value according to processing
    // speed of recent write requests.
    sched_pending_write_threshold: usize,

    // worker pool
    worker_pool: ThreadPool<SchedContext<E>>,

    // high priority commands will be delivered to this pool
    high_priority_pool: ThreadPool<SchedContext<E>>,

    // used to control write flow
    running_write_bytes: usize,
}

impl<E: Engine> Scheduler<E> {
    /// Creates a scheduler.
    pub fn new(
        engine: E,
        scheduler: worker::Scheduler<Msg>,
        concurrency: usize,
        worker_pool_size: usize,
        sched_pending_write_threshold: usize,
    ) -> Self {
        let factory = SchedContextFactory::new(engine.clone());
        Scheduler {
            engine,
            // TODO: GC these two maps.
            cmd_ctxs: Default::default(),
            pending_taskcbs: Default::default(),
            scheduler,
            id_alloc: 0,
            latches: Latches::new(concurrency),
            sched_pending_write_threshold,
            worker_pool: ThreadPoolBuilder::new(thd_name!("sched-worker-pool"), factory.clone())
                .thread_count(worker_pool_size)
                .build(),
            high_priority_pool: ThreadPoolBuilder::new(thd_name!("sched-high-pri-pool"), factory)
                .build(),
            running_write_bytes: 0,
        }
    }

    /// Generates the next command ID.
    fn gen_id(&mut self) -> u64 {
        self.id_alloc += 1;
        self.id_alloc
    }

    fn insert_task(&mut self, ctx: Task) {
        let cid = ctx.cid;
        if self.cmd_ctxs.insert(cid, ctx).is_some() {
            panic!("command cid={} shouldn't exist", cid);
        }
        SCHED_WRITING_BYTES_GAUGE.set(self.running_write_bytes as i64);
        SCHED_CONTEX_GAUGE.set(self.cmd_ctxs.len() as i64);
    }

    fn remove_task(&mut self, cid: u64) -> Task {
        let task = self.cmd_ctxs.remove(&cid).unwrap();
        assert_eq!(task.cid, cid);
        task
    }

    fn insert_taskcb(&mut self, cid: u64, taskcb: TaskCallback) {
        if taskcb.lock.is_write_lock() {
            self.running_write_bytes += taskcb.write_bytes;
        }
        if self.pending_taskcbs.insert(cid, taskcb).is_some() {
            panic!("TaskCallbakc cid={} shouldn't exist", cid);
        }
    }

    fn remove_taskcb(&mut self, cid: u64) -> TaskCallback {
        self.pending_taskcbs.remove(&cid).unwrap()
    }

    fn on_command_finished(&mut self, write_bytes: usize) {
        self.running_write_bytes -= write_bytes;
        SCHED_WRITING_BYTES_GAUGE.set(self.running_write_bytes as i64);
        SCHED_CONTEX_GAUGE.set(self.cmd_ctxs.len() as i64);
    }

    pub fn fetch_worker_pool(&self, priority: CommandPri) -> &ThreadPool<SchedContext<E>> {
        match priority {
            CommandPri::Low | CommandPri::Normal => &self.worker_pool,
            CommandPri::High => &self.high_priority_pool,
        }
    }

    /// Event handler for new command.
    ///
    /// This method will try to acquire all the necessary latches. If all the necessary latches are
    /// acquired, the method initiates a get snapshot operation for furthur processing; otherwise,
    /// the method adds the command to the waiting queue(s). The command will be handled later in
    /// `lock_and_get_snapshot` when its turn comes.
    ///
    /// Note that once a command is ready to execute, the snapshot is always up-to-date during the
    /// execution because 1) all the conflicting commands (if any) must be in the waiting queues;
    /// 2) there may be non-conflicitng commands running concurrently, but it doesn't matter.
    fn schedule_command(&mut self, cmd: Command, callback: StorageCb) {
        let tag = cmd.tag();
        SCHED_STAGE_COUNTER_VEC
            .with_label_values(&[tag, "new"])
            .inc();
        SCHED_COMMANDS_PRI_COUNTER_VEC
            .with_label_values(&[cmd.priority_tag()])
            .inc();

        let cid = self.gen_id();
        debug!("received new command, cid={}, cmd={}", cid, cmd);

        let lock = gen_command_lock(&self.latches, &cmd);
        let write_bytes = if lock.is_write_lock() {
            cmd.write_bytes()
        } else {
            0
        };
        let taskcb = TaskCallback::new(lock, callback, write_bytes, tag);
        self.insert_taskcb(cid, taskcb);

        let task = Task::new(cid, cmd);
        self.insert_task(task);

        self.lock_and_get_snapshot(cid);
    }

    fn too_busy(&self) -> bool {
        fail_point!("txn_scheduler_busy", |_| true);
        self.running_write_bytes >= self.sched_pending_write_threshold
    }

    fn on_receive_new_cmd(&mut self, cmd: Command, callback: StorageCb) {
        // write flow control
        if cmd.need_flow_control() && self.too_busy() {
            SCHED_TOO_BUSY_COUNTER_VEC
                .with_label_values(&[cmd.tag()])
                .inc();
            execute_callback(
                callback,
                ProcessResult::Failed {
                    err: StorageError::SchedTooBusy,
                },
            );
            return;
        }
        self.schedule_command(cmd, callback);
    }

    /// Initiates an async operation to get a snapshot from the storage engine, then posts a
    /// `SnapshotFinished` message back to the event loop when it finishes.
    fn get_snapshot(&mut self, cid: u64) {
        let mut task = self.remove_task(cid);

        let tag = task.tag;
        SCHED_STAGE_COUNTER_VEC
            .with_label_values(&[tag, "snapshot"])
            .inc();

        let ctx = task.cmd.as_ref().unwrap().get_context().clone();
        let scheduler = self.scheduler.clone();
        let pool_scheduler = self
            .fetch_worker_pool(task.cmd.as_ref().unwrap().priority())
            .scheduler();

        task.start();
        let cb = box move |(cb_ctx, snapshot)| {
            task.on_snapshot_finished(cb_ctx, snapshot, pool_scheduler, scheduler);
        };
        if let Err(e) = self.engine.async_snapshot(&ctx, cb) {
            error!("engine async_snapshot failed, err: {:?}", e);
            self.finish_with_err(cid, e.into());
        }
    }

    /// Calls the callback with an error.
    fn finish_with_err(&mut self, cid: u64, err: Error) {
        debug!("command cid={}, finished with error", cid);
        let taskcb = self.remove_taskcb(cid);

        SCHED_STAGE_COUNTER_VEC
            .with_label_values(&[taskcb.tag, "error"])
            .inc();

        let pr = ProcessResult::Failed {
            err: StorageError::from(err),
        };
        execute_callback(taskcb.cb, pr);

        self.release_lock(&taskcb.lock, cid);
        self.on_command_finished(taskcb.write_bytes);
    }

    /// Event handler for the success of read.
    ///
    /// If a next command is present, continues to execute; otherwise, delivers the result to the
    /// callback.
    fn on_read_finished(&mut self, task: Task, pr: ProcessResult) {
        debug!("read command(cid={}) finished", task.cid);
        SCHED_STAGE_COUNTER_VEC
            .with_label_values(&[task.tag, "read_finish"])
            .inc();
        let taskcb = self.remove_taskcb(task.cid);
        if let ProcessResult::NextCommand { cmd } = pr {
            SCHED_STAGE_COUNTER_VEC
                .with_label_values(&[task.tag, "next_cmd"])
                .inc();
            self.schedule_command(cmd, taskcb.cb);
        } else {
            execute_callback(taskcb.cb, pr);
        }

        self.release_lock(&taskcb.lock, task.cid);
        self.on_command_finished(taskcb.write_bytes);
    }

    /// Event handler for the success of write.
    fn on_write_finished(&mut self, task: Task, pr: ProcessResult, result: EngineResult<()>) {
        SCHED_STAGE_COUNTER_VEC
            .with_label_values(&[task.tag, "write_finish"])
            .inc();
        debug!("write finished for command, cid={}", task.cid);
        let taskcb = self.remove_taskcb(task.cid);
        let pr = match result {
            Ok(()) => pr,
            Err(e) => ProcessResult::Failed {
                err: ::storage::Error::from(e),
            },
        };
        if let ProcessResult::NextCommand { cmd } = pr {
            SCHED_STAGE_COUNTER_VEC
                .with_label_values(&[task.tag, "next_cmd"])
                .inc();
            self.schedule_command(cmd, taskcb.cb);
        } else {
            execute_callback(taskcb.cb, pr);
        }

        self.release_lock(&taskcb.lock, task.cid);
        self.on_command_finished(taskcb.write_bytes);
    }

    /// Tries to acquire all the required latches for a command.
    ///
    /// Returns true if successful; returns false otherwise.
    fn acquire_lock(&mut self, cid: u64) -> bool {
        let taskcb = &mut self.pending_taskcbs.get_mut(&cid).unwrap();
        self.latches.acquire(&mut taskcb.lock, cid)
    }

    /// Releases all the latches held by a command.
    fn release_lock(&mut self, lock: &Lock, cid: u64) {
        let wakeup_list = self.latches.release(lock, cid);
        for wcid in wakeup_list {
            self.lock_and_get_snapshot(wcid);
        }
    }

    /// Tries to acquire all the necessary latches. If all the necessary latches are acquired,
    /// the method initiates a get snapshot operation for furthur processing.
    fn lock_and_get_snapshot(&mut self, cid: u64) {
        if self.acquire_lock(cid) {
            self.get_snapshot(cid);
        }
    }
}

impl<E: Engine> Runnable<Msg> for Scheduler<E> {
    fn run(&mut self, _: Msg) {
        unimplemented!()
    }

    fn run_batch(&mut self, msgs: &mut Vec<Msg>) {
        for msg in msgs.drain(..) {
            match msg {
                Msg::Quit => {
                    self.shutdown();
                    return;
                }
                Msg::RawCmd { cmd, cb } => self.on_receive_new_cmd(cmd, cb),
                Msg::ReadFinished { task, pr } => self.on_read_finished(task, pr),
                Msg::WriteFinished {
                    task, pr, result, ..
                } => self.on_write_finished(task, pr, result),
                Msg::FinishedWithErr { cid, err } => self.finish_with_err(cid, err),
            }
        }
    }

    fn shutdown(&mut self) {
        if let Err(e) = self.worker_pool.stop() {
            error!("scheduler run err when worker pool stop:{:?}", e);
        }
        if let Err(e) = self.high_priority_pool.stop() {
            error!("scheduler run err when high priority pool stop:{:?}", e);
        }
        info!("scheduler stopped");
    }
}

/// Generates the lock for a command.
///
/// Basically, read-only commands require no latches, write commands require latches hashed
/// by the referenced keys.
pub fn gen_command_lock(latches: &Latches, cmd: &Command) -> Lock {
    match *cmd {
        Command::Prewrite { ref mutations, .. } => {
            let keys: Vec<&Key> = mutations.iter().map(|x| x.key()).collect();
            latches.gen_lock(&keys)
        }
        Command::ResolveLock { ref key_locks, .. } => {
            let keys: Vec<&Key> = key_locks.iter().map(|x| &x.0).collect();
            latches.gen_lock(&keys)
        }
        Command::Commit { ref keys, .. } | Command::Rollback { ref keys, .. } => {
            latches.gen_lock(keys)
        }
        Command::Cleanup { ref key, .. } => latches.gen_lock(&[key]),
        _ => Lock::new(vec![]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kvproto::kvrpcpb::Context;
    use storage::mvcc;
    use storage::txn::latch::*;
    use storage::{Command, Key, Mutation, Options};
    use util::collections::HashMap;

    #[test]
    fn test_command_latches() {
        let mut temp_map = HashMap::default();
        temp_map.insert(10, 20);
        let readonly_cmds = vec![
            Command::ScanLock {
                ctx: Context::new(),
                max_ts: 5,
                start_key: None,
                limit: 0,
            },
            Command::ResolveLock {
                ctx: Context::new(),
                txn_status: temp_map.clone(),
                scan_key: None,
                key_locks: vec![],
            },
            Command::MvccByKey {
                ctx: Context::new(),
                key: Key::from_raw(b"k"),
            },
            Command::MvccByStartTs {
                ctx: Context::new(),
                start_ts: 25,
            },
        ];
        let write_cmds = vec![
            Command::Prewrite {
                ctx: Context::new(),
                mutations: vec![Mutation::Put((Key::from_raw(b"k"), b"v".to_vec()))],
                primary: b"k".to_vec(),
                start_ts: 10,
                options: Options::default(),
            },
            Command::Commit {
                ctx: Context::new(),
                keys: vec![Key::from_raw(b"k")],
                lock_ts: 10,
                commit_ts: 20,
            },
            Command::Cleanup {
                ctx: Context::new(),
                key: Key::from_raw(b"k"),
                start_ts: 10,
            },
            Command::Rollback {
                ctx: Context::new(),
                keys: vec![Key::from_raw(b"k")],
                start_ts: 10,
            },
            Command::ResolveLock {
                ctx: Context::new(),
                txn_status: temp_map.clone(),
                scan_key: None,
                key_locks: vec![(
                    Key::from_raw(b"k"),
                    mvcc::Lock::new(mvcc::LockType::Put, b"k".to_vec(), 10, 20, None),
                )],
            },
        ];

        let mut latches = Latches::new(1024);

        let write_locks: Vec<Lock> = write_cmds
            .into_iter()
            .enumerate()
            .map(|(id, cmd)| {
                let mut lock = gen_command_lock(&latches, &cmd);
                assert_eq!(latches.acquire(&mut lock, id as u64), id == 0);
                lock
            })
            .collect();

        for (id, cmd) in readonly_cmds.iter().enumerate() {
            let mut lock = gen_command_lock(&latches, cmd);
            assert!(latches.acquire(&mut lock, id as u64));
        }

        // acquire/release locks one by one.
        let max_id = write_locks.len() as u64 - 1;
        for (id, mut lock) in write_locks.into_iter().enumerate() {
            let id = id as u64;
            if id != 0 {
                assert!(latches.acquire(&mut lock, id));
            }
            let unlocked = latches.release(&lock, id);
            if id as u64 == max_id {
                assert!(unlocked.is_empty());
            } else {
                assert_eq!(unlocked, vec![id + 1]);
            }
        }
    }
}
