//! Asynchronous client lighting rebuilds with one pending geometry snapshot.
//!
//! A completed field remains usable while newer geometry is being built. Requests
//! waiting behind an active build coalesce to the latest snapshot; generations
//! identify completed fields without an unbounded request or result queue.
use crate::map::Map;
use crate::remaster_light::{LightingError, RemasterLightMap, ShadowTexture};
use crate::world::World;
use std::io;
use std::sync::{Arc, Condvar, Mutex, TryLockError};
use std::thread::{self, JoinHandle};

/// Worker-owned alpha data in the packed format consumed by the light builder.
/// RGB bits are discarded; geometry rebuilds reuse this allocation.
pub struct OwnedShadowTexture {
    width: usize,
    height: usize,
    pixels: Vec<u32>,
}
impl OwnedShadowTexture {
    pub fn from_argb(width: usize, height: usize, pixels: &[u32]) -> Self {
        Self {
            width,
            height,
            pixels: pixels.iter().map(|pixel| pixel & 0xff00_0000).collect(),
        }
    }

    fn borrowed(&self) -> ShadowTexture<'_> {
        ShadowTexture {
            width: self.width,
            height: self.height,
            pixels: &self.pixels,
        }
    }
}

struct GeometrySnapshot {
    world: World,
    elevations: Vec<f32>,
}

pub struct LightingResult {
    pub generation: u64,
    pub result: Result<RemasterLightMap, LightingError>,
}

pub struct LightingWorker {
    worker: LatestWorker<GeometrySnapshot, Result<RemasterLightMap, LightingError>>,
}
impl LightingWorker {
    /// One worker owns the map reference and texture snapshot for a level.
    /// Thread creation errors are returned so the caller can retain its field.
    pub fn new(map: Arc<Map>, textures: Vec<Option<OwnedShadowTexture>>) -> io::Result<Self> {
        let worker = LatestWorker::new(move |snapshot: GeometrySnapshot| {
            let textures: Vec<_> = textures
                .iter()
                .map(|texture| texture.as_ref().map(OwnedShadowTexture::borrowed))
                .collect();
            RemasterLightMap::build(&map, &snapshot.world, &snapshot.elevations, &textures)
        })?;
        Ok(Self { worker })
    }

    /// Replaces a waiting snapshot, leaving the active build uninterrupted.
    /// Heights use map tiles and the ordering of `world.walls`.
    pub fn submit(&mut self, world: World, elevations: Vec<f32>) -> u64 {
        self.worker.submit(GeometrySnapshot { world, elevations })
    }

    /// Takes the newest finished field/error without waiting for the queue lock.
    /// Results may precede the latest submitted generation while it is building.
    pub fn try_result(&mut self) -> Option<LightingResult> {
        self.worker
            .try_result()
            .map(|(generation, result)| LightingResult { generation, result })
    }
}

struct State<Request, Output> {
    pending: Option<(u64, Request)>,
    completed: Option<(u64, Output)>,
    stopping: bool,
}
struct Shared<Request, Output> {
    state: Mutex<State<Request, Output>>,
    wake: Condvar,
}

