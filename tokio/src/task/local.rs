//! Runs `!Send` futures on the current thread.
use crate::loom::sync::{Arc, Mutex};
use crate::runtime::task::{self, JoinHandle, LocalOwnedTasks, Task};
use crate::sync::AtomicWaker;

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::Poll;

use pin_project_lite::pin_project;

cfg_rt! {
    /// A set of tasks which are executed on the same thread.
    ///
    /// In some cases, it is necessary to run one or more futures that do not
    /// implement [`Send`] and thus are unsafe to send between threads. In these
    /// cases, a [local task set] may be used to schedule one or more `!Send`
    /// futures to run together on the same thread.
    ///
    /// For example, the following code will not compile:
    ///
    /// ```rust,compile_fail
    /// use std::rc::Rc;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     // `Rc` does not implement `Send`, and thus may not be sent between
    ///     // threads safely.
    ///     let unsend_data = Rc::new("my unsend data...");
    ///
    ///     let unsend_data = unsend_data.clone();
    ///     // Because the `async` block here moves `unsend_data`, the future is `!Send`.
    ///     // Since `tokio::spawn` requires the spawned future to implement `Send`, this
    ///     // will not compile.
    ///     tokio::spawn(async move {
    ///         println!("{}", unsend_data);
    ///         // ...
    ///     }).await.unwrap();
    /// }
    /// ```
    ///
    /// # Use with `run_until`
    ///
    /// To spawn `!Send` futures, we can use a local task set to schedule them
    /// on the thread calling [`Runtime::block_on`]. When running inside of the
    /// local task set, we can use [`task::spawn_local`], which can spawn
    /// `!Send` futures. For example:
    ///
    /// ```rust
    /// use std::rc::Rc;
    /// use tokio::task;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let unsend_data = Rc::new("my unsend data...");
    ///
    ///     // Construct a local task set that can run `!Send` futures.
    ///     let local = task::LocalSet::new();
    ///
    ///     // Run the local task set.
    ///     local.run_until(async move {
    ///         let unsend_data = unsend_data.clone();
    ///         // `spawn_local` ensures that the future is spawned on the local
    ///         // task set.
    ///         task::spawn_local(async move {
    ///             println!("{}", unsend_data);
    ///             // ...
    ///         }).await.unwrap();
    ///     }).await;
    /// }
    /// ```
    /// **Note:** The `run_until` method can only be used in `#[tokio::main]`,
    /// `#[tokio::test]` or directly inside a call to [`Runtime::block_on`]. It
    /// cannot be used inside a task spawned with `tokio::spawn`.
    ///
    /// ## Awaiting a `LocalSet`
    ///
    /// Additionally, a `LocalSet` itself implements `Future`, completing when
    /// *all* tasks spawned on the `LocalSet` complete. This can be used to run
    /// several futures on a `LocalSet` and drive the whole set until they
    /// complete. For example,
    ///
    /// ```rust
    /// use tokio::{task, time};
    /// use std::rc::Rc;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let unsend_data = Rc::new("world");
    ///     let local = task::LocalSet::new();
    ///
    ///     let unsend_data2 = unsend_data.clone();
    ///     local.spawn_local(async move {
    ///         // ...
    ///         println!("hello {}", unsend_data2)
    ///     });
    ///
    ///     local.spawn_local(async move {
    ///         time::sleep(time::Duration::from_millis(100)).await;
    ///         println!("goodbye {}", unsend_data)
    ///     });
    ///
    ///     // ...
    ///
    ///     local.await;
    /// }
    /// ```
    /// **Note:** Awaiting a `LocalSet` can only be done inside
    /// `#[tokio::main]`, `#[tokio::test]` or directly inside a call to
    /// [`Runtime::block_on`]. It cannot be used inside a task spawned with
    /// `tokio::spawn`.
    ///
    /// ## Use inside `tokio::spawn`
    ///
    /// The two methods mentioned above cannot be used inside `tokio::spawn`, so
    /// to spawn `!Send` futures from inside `tokio::spawn`, we need to do
    /// something else. The solution is to create the `LocalSet` somewhere else,
    /// and communicate with it using an [`mpsc`] channel.
    ///
    /// The following example puts the `LocalSet` inside a new thread.
    /// ```
    /// use tokio::runtime::Builder;
    /// use tokio::sync::{mpsc, oneshot};
    /// use tokio::task::LocalSet;
    ///
    /// // This struct describes the task you want to spawn. Here we include
    /// // some simple examples. The oneshot channel allows sending a response
    /// // to the spawner.
    /// #[derive(Debug)]
    /// enum Task {
    ///     PrintNumber(u32),
    ///     AddOne(u32, oneshot::Sender<u32>),
    /// }
    ///
    /// #[derive(Clone)]
    /// struct LocalSpawner {
    ///    send: mpsc::UnboundedSender<Task>,
    /// }
    ///
    /// impl LocalSpawner {
    ///     pub fn new() -> Self {
    ///         let (send, mut recv) = mpsc::unbounded_channel();
    ///
    ///         let rt = Builder::new_current_thread()
    ///             .enable_all()
    ///             .build()
    ///             .unwrap();
    ///
    ///         std::thread::spawn(move || {
    ///             let local = LocalSet::new();
    ///
    ///             local.spawn_local(async move {
    ///                 while let Some(new_task) = recv.recv().await {
    ///                     tokio::task::spawn_local(run_task(new_task));
    ///                 }
    ///                 // If the while loop returns, then all the LocalSpawner
    ///                 // objects have have been dropped.
    ///             });
    ///
    ///             // This will return once all senders are dropped and all
    ///             // spawned tasks have returned.
    ///             rt.block_on(local);
    ///         });
    ///
    ///         Self {
    ///             send,
    ///         }
    ///     }
    ///
    ///     pub fn spawn(&self, task: Task) {
    ///         self.send.send(task).expect("Thread with LocalSet has shut down.");
    ///     }
    /// }
    ///
    /// // This task may do !Send stuff. We use printing a number as an example,
    /// // but it could be anything.
    /// //
    /// // The Task struct is an enum to support spawning many different kinds
    /// // of operations.
    /// async fn run_task(task: Task) {
    ///     match task {
    ///         Task::PrintNumber(n) => {
    ///             println!("{}", n);
    ///         },
    ///         Task::AddOne(n, response) => {
    ///             // We ignore failures to send the response.
    ///             let _ = response.send(n + 1);
    ///         },
    ///     }
    /// }
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let spawner = LocalSpawner::new();
    ///
    ///     let (send, response) = oneshot::channel();
    ///     spawner.spawn(Task::AddOne(10, send));
    ///     let eleven = response.await.unwrap();
    ///     assert_eq!(eleven, 11);
    /// }
    /// ```
    ///
    /// [`Send`]: trait@std::marker::Send
    /// [local task set]: struct@LocalSet
    /// [`Runtime::block_on`]: method@crate::runtime::Runtime::block_on
    /// [`task::spawn_local`]: fn@spawn_local
    /// [`mpsc`]: mod@crate::sync::mpsc
    pub struct LocalSet {
        /// Current scheduler tick
        tick: Cell<u8>,

        /// State available from thread-local
        context: Context,

        /// This type should not be Send.
        _not_send: PhantomData<*const ()>,
    }
}

