use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitKind {
    Call,
    Join,
    Race,
    Event,
    Sleep,
    AskUser,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WaitHandle {
    pub workflow_id: String,
    pub sequence: u64,
    pub generation: u64,
    pub kind: WaitKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowErrorKind {
    EncodePayload,
    DecodeOutput,
    Runtime,
    Cancelled,
    Timeout,
    Zombie,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowError {
    pub kind: WorkflowErrorKind,
    pub message: String,
}

impl WorkflowError {
    fn new(kind: WorkflowErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl Display for WorkflowError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for WorkflowError {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallSpec {
    pub plugin_path: String,
    pub node_id: String,
    #[serde(default)]
    pub payload: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl CallSpec {
    pub fn try_new<P: Serialize>(
        plugin_path: impl Into<String>,
        node_id: impl Into<String>,
        payload: P,
    ) -> Result<Self, WorkflowError> {
        let payload = serde_json::to_value(payload).map_err(|err| {
            WorkflowError::new(
                WorkflowErrorKind::EncodePayload,
                format!("call payload serialization failed: {err}"),
            )
        })?;
        Ok(Self {
            plugin_path: plugin_path.into(),
            node_id: node_id.into(),
            payload,
            timeout_ms: None,
        })
    }

    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = Some(timeout_ms);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JoinPolicy {
    All,
    AtLeast(usize),
    FirstSuccess,
    FirstCompleted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JoinSpec {
    pub policy: JoinPolicy,
    pub calls: Vec<CallSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl JoinSpec {
    pub fn new(policy: JoinPolicy, calls: Vec<CallSpec>) -> Self {
        Self {
            policy,
            calls,
            timeout_ms: None,
        }
    }

    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = Some(timeout_ms);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RacePolicy {
    FirstCompleted,
    FirstSuccess,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RaceSpec {
    pub policy: RacePolicy,
    pub calls: Vec<CallSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl RaceSpec {
    pub fn new(policy: RacePolicy, calls: Vec<CallSpec>) -> Self {
        Self {
            policy,
            calls,
            timeout_ms: None,
        }
    }

    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = Some(timeout_ms);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventSpec {
    pub topic: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl EventSpec {
    pub fn new(topic: impl Into<String>) -> Self {
        Self {
            topic: topic.into(),
            key: None,
            timeout_ms: None,
        }
    }

    pub fn with_key(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }

    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = Some(timeout_ms);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SleepSpec {
    pub duration_ms: u64,
}

impl SleepSpec {
    pub fn new(duration_ms: u64) -> Self {
        Self { duration_ms }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AskUserSpec {
    pub prompt: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub context: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl AskUserSpec {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            context: BTreeMap::new(),
            timeout_ms: None,
        }
    }

    pub fn with_context_field<V: Serialize>(
        mut self,
        key: impl Into<String>,
        value: V,
    ) -> Result<Self, WorkflowError> {
        let value = serde_json::to_value(value).map_err(|err| {
            WorkflowError::new(
                WorkflowErrorKind::EncodePayload,
                format!("ask_user context serialization failed: {err}"),
            )
        })?;
        self.context.insert(key.into(), value);
        Ok(self)
    }

    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = Some(timeout_ms);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitSpec {
    Call(CallSpec),
    Join(JoinSpec),
    Race(RaceSpec),
    Event(EventSpec),
    Sleep(SleepSpec),
    AskUser(AskUserSpec),
}

impl WaitSpec {
    pub fn kind(&self) -> WaitKind {
        match self {
            WaitSpec::Call(_) => WaitKind::Call,
            WaitSpec::Join(_) => WaitKind::Join,
            WaitSpec::Race(_) => WaitKind::Race,
            WaitSpec::Event(_) => WaitKind::Event,
            WaitSpec::Sleep(_) => WaitKind::Sleep,
            WaitSpec::AskUser(_) => WaitKind::AskUser,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WaitOutcome {
    Value { payload: Value },
    Error { message: String },
    Cancelled { reason: String },
    TimedOut { timeout_ms: Option<u64> },
    Zombie { reason: String },
}

pub trait WorkflowRuntime {
    fn submit_wait(&mut self, spec: WaitSpec) -> WaitHandle;
    fn poll_wait(&mut self, handle: &WaitHandle, cx: &mut Context<'_>) -> Poll<WaitOutcome>;
    fn cancel_wait(&mut self, handle: &WaitHandle) -> Result<(), WorkflowError>;
}

pub struct WorkflowSession<'rt> {
    runtime: &'rt mut dyn WorkflowRuntime,
}

impl<'rt> WorkflowSession<'rt> {
    pub fn new(runtime: &'rt mut dyn WorkflowRuntime) -> Self {
        Self { runtime }
    }

    pub fn call<'a, T>(&'a mut self, spec: CallSpec) -> WaitFuture<'a, T>
    where
        T: DeserializeOwned,
    {
        WaitFuture::new(&mut *self.runtime, WaitSpec::Call(spec))
    }

    pub fn join<'a, T>(&'a mut self, spec: JoinSpec) -> WaitFuture<'a, T>
    where
        T: DeserializeOwned,
    {
        WaitFuture::new(&mut *self.runtime, WaitSpec::Join(spec))
    }

    pub fn race<'a, T>(&'a mut self, spec: RaceSpec) -> WaitFuture<'a, T>
    where
        T: DeserializeOwned,
    {
        WaitFuture::new(&mut *self.runtime, WaitSpec::Race(spec))
    }

    pub fn wait_event<'a, T>(&'a mut self, spec: EventSpec) -> WaitFuture<'a, T>
    where
        T: DeserializeOwned,
    {
        WaitFuture::new(&mut *self.runtime, WaitSpec::Event(spec))
    }

    pub fn sleep<'a>(&'a mut self, duration_ms: u64) -> WaitFuture<'a, ()> {
        WaitFuture::new(
            &mut *self.runtime,
            WaitSpec::Sleep(SleepSpec::new(duration_ms)),
        )
    }

    pub fn ask_user<'a, T>(&'a mut self, spec: AskUserSpec) -> WaitFuture<'a, T>
    where
        T: DeserializeOwned,
    {
        WaitFuture::new(&mut *self.runtime, WaitSpec::AskUser(spec))
    }
}

pub fn session(runtime: &mut dyn WorkflowRuntime) -> WorkflowSession<'_> {
    WorkflowSession::new(runtime)
}

pub struct WaitFuture<'rt, T> {
    runtime: &'rt mut dyn WorkflowRuntime,
    spec: Option<WaitSpec>,
    handle: Option<WaitHandle>,
    _marker: PhantomData<T>,
}

impl<'rt, T> WaitFuture<'rt, T> {
    fn new(runtime: &'rt mut dyn WorkflowRuntime, spec: WaitSpec) -> Self {
        Self {
            runtime,
            spec: Some(spec),
            handle: None,
            _marker: PhantomData,
        }
    }
}

impl<'rt, T> Unpin for WaitFuture<'rt, T> {}

impl<T> Future for WaitFuture<'_, T>
where
    T: DeserializeOwned,
{
    type Output = Result<T, WorkflowError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.handle.is_none() {
            let spec = self
                .spec
                .take()
                .expect("wait spec must exist before submit");
            self.handle = Some(self.runtime.submit_wait(spec));
        }

        let handle = self
            .handle
            .clone()
            .expect("wait handle must exist after submit");
        match self.runtime.poll_wait(&handle, cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(outcome) => {
                self.handle = None;
                Poll::Ready(decode_outcome(outcome))
            }
        }
    }
}

impl<T> Drop for WaitFuture<'_, T> {
    fn drop(&mut self) {
        if let Some(handle) = &self.handle {
            let _ = self.runtime.cancel_wait(handle);
        }
    }
}

fn decode_outcome<T: DeserializeOwned>(outcome: WaitOutcome) -> Result<T, WorkflowError> {
    match outcome {
        WaitOutcome::Value { payload } => serde_json::from_value(payload).map_err(|err| {
            WorkflowError::new(
                WorkflowErrorKind::DecodeOutput,
                format!("workflow output decode failed: {err}"),
            )
        }),
        WaitOutcome::Error { message } => {
            Err(WorkflowError::new(WorkflowErrorKind::Runtime, message))
        }
        WaitOutcome::Cancelled { reason } => {
            Err(WorkflowError::new(WorkflowErrorKind::Cancelled, reason))
        }
        WaitOutcome::TimedOut { timeout_ms } => Err(WorkflowError::new(
            WorkflowErrorKind::Timeout,
            match timeout_ms {
                Some(value) => format!("wait timed out after {value}ms"),
                None => "wait timed out".to_string(),
            },
        )),
        WaitOutcome::Zombie { reason } => {
            Err(WorkflowError::new(WorkflowErrorKind::Zombie, reason))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{RawWaker, RawWakerVTable, Waker};

    #[derive(Debug, Clone, PartialEq, Deserialize)]
    struct NumberValue {
        value: f64,
    }

    #[derive(Debug, Clone)]
    struct ReadyRuntime {
        next_sequence: u64,
        outcome: WaitOutcome,
        submitted: Vec<WaitSpec>,
        cancel_count: usize,
    }

    impl ReadyRuntime {
        fn new(outcome: WaitOutcome) -> Self {
            Self {
                next_sequence: 0,
                outcome,
                submitted: Vec::new(),
                cancel_count: 0,
            }
        }
    }

    impl WorkflowRuntime for ReadyRuntime {
        fn submit_wait(&mut self, spec: WaitSpec) -> WaitHandle {
            self.next_sequence += 1;
            let handle = WaitHandle {
                workflow_id: "wf_test".to_string(),
                sequence: self.next_sequence,
                generation: 1,
                kind: spec.kind(),
            };
            self.submitted.push(spec);
            handle
        }

        fn poll_wait(&mut self, _handle: &WaitHandle, _cx: &mut Context<'_>) -> Poll<WaitOutcome> {
            Poll::Ready(self.outcome.clone())
        }

        fn cancel_wait(&mut self, _handle: &WaitHandle) -> Result<(), WorkflowError> {
            self.cancel_count += 1;
            Ok(())
        }
    }

    #[test]
    fn call_future_decodes_typed_output() {
        let mut runtime = ReadyRuntime::new(WaitOutcome::Value {
            payload: serde_json::json!({ "value": 7.0 }),
        });
        {
            let mut wf = session(&mut runtime);
            let spec = CallSpec::try_new(
                "expr",
                "expr_entry",
                serde_json::json!({ "expression": "1 + 2 * 2" }),
            )
            .expect("call spec");
            let mut fut = wf.call::<NumberValue>(spec);
            let value = poll_once(&mut fut).expect("typed decode should succeed");
            assert_eq!(value.value, 7.0);
        }
        assert_eq!(runtime.submitted.len(), 1);
        assert_eq!(runtime.cancel_count, 0);
        match &runtime.submitted[0] {
            WaitSpec::Call(spec) => {
                assert_eq!(spec.plugin_path, "expr");
                assert_eq!(spec.node_id, "expr_entry");
            }
            other => panic!("unexpected wait kind: {other:?}"),
        }
    }

    #[test]
    fn timed_out_outcome_becomes_timeout_error() {
        let mut runtime = ReadyRuntime::new(WaitOutcome::TimedOut {
            timeout_ms: Some(5000),
        });
        let err = {
            let mut wf = session(&mut runtime);
            let spec =
                CallSpec::try_new("expr", "expr_entry", serde_json::json!({})).expect("spec");
            let mut fut = wf.call::<NumberValue>(spec);
            poll_once(&mut fut).expect_err("timeout must map to error")
        };
        assert_eq!(err.kind, WorkflowErrorKind::Timeout);
        assert!(err.message.contains("5000"));
    }

    fn poll_once<F>(future: &mut F) -> F::Output
    where
        F: Future + Unpin,
    {
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        match Pin::new(future).poll(&mut cx) {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("ready runtime should not return pending"),
        }
    }

    fn noop_waker() -> Waker {
        // SAFETY: no-op vtable is sufficient for unit tests that manually poll a ready future.
        unsafe { Waker::from_raw(noop_raw_waker()) }
    }

    fn noop_raw_waker() -> RawWaker {
        fn clone(_: *const ()) -> RawWaker {
            noop_raw_waker()
        }
        fn wake(_: *const ()) {}
        fn wake_by_ref(_: *const ()) {}
        fn drop(_: *const ()) {}
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
        RawWaker::new(std::ptr::null(), &VTABLE)
    }
}