/// Generic scheduling core lets tests control build completion without allocating
/// full light fields or depending on machine-specific rendering timings.
struct LatestWorker<Request, Output> {
    shared: Arc<Shared<Request, Output>>,
    thread: Option<JoinHandle<()>>,
    generation: u64,
}
impl<Request: Send + 'static, Output: Send + 'static> LatestWorker<Request, Output> {
    fn new(mut build: impl FnMut(Request) -> Output + Send + 'static) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                pending: None,
                completed: None,
                stopping: false,
            }),
            wake: Condvar::new(),
        });
        let work = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("remaster-lighting".into())
            .spawn(move || loop {
                let (generation, request) = {
                    let mut state = work.state.lock().unwrap_or_else(|error| error.into_inner());
                    while state.pending.is_none() && !state.stopping {
                        state = work
                            .wake
                            .wait(state)
                            .unwrap_or_else(|error| error.into_inner());
                    }
                    if state.stopping {
                        return;
                    }
                    state.pending.take().expect("pending work after wake")
                };
                // Neither building nor freeing replaced fields holds the queue lock.
                let output = build(request);
                let old = {
                    let mut state = work.state.lock().unwrap_or_else(|error| error.into_inner());
                    if state.stopping {
                        return;
                    }
                    state.completed.replace((generation, output))
                };
                work.wake.notify_all();
                drop(old);
            })?;
        Ok(Self {
            shared,
            thread: Some(thread),
            generation: 0,
        })
    }

    fn submit(&mut self, request: Request) -> u64 {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("lighting generation exhausted");
        let old = {
            let mut state = self
                .shared
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.pending.replace((self.generation, request))
        };
        self.shared.wake.notify_one();
        drop(old);
        self.generation
    }

    fn try_result(&mut self) -> Option<(u64, Output)> {
        let mut state = match self.shared.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => return None,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
        };
        state.completed.take()
    }
}
impl<Request, Output> Drop for LatestWorker<Request, Output> {
    fn drop(&mut self) {
        let discarded = {
            let mut state = self
                .shared
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.stopping = true;
            (state.pending.take(), state.completed.take())
        };
        self.shared.wake.notify_all();
        drop(discarded);
        if let Some(thread) = self.thread.take() {
            // A running build finishes within the light builder's work budgets;
            // queued work is discarded, and sleeping workers wake immediately.
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    const TIMEOUT: Duration = Duration::from_secs(3);

    fn wait_result<Request, Output>(worker: &LatestWorker<Request, Output>) -> (u64, Output) {
        let deadline = Instant::now() + TIMEOUT;
        let mut state = worker.shared.state.lock().unwrap();
        loop {
            if let Some(result) = state.completed.take() {
                return result;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "worker did not finish");
            state = worker.shared.wake.wait_timeout(state, remaining).unwrap().0;
        }
    }

    #[test]
    fn pending_requests_coalesce_and_results_keep_generations() {
        let (started, starts) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let mut worker = LatestWorker::new(move |request: usize| {
            started.send(request).unwrap();
            released.recv_timeout(TIMEOUT).unwrap();
            request * 10
        })
        .unwrap();
        assert_eq!(worker.submit(1), 1);
        assert_eq!(starts.recv_timeout(TIMEOUT).unwrap(), 1);
        assert_eq!(worker.submit(2), 2);
        assert_eq!(worker.submit(3), 3);
        assert_eq!(worker.shared.state.lock().unwrap().pending, Some((3, 3)));
        assert!(worker.try_result().is_none());
        release.send(()).unwrap();
        // The first field stays useful while the third request is being built.
        assert_eq!(starts.recv_timeout(TIMEOUT).unwrap(), 3);
        assert_eq!(wait_result(&worker), (1, 10));
        release.send(()).unwrap();
        assert_eq!(wait_result(&worker), (3, 30));
        assert!(worker.try_result().is_none());
        assert!(starts.try_recv().is_err());
    }

    #[test]
    fn completed_slot_replaces_old_results_and_propagates_build_errors() {
        let mut worker = LatestWorker::new(|request| {
            if request == 2 {
                Err(LightingError::SourceWorkLimit)
            } else {
                Ok(request)
            }
        })
        .unwrap();
        worker.submit(1);
        // Observe completion without consuming it, so the next result replaces it.
        {
            let mut state = worker.shared.state.lock().unwrap();
            while state.completed.is_none() {
                let (next, timeout) = worker.shared.wake.wait_timeout(state, TIMEOUT).unwrap();
                assert!(!timeout.timed_out());
                state = next;
            }
        }
        worker.submit(2);
        {
            let mut state = worker.shared.state.lock().unwrap();
            while state
                .completed
                .as_ref()
                .is_none_or(|(generation, _)| *generation != 2)
            {
                let (next, timeout) = worker.shared.wake.wait_timeout(state, TIMEOUT).unwrap();
                assert!(!timeout.timed_out());
                state = next;
            }
        }
        assert_eq!(
            worker.try_result(),
            Some((2, Err(LightingError::SourceWorkLimit)))
        );
    }

    #[test]
    fn polling_does_not_wait_for_queue_lock() {
        let mut worker = LatestWorker::new(|request: u8| request).unwrap();
        let shared = Arc::clone(&worker.shared);
        let _lock = shared.state.lock().unwrap();
        assert!(worker.try_result().is_none());
    }

    #[test]
    fn shutdown_discards_pending_work_and_joins_active_build() {
        let (started, starts) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let mut worker = LatestWorker::new(move |request| {
            started.send(request).unwrap();
            released.recv_timeout(TIMEOUT).unwrap();
            request
        })
        .unwrap();
        worker.submit(1);
        assert_eq!(starts.recv_timeout(TIMEOUT).unwrap(), 1);
        worker.submit(2);
        let shared = Arc::clone(&worker.shared);
        let dropping = thread::spawn(move || drop(worker));
        {
            let mut state = shared.state.lock().unwrap();
            while !state.stopping {
                let (next, timeout) = shared.wake.wait_timeout(state, TIMEOUT).unwrap();
                assert!(!timeout.timed_out());
                state = next;
            }
            assert!(state.pending.is_none());
        }
        release.send(()).unwrap();
        dropping.join().unwrap();
        assert!(starts.try_recv().is_err());
        assert!(shared.state.lock().unwrap().completed.is_none());
    }

    #[test]
    fn idle_worker_shuts_down_and_texture_snapshot_keeps_only_alpha() {
        let worker = LatestWorker::new(|request: u8| request).unwrap();
        drop(worker);
        let owned = OwnedShadowTexture::from_argb(2, 1, &[0x8040_2010, 0xff12_3456]);
        let borrowed = owned.borrowed();
        assert_eq!((borrowed.width, borrowed.height), (2, 1));
        assert_eq!(borrowed.pixels, &[0x8000_0000, 0xff00_0000]);
    }
}