/// State available from the thread-local
struct Context {
    /// Owned task set and local run queue
    tasks: RefCell<Tasks>,

    /// State shared between threads.
    shared: Arc<Shared>,
}

struct Tasks {
    /// Collection of all active tasks spawned onto this executor.
    owned: LocalOwnedTasks<Arc<Shared>>,

    /// Local run queue sender and receiver.
    queue: VecDeque<task::Notified<Arc<Shared>>>,
}

/// LocalSet state shared between threads.
struct Shared {
    /// Remote run queue sender
    queue: Mutex<Option<VecDeque<task::Notified<Arc<Shared>>>>>,

    /// Wake the `LocalSet` task
    waker: AtomicWaker,
}

pin_project! {
    #[derive(Debug)]
    struct RunUntil<'a, F> {
        local_set: &'a LocalSet,
        #[pin]
        future: F,
    }
}

scoped_thread_local!(static CURRENT: Context);

cfg_rt! {
    /// Spawns a `!Send` future on the local task set.
    ///
    /// The spawned future will be run on the same thread that called `spawn_local.`
    /// This may only be called from the context of a local task set.
    ///
    /// # Panics
    ///
    /// - This function panics if called outside of a local task set.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use std::rc::Rc;
    /// use tokio::task;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let unsend_data = Rc::new("my unsend data...");
    ///
    ///     let local = task::LocalSet::new();
    ///
    ///     // Run the local task set.
    ///     local.run_until(async move {
    ///         let unsend_data = unsend_data.clone();
    ///         task::spawn_local(async move {
    ///             println!("{}", unsend_data);
    ///             // ...
    ///         }).await.unwrap();
    ///     }).await;
    /// }
    /// ```
    #[cfg_attr(tokio_track_caller, track_caller)]
    pub fn spawn_local<F>(future: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        spawn_local_inner(future, None)
    }

    pub(super) fn spawn_local_inner<F>(future: F, name: Option<&str>) -> JoinHandle<F::Output>
    where F: Future + 'static,
          F::Output: 'static
    {
        let future = crate::util::trace::task(future, "local", name);
        CURRENT.with(|maybe_cx| {
            let cx = maybe_cx
                .expect("`spawn_local` called from outside of a `task::LocalSet`");

            let (handle, notified) = cx.tasks.borrow_mut().owned.bind(future, cx.shared.clone());

            if let Some(notified) = notified {
                cx.shared.schedule(notified);
            }

            handle
        })
    }
}

