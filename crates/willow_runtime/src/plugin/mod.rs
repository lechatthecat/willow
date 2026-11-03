//! Explicit isolated plugin sessions for owned byte messages. Static bridges and
//! `native_library` remain trusted in-process APIs. No implicit fallback exists.
//! A session has one owner; cancellation tokens may be used from other threads.
//! `drive_one` executes one queued request without holding a host lock.
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{
    Arc,
    atomic::{AtomicU8, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};
pub use willow_abi::plugin::{Budgets, Error, Manifest, Mode};
use willow_abi::plugin::{Header, Tag};
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod native;
#[cfg(test)]
mod tests;
mod wasm;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Starting,
    Running,
    Draining,
    Stopped,
    Failed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RequestId {
    generation: u64,
    number: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceId {
    generation: u64,
    number: u64,
}
#[derive(Clone)]
pub struct Cancellation(Arc<AtomicU8>);
impl Cancellation {
    /// Returns true only for the transition that wins against response delivery.
    pub fn cancel(&self) -> bool {
        self.0
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    fn check(&self, deadline: Instant) -> Result<(), Error> {
        if self.0.load(Ordering::Acquire) == 1 {
            Err(Error::Cancelled)
        } else if Instant::now() >= deadline {
            Err(Error::Timeout)
        } else {
            Ok(())
        }
    }
    fn finish(&self, result: Result<Vec<u8>, Error>) -> Result<Vec<u8>, Error> {
        match self
            .0
            .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => result,
            Err(1) => Err(Error::Cancelled),
            _ => result,
        }
    }
}
struct Pending {
    header: Header,
    input: Vec<u8>,
    reserved: usize,
    deadline: Instant,
    cancellation: Cancellation,
    result: Option<Result<Vec<u8>, Error>>,
    callbacks: VecDeque<Vec<u8>>,
}
pub(crate) type Resources = HashMap<u64, Arc<[u8]>>;
enum Backend {
    Wasm(Box<wasm::Wasm>),
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    Native(native::Native),
}
impl Backend {
    fn callbacks(&mut self) -> VecDeque<Vec<u8>> {
        match self {
            Self::Wasm(w) => w.callbacks(),
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            Self::Native(n) => std::mem::take(&mut n.callbacks),
        }
    }
    fn call(
        &mut self,
        header: Header,
        input: &[u8],
        cancel: &Cancellation,
        deadline: Instant,
    ) -> Result<Vec<u8>, Error> {
        match self {
            Self::Wasm(w) => w.call(header, input, cancel, deadline),
            #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
            Self::Native(n) => n.call(header, input, cancel, deadline),
        }
    }
}

pub struct Session {
    manifest: Manifest,
    generation: u64,
    next_request: u64,
    state: State,
    backend: Option<Backend>,
    methods: HashSet<u32>,
    resource_names: HashMap<String, ResourceId>,
    pending: HashMap<RequestId, Pending>,
    queue: VecDeque<RequestId>,
    reserved: usize,
    dispatches: u64,
    completions: u64,
}
impl Session {
    /// Resources are immutable snapshots, not paths into the host filesystem.
    /// Native plugins read them at `/resources/<one-based manifest index>`.
    /// Artifact bytes must be supplied by the package/distribution resolver.
    pub fn start(
        manifest: Manifest,
        artifact: &[u8],
        resources: HashMap<String, Vec<u8>>,
    ) -> Result<Self, Error> {
        manifest.validate()?;
        if artifact.len() > manifest.budgets.artifact_bytes {
            return Err(Error::ResourceLimit);
        }
        if digest(artifact) != manifest.digest {
            return Err(Error::DigestMismatch);
        }
        if resources.len() != manifest.read_resources.len() {
            return Err(Error::DeniedCapability);
        }
        let mut bytes = 0usize;
        for data in resources.values() {
            bytes = bytes.checked_add(data.len()).ok_or(Error::ResourceLimit)?;
            if bytes > manifest.budgets.resource_bytes {
                return Err(Error::ResourceLimit);
            }
        }
        static GENERATION: AtomicU64 = AtomicU64::new(1);
        let generation = GENERATION
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| Error::ResourceLimit)?;
        let mut owned = Resources::new();
        let mut resource_names = HashMap::new();
        let mut resources = resources;
        for (index, name) in manifest.read_resources.iter().enumerate() {
            let number = index as u64 + 1;
            owned.insert(
                number,
                Arc::from(resources.remove(name).ok_or(Error::DeniedCapability)?),
            );
            resource_names.insert(name.clone(), ResourceId { generation, number });
        }
        let mut session = Self {
            methods: manifest.methods.iter().copied().collect(),
            manifest,
            generation,
            next_request: 1,
            state: State::Starting,
            backend: None,
            resource_names,
            pending: HashMap::new(),
            queue: VecDeque::new(),
            reserved: 0,
            dispatches: 0,
            completions: 0,
        };
        session.backend = Some(match session.manifest.mode {
            Mode::Wasm => Backend::Wasm(Box::new(wasm::Wasm::start(
                &session.manifest,
                artifact,
                owned,
            )?)),
            Mode::NativeProcess => {
                #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
                {
                    Backend::Native(native::Native::start(&session.manifest, artifact, owned)?)
                }
                #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
                {
                    return Err(Error::IsolationUnavailable);
                }
            }
        });
        session.state = State::Running;
        Ok(session)
    }
    pub fn state(&self) -> State {
        self.state
    }
    pub fn resource(&self, name: &str) -> Option<ResourceId> {
        self.resource_names.get(name).copied()
    }
    /// Machine-readable, host-owned configuration; never contains guest diagnostics.
    pub fn diagnostics(&self) -> serde_json::Value {
        let read_resources = if self.state == State::Running {
            self.manifest.read_resources.as_slice()
        } else {
            &[]
        };
        serde_json::json!({ "mode": self.manifest.mode, "engine": self.manifest.engine,
            "target": self.manifest.target, "read_resources": read_resources,
            "filesystem": "immutable_resource_snapshots_only", "network": false,
            "environment": false, "child_processes": false, "state": format!("{:?}", self.state) })
    }
    pub fn counters(&self) -> (u64, u64, usize, usize) {
        (
            self.dispatches,
            self.completions,
            self.pending.len(),
            self.reserved,
        )
    }
    pub fn submit(
        &mut self,
        method: u32,
        resource: Option<ResourceId>,
        payload: &[u8],
    ) -> Result<(RequestId, Cancellation), Error> {
        if self.state != State::Running {
            return Err(Error::Stopped);
        }
        if !self.methods.contains(&method) {
            return Err(Error::DeniedCapability);
        }
        let resource = match resource {
            Some(r)
                if r.generation == self.generation
                    && r.number > 0
                    && r.number <= self.resource_names.len() as u64 =>
            {
                r.number
            }
            Some(_) => return Err(Error::DeniedCapability),
            None => 0,
        };
        let b = &self.manifest.budgets;
        let reserved = payload
            .len()
            .checked_add(b.payload_bytes)
            .ok_or(Error::ResourceLimit)?;
        if payload.len() > b.payload_bytes
            || self.pending.len() >= b.requests
            || reserved > b.queued_bytes.saturating_sub(self.reserved)
        {
            return Err(Error::ResourceLimit);
        }
        let next = self
            .next_request
            .checked_add(1)
            .ok_or(Error::ResourceLimit)?;
        let id = RequestId {
            generation: self.generation,
            number: self.next_request,
        };
        self.next_request = next;
        let cancellation = Cancellation(Arc::new(AtomicU8::new(0)));
        self.pending.insert(
            id,
            Pending {
                header: Header {
                    tag: Tag::Request,
                    method,
                    generation: self.generation,
                    request: id.number,
                    resource,
                    length: payload.len(),
                },
                input: payload.to_vec(),
                reserved,
                deadline: Instant::now() + Duration::from_millis(b.deadline_ms.into()),
                cancellation: cancellation.clone(),
                result: None,
                callbacks: VecDeque::new(),
            },
        );
        self.queue.push_back(id);
        self.reserved += reserved;
        Ok((id, cancellation))
    }
    /// Serial dispatch keeps transport state private. Other threads may cancel
    /// through tokens while this method runs. Completed results retain their
    /// reservation until taken, so an unresponsive consumer cannot grow memory.
    pub fn drive_one(&mut self) -> bool {
        let Some(id) = self.queue.pop_front() else {
            return false;
        };
        let p = self.pending.get_mut(&id).expect("queued request");
        let mut dispatched = false;
        let result = match p.cancellation.check(p.deadline) {
            Err(error) => Err(error),
            Ok(()) => {
                self.dispatches += 1;
                dispatched = true;
                self.backend.as_mut().expect("running backend").call(
                    p.header,
                    &p.input,
                    &p.cancellation,
                    p.deadline,
                )
            }
        };
        let result = result.and_then(|bytes| p.cancellation.check(p.deadline).map(|()| bytes));
        let failure = if dispatched {
            result.as_ref().err().copied()
        } else {
            None
        };
        p.input = Vec::new();
        p.result = Some(p.cancellation.finish(result));
        if dispatched {
            let callbacks = self.backend.as_mut().unwrap().callbacks();
            if p.result.as_ref().unwrap().is_ok() {
                p.callbacks = callbacks;
            }
        }
        self.completions += 1;
        if let Some(error) = failure {
            self.fail(error);
        }
        true
    }
    /// Callback bytes are delivered only after successful execution, on the
    /// owner thread. Drain before `take`; taking the result discards leftovers.
    pub fn take_callback(&mut self, id: RequestId) -> Option<Vec<u8>> {
        self.pending.get_mut(&id)?.callbacks.pop_front()
    }
    pub fn take(&mut self, id: RequestId) -> Option<Result<Vec<u8>, Error>> {
        self.pending.get(&id)?.result.as_ref()?;
        let p = self.pending.remove(&id).unwrap();
        self.reserved -= p.reserved;
        p.result
    }
    fn fail(&mut self, error: Error) {
        self.state = State::Failed;
        self.backend.take();
        self.resource_names.clear();
        self.queue.clear();
        for p in self.pending.values_mut() {
            if p.result.is_none() {
                p.input = Vec::new();
                p.result = Some(p.cancellation.finish(Err(error)));
                self.completions += 1;
            }
        }
    }
    pub fn shutdown(&mut self) {
        if self.state == State::Stopped {
            return;
        }
        self.state = State::Draining;
        self.fail(Error::Cancelled);
        self.state = State::Stopped;
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.shutdown();
    }
}
pub fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
