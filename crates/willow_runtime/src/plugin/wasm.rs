use super::*;
use wasmi::{
    Caller, CompilationMode, Config, EnforcedLimits, Engine, Linker, Memory, Module, Store,
    StoreLimits, StoreLimitsBuilder, TypedFunc, TypedResumableCall,
};
struct Host {
    limits: StoreLimits,
    resources: Resources,
    payload_limit: usize,
    callbacks: VecDeque<Vec<u8>>,
    callback_bytes: usize,
    execution: Option<(Cancellation, Instant)>,
    host_bytes: u64,
    failure: Option<Error>,
}
impl Host {
    fn reject(&mut self, error: Error) -> wasmi::Error {
        self.failure = Some(error);
        wasmi::Error::new("host request rejected")
    }
    fn checkpoint(&mut self, bytes: usize) -> Result<(), wasmi::Error> {
        let error = self
            .execution
            .as_ref()
            .and_then(|(cancel, deadline)| cancel.check(*deadline).err())
            .or_else(|| (bytes as u64 > self.host_bytes).then_some(Error::ResourceLimit));
        if let Some(error) = error {
            self.failure = Some(error);
            return Err(wasmi::Error::new("host budget exhausted"));
        }
        self.host_bytes -= bytes as u64;
        Ok(())
    }
}
pub(super) struct Wasm {
    store: Store<Host>,
    memory: Memory,
    call: TypedFunc<(i32, i32, i32, i64), i64>,
    budgets: Budgets,
}
impl Wasm {
    pub fn start(
        manifest: &Manifest,
        artifact: &[u8],
        resources: Resources,
    ) -> Result<Self, Error> {
        let mut config = Config::default();
        config
            .consume_fuel(true)
            .enforced_limits(EnforcedLimits::strict())
            .compilation_mode(CompilationMode::Eager)
            .wasm_multi_memory(false)
            .set_max_recursion_depth(128)
            .set_max_stack_height(1 << 20)
            .set_max_cached_stacks(1);
        let engine = Engine::new(&config);
        let module = Module::new(&engine, artifact).map_err(|_| Error::ProtocolMismatch)?;
        for import in module.imports() {
            if import.module() != "willow"
                || !matches!(import.name(), "resource_read" | "callback")
                || (import.name() == "resource_read" && manifest.read_resources.is_empty())
            {
                return Err(Error::DeniedCapability);
            }
        }
        let limits = StoreLimitsBuilder::new()
            .memory_size(manifest.budgets.memory_bytes)
            .memories(1)
            .tables(1)
            .table_elements(4096)
            .instances(1)
            .trap_on_grow_failure(true)
            .build();
        let mut store = Store::new(
            &engine,
            Host {
                limits,
                resources,
                payload_limit: manifest.budgets.payload_bytes,
                callbacks: VecDeque::new(),
                callback_bytes: 0,
                execution: None,
                host_bytes: manifest.budgets.fuel.min(10000),
                failure: None,
            },
        );
        store.limiter(|h| &mut h.limits);
        store
            .set_fuel(manifest.budgets.fuel.min(10000))
            .map_err(|_| Error::IsolationUnavailable)?;
        let mut linker = Linker::new(&engine);
        if !manifest.read_resources.is_empty() {
            linker
                .func_wrap(
                    "willow",
                    "resource_read",
                    |mut caller: Caller<'_, Host>,
                     id: i64,
                     offset: i32,
                     length: i32|
                     -> Result<i32, wasmi::Error> {
                        let Some(data) = caller.data().resources.get(&(id as u64)).cloned() else {
                            return Err(caller.data_mut().reject(Error::DeniedCapability));
                        };
                        let length = length as u32 as usize;
                        if length > caller.data().payload_limit || length > data.len() {
                            return Err(caller.data_mut().reject(Error::ResourceLimit));
                        }
                        let Some(memory) =
                            caller.get_export("memory").and_then(|e| e.into_memory())
                        else {
                            return Err(caller.data_mut().reject(Error::ProtocolMismatch));
                        };
                        caller.data_mut().checkpoint(length)?;
                        if memory
                            .write(&mut caller, offset as u32 as usize, &data[..length])
                            .is_err()
                        {
                            return Err(caller.data_mut().reject(Error::ProtocolMismatch));
                        }
                        Ok(length as i32)
                    },
                )
                .map_err(|_| Error::IsolationUnavailable)?;
        }
        linker
            .func_wrap(
                "willow",
                "callback",
                |mut caller: Caller<'_, Host>,
                 offset: i32,
                 length: i32|
                 -> Result<i32, wasmi::Error> {
                    let length = length as u32 as usize;
                    let host = caller.data();
                    if host.callbacks.len() == 16
                        || length > host.payload_limit - host.callback_bytes
                    {
                        return Ok(-1);
                    }
                    let Some(memory) = caller.get_export("memory").and_then(|e| e.into_memory())
                    else {
                        return Ok(-1);
                    };
                    let offset = offset as u32 as usize;
                    let Some(end) = offset.checked_add(length) else {
                        return Ok(-1);
                    };
                    caller.data_mut().checkpoint(length)?;
                    let Some(data) = memory.data(&caller).get(offset..end) else {
                        return Ok(-1);
                    };
                    let owned = data.to_vec();
                    caller.data_mut().callbacks.push_back(owned);
                    caller.data_mut().callback_bytes += length;
                    Ok(0)
                },
            )
            .map_err(|_| Error::IsolationUnavailable)?;
        let instance = linker
            .instantiate_and_start(&mut store, &module)
            .map_err(|_| store.data().failure.unwrap_or(Error::Trap))?;
        let memory = instance
            .get_memory(&store, "memory")
            .ok_or(Error::ProtocolMismatch)?;
        let call = instance
            .get_typed_func(&store, "call")
            .map_err(|_| Error::ProtocolMismatch)?;
        store.data_mut().callbacks.clear();
        store.data_mut().callback_bytes = 0;
        Ok(Self {
            store,
            memory,
            call,
            budgets: manifest.budgets.clone(),
        })
    }
    pub fn callbacks(&mut self) -> VecDeque<Vec<u8>> {
        self.store.data_mut().callback_bytes = 0;
        std::mem::take(&mut self.store.data_mut().callbacks)
    }
    pub fn call(
        &mut self,
        header: Header,
        input: &[u8],
        cancel: &Cancellation,
        deadline: Instant,
    ) -> Result<Vec<u8>, Error> {
        cancel.check(deadline)?;
        self.store.data_mut().execution = Some((cancel.clone(), deadline));
        self.store.data_mut().host_bytes = self.budgets.fuel;
        self.store.data_mut().failure = None;
        self.memory
            .write(&mut self.store, 0, input)
            .map_err(|_| Error::ResourceLimit)?;
        let mut remaining = self.budgets.fuel;
        let chunk = remaining.min(10000);
        remaining -= chunk;
        self.store
            .set_fuel(chunk)
            .map_err(|_| Error::IsolationUnavailable)?;
        let mut call = self
            .call
            .call_resumable(
                &mut self.store,
                (
                    header.method as i32,
                    0,
                    input.len() as i32,
                    header.resource as i64,
                ),
            )
            .map_err(|_| self.store.data().failure.unwrap_or(Error::Trap))?;
        let packed = loop {
            cancel.check(deadline)?;
            match call {
                TypedResumableCall::Finished(value) => break value as u64,
                TypedResumableCall::HostTrap(_) => {
                    return Err(self.store.data().failure.unwrap_or(Error::Trap));
                }
                TypedResumableCall::OutOfFuel(invocation) => {
                    let required = invocation.required_fuel();
                    let unspent = self
                        .store
                        .get_fuel()
                        .map_err(|_| Error::IsolationUnavailable)?;
                    remaining = remaining.checked_add(unspent).ok_or(Error::ResourceLimit)?;
                    if remaining < required || remaining == 0 {
                        return Err(Error::ResourceLimit);
                    }
                    let chunk = remaining.min(required.max(10000));
                    remaining -= chunk;
                    self.store
                        .set_fuel(chunk)
                        .map_err(|_| Error::IsolationUnavailable)?;
                    call = invocation
                        .resume(&mut self.store)
                        .map_err(|_| self.store.data().failure.unwrap_or(Error::Trap))?;
                }
            }
        };
        let offset = (packed >> 32) as usize;
        let length = (packed & 0xffff_ffff) as usize;
        if length > self.budgets.payload_bytes - self.store.data().callback_bytes {
            return Err(Error::ResourceLimit);
        }
        let end = offset.checked_add(length).ok_or(Error::ProtocolMismatch)?;
        let data = self
            .memory
            .data(&self.store)
            .get(offset..end)
            .ok_or(Error::ProtocolMismatch)?;
        Ok(data.to_vec())
    }
}