/// Initial queue capacity
const INITIAL_CAPACITY: usize = 64;

/// Max number of tasks to poll per tick.
const MAX_TASKS_PER_TICK: usize = 61;

/// How often it check the remote queue first
const REMOTE_FIRST_INTERVAL: u8 = 31;

impl LocalSet {
    /// Returns a new local task set.
    pub fn new() -> LocalSet {
        LocalSet {
            tick: Cell::new(0),
            context: Context {
                tasks: RefCell::new(Tasks {
                    owned: LocalOwnedTasks::new(),
                    queue: VecDeque::with_capacity(INITIAL_CAPACITY),
                }),
                shared: Arc::new(Shared {
                    queue: Mutex::new(Some(VecDeque::with_capacity(INITIAL_CAPACITY))),
                    waker: AtomicWaker::new(),
                }),
            },
            _not_send: PhantomData,
        }
    }

    /// Spawns a `!Send` task onto the local task set.
    ///
    /// This task is guaranteed to be run on the current thread.
    ///
    /// Unlike the free function [`spawn_local`], this method may be used to
    /// spawn local tasks when the task set is _not_ running. For example:
    /// ```rust
    /// use tokio::task;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let local = task::LocalSet::new();
    ///
    ///     // Spawn a future on the local set. This future will be run when
    ///     // we call `run_until` to drive the task set.
    ///     local.spawn_local(async {
    ///        // ...
    ///     });
    ///
    ///     // Run the local task set.
    ///     local.run_until(async move {
    ///         // ...
    ///     }).await;
    ///
    ///     // When `run` finishes, we can spawn _more_ futures, which will
    ///     // run in subsequent calls to `run_until`.
    ///     local.spawn_local(async {
    ///        // ...
    ///     });
    ///
    ///     local.run_until(async move {
    ///         // ...
    ///     }).await;
    /// }
    /// ```
    /// [`spawn_local`]: fn@spawn_local
    #[cfg_attr(tokio_track_caller, track_caller)]
    pub fn spawn_local<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        let future = crate::util::trace::task(future, "local", None);

        let (handle, notified) = self
            .context
            .tasks
            .borrow_mut()
            .owned
            .bind(future, self.context.shared.clone());

        if let Some(notified) = notified {
            self.context.shared.schedule(notified);
        }

        self.context.shared.waker.wake();
        handle
    }

    /// Runs a future to completion on the provided runtime, driving any local
    /// futures spawned on this task set on the current thread.
    ///
    /// This runs the given future on the runtime, blocking until it is
    /// complete, and yielding its resolved result. Any tasks or timers which
    /// the future spawns internally will be executed on the runtime. The future
    /// may also call [`spawn_local`] to spawn_local additional local futures on the
    /// current thread.
    ///
    /// This method should not be called from an asynchronous context.
    ///
    /// # Panics
    ///
    /// This function panics if the executor is at capacity, if the provided
    /// future panics, or if called within an asynchronous execution context.
    ///
    /// # Notes
    ///
    /// Since this function internally calls [`Runtime::block_on`], and drives
    /// futures in the local task set inside that call to `block_on`, the local
    /// futures may not use [in-place blocking]. If a blocking call needs to be
    /// issued from a local task, the [`spawn_blocking`] API may be used instead.
    ///
    /// For example, this will panic:
    /// ```should_panic
    /// use tokio::runtime::Runtime;
    /// use tokio::task;
    ///
    /// let rt  = Runtime::new().unwrap();
    /// let local = task::LocalSet::new();
    /// local.block_on(&rt, async {
    ///     let join = task::spawn_local(async {
    ///         let blocking_result = task::block_in_place(|| {
    ///             // ...
    ///         });
    ///         // ...
    ///     });
    ///     join.await.unwrap();
    /// })
    /// ```
    /// This, however, will not panic:
    /// ```
    /// use tokio::runtime::Runtime;
    /// use tokio::task;
    ///
    /// let rt  = Runtime::new().unwrap();
    /// let local = task::LocalSet::new();
    /// local.block_on(&rt, async {
    ///     let join = task::spawn_local(async {
    ///         let blocking_result = task::spawn_blocking(|| {
    ///             // ...
    ///         }).await;
    ///         // ...
    ///     });
    ///     join.await.unwrap();
    /// })
    /// ```
    ///
    /// [`spawn_local`]: fn@spawn_local
    /// [`Runtime::block_on`]: method@crate::runtime::Runtime::block_on
    /// [in-place blocking]: fn@crate::task::block_in_place
    /// [`spawn_blocking`]: fn@crate::task::spawn_blocking
    #[cfg(feature = "rt")]
    #[cfg_attr(docsrs, doc(cfg(feature = "rt")))]
    pub fn block_on<F>(&self, rt: &crate::runtime::Runtime, future: F) -> F::Output
    where
        F: Future,
    {
        rt.block_on(self.run_until(future))
    }

    /// Run a future to completion on the local set, returning its output.
    ///
    /// This returns a future that runs the given future with a local set,
    /// allowing it to call [`spawn_local`] to spawn additional `!Send` futures.
    /// Any local futures spawned on the local set will be driven in the
    /// background until the future passed to `run_until` completes. When the future
    /// passed to `run` finishes, any local futures which have not completed
    /// will remain on the local set, and will be driven on subsequent calls to
    /// `run_until` or when [awaiting the local set] itself.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use tokio::task;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     task::LocalSet::new().run_until(async {
    ///         task::spawn_local(async move {
    ///             // ...
    ///         }).await.unwrap();
    ///         // ...
    ///     }).await;
    /// }
    /// ```
    ///
    /// [`spawn_local`]: fn@spawn_local
    /// [awaiting the local set]: #awaiting-a-localset
    pub async fn run_until<F>(&self, future: F) -> F::Output
    where
        F: Future,
    {
        let run_until = RunUntil {
            future,
            local_set: self,
        };
        run_until.await
    }

    /// Tick the scheduler, returning whether the local future needs to be
    /// notified again.
    fn tick(&self) -> bool {
        for _ in 0..MAX_TASKS_PER_TICK {
            match self.next_task() {
                // Run the task
                //
                // Safety: As spawned tasks are `!Send`, `run_unchecked` must be
                // used. We are responsible for maintaining the invariant that
                // `run_unchecked` is only called on threads that spawned the
                // task initially. Because `LocalSet` itself is `!Send`, and
                // `spawn_local` spawns into the `LocalSet` on the current
                // thread, the invariant is maintained.
                Some(task) => crate::coop::budget(|| task.run()),
                // We have fully drained the queue of notified tasks, so the
                // local future doesn't need to be notified again — it can wait
                // until something else wakes a task in the local set.
                None => return false,
            }
        }

        true
    }

    fn next_task(&self) -> Option<task::LocalNotified<Arc<Shared>>> {
        let tick = self.tick.get();
        self.tick.set(tick.wrapping_add(1));

        let task = if tick % REMOTE_FIRST_INTERVAL == 0 {
            self.context
                .shared
                .queue
                .lock()
                .as_mut()
                .and_then(|queue| queue.pop_front())
                .or_else(|| self.context.tasks.borrow_mut().queue.pop_front())
        } else {
            self.context
                .tasks
                .borrow_mut()
                .queue
                .pop_front()
                .or_else(|| {
                    self.context
                        .shared
                        .queue
                        .lock()
                        .as_mut()
                        .and_then(|queue| queue.pop_front())
                })
        };

        task.map(|task| self.context.tasks.borrow_mut().owned.assert_owner(task))
    }

    fn with<T>(&self, f: impl FnOnce() -> T) -> T {
        CURRENT.set(&self.context, f)
    }
}

impl fmt::Debug for LocalSet {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("LocalSet").finish()
    }
}

impl Future for LocalSet {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        // Register the waker before starting to work
        self.context.shared.waker.register_by_ref(cx.waker());

        if self.with(|| self.tick()) {
            // If `tick` returns true, we need to notify the local future again:
            // there are still tasks remaining in the run queue.
            cx.waker().wake_by_ref();
            Poll::Pending
        } else if self.context.tasks.borrow().owned.is_empty() {
            // If the scheduler has no remaining futures, we're done!
            Poll::Ready(())
        } else {
            // There are still futures in the local set, but we've polled all the
            // futures in the run queue. Therefore, we can just return Pending
            // since the remaining futures will be woken from somewhere else.
            Poll::Pending
        }
    }
}

impl Default for LocalSet {
    fn default() -> LocalSet {
        LocalSet::new()
    }
}

impl Drop for LocalSet {
    fn drop(&mut self) {
        self.with(|| {
            // Close the LocalOwnedTasks. This ensures that any calls to
            // spawn_local in the destructor of a future on this LocalSet will
            // immediately cancel the task, and prevents the task from being
            // added to `owned`.
            self.context.tasks.borrow_mut().owned.close();

            // Loop required here to ensure borrow is dropped between iterations
            #[allow(clippy::while_let_loop)]
            loop {
                let task = match self.context.tasks.borrow_mut().owned.pop_back() {
                    Some(task) => task,
                    None => break,
                };

                // Safety: same as `run_unchecked`.
                task.shutdown();
            }

            // We already called shutdown on all tasks above, so there is no
            // need to call shutdown.
            for task in self.context.tasks.borrow_mut().queue.drain(..) {
                drop(task);
            }

            // Take the queue from the Shared object to prevent pushing
            // notifications to it in the future.
            let queue = self.context.shared.queue.lock().take().unwrap();
            for task in queue {
                drop(task);
            }

            assert!(self.context.tasks.borrow().owned.is_empty());
        });
    }
}

// === impl LocalFuture ===

impl<T: Future> Future for RunUntil<'_, T> {
    type Output = T::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let me = self.project();

        me.local_set.with(|| {
            me.local_set
                .context
                .shared
                .waker
                .register_by_ref(cx.waker());

            let _no_blocking = crate::runtime::enter::disallow_blocking();
            let f = me.future;

            if let Poll::Ready(output) = crate::coop::budget(|| f.poll(cx)) {
                return Poll::Ready(output);
            }

            if me.local_set.tick() {
                // If `tick` returns `true`, we need to notify the local future again:
                // there are still tasks remaining in the run queue.
                cx.waker().wake_by_ref();
            }

            Poll::Pending
        })
    }
}

impl Shared {
    /// Schedule the provided task on the scheduler.
    fn schedule(&self, task: task::Notified<Arc<Self>>) {
        CURRENT.with(|maybe_cx| match maybe_cx {
            Some(cx) if cx.shared.ptr_eq(self) => {
                cx.tasks.borrow_mut().queue.push_back(task);
            }
            _ => {
                // First check whether the queue is still there (if not, the
                // LocalSet is dropped). Then push to it if so, and if not,
                // do nothing.
                let mut lock = self.queue.lock();

                if let Some(queue) = lock.as_mut() {
                    queue.push_back(task);
                    drop(lock);
                    self.waker.wake();
                }
            }
        });
    }

    fn ptr_eq(&self, other: &Shared) -> bool {
        std::ptr::eq(self, other)
    }
}

impl task::Schedule for Arc<Shared> {
    fn release(&self, task: &Task<Self>) -> Option<Task<Self>> {
        CURRENT.with(|maybe_cx| {
            let cx = maybe_cx.expect("scheduler context missing");
            assert!(cx.shared.ptr_eq(self));
            cx.tasks.borrow_mut().owned.remove(&task)
        })
    }

    fn schedule(&self, task: task::Notified<Self>) {
        Shared::schedule(self, task);
    }
}
