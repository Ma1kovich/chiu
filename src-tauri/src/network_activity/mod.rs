use crate::local_log::{LocalLog, SafeEvent};
use std::{
    collections::BTreeMap,
    error::Error,
    sync::{Arc, Mutex, mpsc},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "macos")]
use macos::MacOsCounterProvider as PlatformCounterProvider;
#[cfg(target_os = "windows")]
use windows::WindowsCounterProvider as PlatformCounterProvider;

const MAX_TRUSTED_INTERVAL: Duration = Duration::from_secs(5);
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
type BoxError = Box<dyn Error + Send + Sync>;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct InterfaceId {
    numeric: u64,
    incarnation: Option<Box<[u8]>>,
}

impl InterfaceId {
    #[cfg(any(target_os = "windows", test))]
    fn numeric(value: u64) -> Self {
        Self {
            numeric: value,
            incarnation: None,
        }
    }

    fn with_incarnation(value: u64, incarnation: Box<[u8]>) -> Self {
        Self {
            numeric: value,
            incarnation: Some(incarnation),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InterfaceCounter {
    id: InterfaceId,
    received_bytes: u64,
    loopback: bool,
}

impl InterfaceCounter {
    #[cfg(test)]
    fn external(id: u64, received_bytes: u64) -> Self {
        Self {
            id: InterfaceId::numeric(id),
            received_bytes,
            loopback: false,
        }
    }

    #[cfg(test)]
    fn loopback(id: u64, received_bytes: u64) -> Self {
        Self {
            id: InterfaceId::numeric(id),
            received_bytes,
            loopback: true,
        }
    }

    #[cfg(test)]
    fn external_incarnation(id: u64, incarnation: &[u8], received_bytes: u64) -> Self {
        Self {
            id: InterfaceId::with_incarnation(id, incarnation.into()),
            received_bytes,
            loopback: false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReceiveActivitySample {
    received_bytes: u64,
    observed_over: Duration,
}

impl ReceiveActivitySample {
    pub(crate) fn try_new(received_bytes: u64, observed_over: Duration) -> Option<Self> {
        is_trusted_interval(observed_over).then_some(Self {
            received_bytes,
            observed_over,
        })
    }

    pub(crate) fn received_bytes(&self) -> u64 {
        self.received_bytes
    }

    pub(crate) fn observed_over(&self) -> Duration {
        self.observed_over
    }
}

fn is_trusted_interval(observed_over: Duration) -> bool {
    !observed_over.is_zero() && observed_over <= MAX_TRUSTED_INTERVAL
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NetworkActivityAvailability {
    Available,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NetworkActivityEvent {
    Sample(ReceiveActivitySample),
    ContinuityLost(ContinuityLossReason),
    AvailabilityChanged(NetworkActivityAvailability),
}

#[derive(Debug, Eq, PartialEq)]
enum NormalizedObservation {
    Baseline,
    Sample(ReceiveActivitySample),
    ContinuityLost(ContinuityLossReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ContinuityLossReason {
    #[cfg(test)]
    ExplicitReset,
    ProviderUnavailable,
    UntrustedInterval,
    ArithmeticOverflow,
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct NetworkInterfaceDiagnosticId(u64);

impl NetworkInterfaceDiagnosticId {
    pub(crate) const fn value(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NetworkInterfaceContinuity {
    New,
    Continuous,
    CounterReset,
    Removed,
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct NetworkInterfaceDiagnostic {
    id: NetworkInterfaceDiagnosticId,
    last_observed_receive_bytes: u64,
    recent_delta: Option<u64>,
    included_in_activity: bool,
    continuity: NetworkInterfaceContinuity,
}

impl NetworkInterfaceDiagnostic {
    pub(crate) fn id(&self) -> NetworkInterfaceDiagnosticId {
        self.id
    }

    pub(crate) fn last_observed_receive_bytes(&self) -> u64 {
        self.last_observed_receive_bytes
    }

    pub(crate) fn recent_delta(&self) -> Option<u64> {
        self.recent_delta
    }

    pub(crate) fn included_in_activity(&self) -> bool {
        self.included_in_activity
    }

    pub(crate) fn continuity(&self) -> NetworkInterfaceContinuity {
        self.continuity
    }
}

#[derive(Clone, Copy)]
struct TrackedInterface {
    diagnostic_id: NetworkInterfaceDiagnosticId,
    received_bytes: u64,
    included_in_activity: bool,
}

struct ActivityNormalizer {
    initialized: bool,
    previous: BTreeMap<InterfaceId, TrackedInterface>,
    diagnostics: Vec<NetworkInterfaceDiagnostic>,
    next_diagnostic_id: u64,
}

struct SamplingCore {
    normalizer: ActivityNormalizer,
    last_observed_at: Option<Duration>,
    availability: Option<NetworkActivityAvailability>,
}

impl SamplingCore {
    fn new() -> Self {
        Self {
            normalizer: ActivityNormalizer::new(),
            last_observed_at: None,
            availability: None,
        }
    }

    fn process(
        &mut self,
        observation: Result<Vec<InterfaceCounter>, BoxError>,
        observed_at: Duration,
    ) -> Vec<NetworkActivityEvent> {
        match observation {
            Ok(counters) => {
                let observed_over = self
                    .last_observed_at
                    .and_then(|previous| observed_at.checked_sub(previous))
                    .unwrap_or_default();
                let normalized = self.normalizer.observe(counters, observed_over);
                self.last_observed_at = Some(observed_at);

                let mut events = Vec::new();
                if self.availability != Some(NetworkActivityAvailability::Available) {
                    self.availability = Some(NetworkActivityAvailability::Available);
                    events.push(NetworkActivityEvent::AvailabilityChanged(
                        NetworkActivityAvailability::Available,
                    ));
                }
                match normalized {
                    NormalizedObservation::Baseline => {}
                    NormalizedObservation::Sample(sample) => {
                        events.push(NetworkActivityEvent::Sample(sample));
                    }
                    NormalizedObservation::ContinuityLost(reason) => {
                        events.push(NetworkActivityEvent::ContinuityLost(reason));
                    }
                }
                events
            }
            Err(_) => {
                self.normalizer.reset();
                self.last_observed_at = None;
                if self.availability == Some(NetworkActivityAvailability::Unavailable) {
                    return Vec::new();
                }
                self.availability = Some(NetworkActivityAvailability::Unavailable);
                vec![
                    NetworkActivityEvent::ContinuityLost(ContinuityLossReason::ProviderUnavailable),
                    NetworkActivityEvent::AvailabilityChanged(
                        NetworkActivityAvailability::Unavailable,
                    ),
                ]
            }
        }
    }

    #[cfg(test)]
    fn reset(&mut self) -> NetworkActivityEvent {
        self.normalizer.reset();
        self.last_observed_at = None;
        NetworkActivityEvent::ContinuityLost(ContinuityLossReason::ExplicitReset)
    }

    fn diagnostics(&self) -> Vec<NetworkInterfaceDiagnostic> {
        self.normalizer.diagnostics()
    }
}

trait CounterProvider: Send + 'static {
    fn read(&mut self) -> Result<Vec<InterfaceCounter>, BoxError>;
}

pub(crate) trait NetworkActivityConsumer: Send + 'static {
    fn consume(&mut self, event: NetworkActivityEvent);
}

impl<F> NetworkActivityConsumer for F
where
    F: FnMut(NetworkActivityEvent) + Send + 'static,
{
    fn consume(&mut self, event: NetworkActivityEvent) {
        self(event);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NetworkActivitySourceStatus {
    NotStarted,
    Starting,
    Available,
    Unavailable,
    Stopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NetworkActivityFailure {
    ProviderRead,
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct NetworkActivitySnapshot {
    status: NetworkActivitySourceStatus,
    latest_failure: Option<NetworkActivityFailure>,
    interfaces: Vec<NetworkInterfaceDiagnostic>,
}

impl NetworkActivitySnapshot {
    pub(crate) fn status(&self) -> NetworkActivitySourceStatus {
        self.status
    }

    pub(crate) fn latest_failure(&self) -> Option<NetworkActivityFailure> {
        self.latest_failure
    }

    pub(crate) fn interfaces(&self) -> &[NetworkInterfaceDiagnostic] {
        &self.interfaces
    }
}

struct SourceControl {
    sender: Option<mpsc::Sender<WorkerCommand>>,
    started: bool,
}

struct SourceShared {
    control: Mutex<SourceControl>,
    snapshot: Mutex<NetworkActivitySnapshot>,
    log: LocalLog,
}

#[derive(Clone)]
pub(crate) struct NetworkActivitySource {
    shared: Arc<SourceShared>,
}

impl NetworkActivitySource {
    pub(crate) fn new(log: LocalLog) -> Self {
        Self {
            shared: Arc::new(SourceShared {
                control: Mutex::new(SourceControl {
                    sender: None,
                    started: false,
                }),
                snapshot: Mutex::new(NetworkActivitySnapshot {
                    status: NetworkActivitySourceStatus::NotStarted,
                    latest_failure: None,
                    interfaces: Vec::new(),
                }),
                log,
            }),
        }
    }

    pub(crate) fn snapshot(&self) -> NetworkActivitySnapshot {
        self.shared
            .snapshot
            .lock()
            .expect("network activity snapshot lock poisoned")
            .clone()
    }

    #[cfg(test)]
    pub(crate) fn reset(&self) -> Result<(), NetworkActivityControlError> {
        let (acknowledge, accepted) = mpsc::channel();
        {
            let control = self
                .shared
                .control
                .lock()
                .expect("network activity control lock poisoned");
            let sender = control
                .sender
                .as_ref()
                .ok_or(NetworkActivityControlError::Unavailable)?;
            sender
                .send(WorkerCommand::Reset(acknowledge))
                .map_err(|_| NetworkActivityControlError::Unavailable)?;
        }
        accepted
            .recv()
            .map_err(|_| NetworkActivityControlError::Unavailable)
    }

    pub(crate) fn start<C>(
        &self,
        consumer: C,
    ) -> Result<NetworkActivityRuntime, NetworkActivityRuntimeError>
    where
        C: NetworkActivityConsumer,
    {
        self.start_with(PlatformCounterProvider::new(), consumer)
    }

    fn start_with<P, C>(
        &self,
        provider: P,
        consumer: C,
    ) -> Result<NetworkActivityRuntime, NetworkActivityRuntimeError>
    where
        P: CounterProvider,
        C: NetworkActivityConsumer,
    {
        let (sender, receiver) = mpsc::channel();
        {
            let mut control = self
                .shared
                .control
                .lock()
                .expect("network activity control lock poisoned");
            if control.started {
                return Err(NetworkActivityRuntimeError::AlreadyStarted);
            }
            control.started = true;
            control.sender = Some(sender);
        }
        self.set_status(NetworkActivitySourceStatus::Starting, None);

        let shared = self.shared.clone();
        let worker = thread::Builder::new()
            .name("chiu-network-activity".into())
            .spawn(move || run_worker(shared, provider, consumer, receiver))
            .map_err(|error| {
                let mut control = self
                    .shared
                    .control
                    .lock()
                    .expect("network activity control lock poisoned");
                control.started = false;
                control.sender = None;
                self.set_status(NetworkActivitySourceStatus::NotStarted, None);
                NetworkActivityRuntimeError::Spawn(error)
            })?;

        Ok(NetworkActivityRuntime {
            source: self.clone(),
            worker: Some(worker),
        })
    }

    fn set_status(
        &self,
        status: NetworkActivitySourceStatus,
        latest_failure: Option<NetworkActivityFailure>,
    ) {
        let mut snapshot = self
            .shared
            .snapshot
            .lock()
            .expect("network activity snapshot lock poisoned");
        let changed = snapshot.status != status;
        snapshot.status = status;
        snapshot.latest_failure = latest_failure;
        drop(snapshot);
        if changed {
            self.shared.log.record(SafeEvent::NetworkAvailability {
                status: source_status_name(status),
            });
        }
    }
}

pub(crate) struct NetworkActivityRuntime {
    source: NetworkActivitySource,
    worker: Option<JoinHandle<()>>,
}

impl NetworkActivityRuntime {
    pub(crate) fn shutdown(&mut self) -> Result<(), NetworkActivityRuntimeError> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        let sender = self
            .source
            .shared
            .control
            .lock()
            .expect("network activity control lock poisoned")
            .sender
            .take();
        if let Some(sender) = sender {
            let _ = sender.send(WorkerCommand::Shutdown);
        }
        worker
            .join()
            .map_err(|_| NetworkActivityRuntimeError::WorkerPanicked)
    }
}

impl Drop for NetworkActivityRuntime {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

enum WorkerCommand {
    #[cfg(test)]
    Reset(mpsc::Sender<()>),
    Shutdown,
}

fn run_worker<P, C>(
    shared: Arc<SourceShared>,
    mut provider: P,
    mut consumer: C,
    commands: mpsc::Receiver<WorkerCommand>,
) where
    P: CounterProvider,
    C: NetworkActivityConsumer,
{
    let origin = Instant::now();
    let mut sampling = SamplingCore::new();

    sample_once(&shared, &mut provider, &mut consumer, &mut sampling, origin);
    loop {
        match commands.recv_timeout(SAMPLE_INTERVAL) {
            #[cfg(test)]
            Ok(WorkerCommand::Reset(acknowledge)) => {
                let event = sampling.reset();
                shared
                    .snapshot
                    .lock()
                    .expect("network activity snapshot lock poisoned")
                    .interfaces
                    .clear();
                consumer.consume(event);
                let _ = acknowledge.send(());
            }
            Ok(WorkerCommand::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                sample_once(&shared, &mut provider, &mut consumer, &mut sampling, origin);
            }
        }
    }

    let mut snapshot = shared
        .snapshot
        .lock()
        .expect("network activity snapshot lock poisoned");
    snapshot.status = NetworkActivitySourceStatus::Stopped;
    snapshot.latest_failure = None;
}

fn sample_once<P, C>(
    shared: &SourceShared,
    provider: &mut P,
    consumer: &mut C,
    sampling: &mut SamplingCore,
    origin: Instant,
) where
    P: CounterProvider,
    C: NetworkActivityConsumer,
{
    let observation = provider.read();
    let failed = observation.is_err();
    let events = sampling.process(observation, origin.elapsed());
    let mut topology_count = None;
    {
        let mut snapshot = shared
            .snapshot
            .lock()
            .expect("network activity snapshot lock poisoned");
        if !failed {
            let diagnostics = sampling.diagnostics();
            if topology_changed(&snapshot.interfaces, &diagnostics) {
                topology_count = Some(diagnostics.len());
            }
            snapshot.interfaces = diagnostics;
        }
        for event in &events {
            if let NetworkActivityEvent::AvailabilityChanged(availability) = event {
                match availability {
                    NetworkActivityAvailability::Available => {
                        snapshot.status = NetworkActivitySourceStatus::Available;
                        snapshot.latest_failure = None;
                    }
                    NetworkActivityAvailability::Unavailable => {
                        snapshot.status = NetworkActivitySourceStatus::Unavailable;
                        snapshot.latest_failure = Some(NetworkActivityFailure::ProviderRead);
                    }
                }
            }
        }
        if failed {
            snapshot.latest_failure = Some(NetworkActivityFailure::ProviderRead);
        }
    }
    for event in &events {
        match event {
            NetworkActivityEvent::AvailabilityChanged(availability) => {
                shared.log.record(SafeEvent::NetworkAvailability {
                    status: match availability {
                        NetworkActivityAvailability::Available => "available",
                        NetworkActivityAvailability::Unavailable => "unavailable",
                    },
                });
            }
            NetworkActivityEvent::ContinuityLost(_) => {
                shared.log.record(SafeEvent::NetworkContinuityReset);
            }
            NetworkActivityEvent::Sample(_) => {}
        }
    }
    if let Some(interfaces) = topology_count {
        shared
            .log
            .record(SafeEvent::NetworkTopologyChanged { interfaces });
    }
    for event in events {
        consumer.consume(event);
    }
}

fn topology_changed(
    previous: &[NetworkInterfaceDiagnostic],
    current: &[NetworkInterfaceDiagnostic],
) -> bool {
    previous.len() != current.len()
        || previous.iter().zip(current).any(|(previous, current)| {
            previous.id != current.id
                || previous.included_in_activity != current.included_in_activity
                || previous.continuity != current.continuity
        })
}

fn source_status_name(status: NetworkActivitySourceStatus) -> &'static str {
    match status {
        NetworkActivitySourceStatus::NotStarted => "not-started",
        NetworkActivitySourceStatus::Starting => "starting",
        NetworkActivitySourceStatus::Available => "available",
        NetworkActivitySourceStatus::Unavailable => "unavailable",
        NetworkActivitySourceStatus::Stopped => "stopped",
    }
}

#[cfg(test)]
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum NetworkActivityControlError {
    Unavailable,
}

#[cfg(test)]
impl std::fmt::Display for NetworkActivityControlError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("network activity source is unavailable")
    }
}

#[cfg(test)]
impl Error for NetworkActivityControlError {}

#[derive(Debug)]
pub(crate) enum NetworkActivityRuntimeError {
    AlreadyStarted,
    Spawn(std::io::Error),
    WorkerPanicked,
}

impl std::fmt::Display for NetworkActivityRuntimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyStarted => formatter.write_str("network activity source already started"),
            Self::Spawn(error) => write!(
                formatter,
                "failed to start network activity source: {error}"
            ),
            Self::WorkerPanicked => formatter.write_str("network activity worker panicked"),
        }
    }
}

impl Error for NetworkActivityRuntimeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Spawn(error) => Some(error),
            Self::AlreadyStarted | Self::WorkerPanicked => None,
        }
    }
}

impl ActivityNormalizer {
    fn new() -> Self {
        Self {
            initialized: false,
            previous: BTreeMap::new(),
            diagnostics: Vec::new(),
            next_diagnostic_id: 1,
        }
    }

    fn observe(
        &mut self,
        counters: Vec<InterfaceCounter>,
        observed_over: Duration,
    ) -> NormalizedObservation {
        let current = counters
            .into_iter()
            .map(|counter| (counter.id, (counter.received_bytes, !counter.loopback)))
            .collect::<BTreeMap<_, _>>();

        if !self.initialized {
            self.initialized = true;
            self.rebaseline(current);
            return NormalizedObservation::Baseline;
        }

        if !is_trusted_interval(observed_over) {
            self.rebaseline(current);
            return NormalizedObservation::ContinuityLost(ContinuityLossReason::UntrustedInterval);
        }

        let mut received_bytes = 0_u64;
        let mut overflowed = false;
        let mut next = BTreeMap::new();
        let mut diagnostics = Vec::new();
        for (id, (current_value, included_in_activity)) in current {
            let (diagnostic_id, recent_delta, continuity) =
                if let Some(previous) = self.previous.get(&id).copied() {
                    match current_value.checked_sub(previous.received_bytes) {
                        Some(delta) => (
                            previous.diagnostic_id,
                            Some(delta),
                            NetworkInterfaceContinuity::Continuous,
                        ),
                        None => (
                            previous.diagnostic_id,
                            None,
                            NetworkInterfaceContinuity::CounterReset,
                        ),
                    }
                } else {
                    (
                        self.allocate_diagnostic_id(),
                        None,
                        NetworkInterfaceContinuity::New,
                    )
                };
            if included_in_activity
                && let Some(delta) = recent_delta
                && !overflowed
            {
                match received_bytes.checked_add(delta) {
                    Some(aggregate) => received_bytes = aggregate,
                    None => overflowed = true,
                }
            }
            diagnostics.push(NetworkInterfaceDiagnostic {
                id: diagnostic_id,
                last_observed_receive_bytes: current_value,
                recent_delta,
                included_in_activity,
                continuity,
            });
            next.insert(
                id,
                TrackedInterface {
                    diagnostic_id,
                    received_bytes: current_value,
                    included_in_activity,
                },
            );
        }
        for (id, previous) in &self.previous {
            if !next.contains_key(id) {
                diagnostics.push(NetworkInterfaceDiagnostic {
                    id: previous.diagnostic_id,
                    last_observed_receive_bytes: previous.received_bytes,
                    recent_delta: None,
                    included_in_activity: previous.included_in_activity,
                    continuity: NetworkInterfaceContinuity::Removed,
                });
            };
        }
        self.previous = next;
        self.diagnostics = diagnostics;

        if overflowed {
            NormalizedObservation::ContinuityLost(ContinuityLossReason::ArithmeticOverflow)
        } else {
            NormalizedObservation::Sample(
                ReceiveActivitySample::try_new(received_bytes, observed_over)
                    .expect("normalizer validated the observed interval"),
            )
        }
    }

    fn diagnostics(&self) -> Vec<NetworkInterfaceDiagnostic> {
        self.diagnostics.clone()
    }

    fn rebaseline(&mut self, current: BTreeMap<InterfaceId, (u64, bool)>) {
        let mut next = BTreeMap::new();
        let mut diagnostics = Vec::new();
        for (id, (received_bytes, included_in_activity)) in current {
            let diagnostic_id = self
                .previous
                .get(&id)
                .map(|previous| previous.diagnostic_id)
                .unwrap_or_else(|| self.allocate_diagnostic_id());
            diagnostics.push(NetworkInterfaceDiagnostic {
                id: diagnostic_id,
                last_observed_receive_bytes: received_bytes,
                recent_delta: None,
                included_in_activity,
                continuity: NetworkInterfaceContinuity::New,
            });
            next.insert(
                id,
                TrackedInterface {
                    diagnostic_id,
                    received_bytes,
                    included_in_activity,
                },
            );
        }
        for (id, previous) in &self.previous {
            if !next.contains_key(id) {
                diagnostics.push(NetworkInterfaceDiagnostic {
                    id: previous.diagnostic_id,
                    last_observed_receive_bytes: previous.received_bytes,
                    recent_delta: None,
                    included_in_activity: previous.included_in_activity,
                    continuity: NetworkInterfaceContinuity::Removed,
                });
            }
        }
        self.previous = next;
        self.diagnostics = diagnostics;
    }

    fn allocate_diagnostic_id(&mut self) -> NetworkInterfaceDiagnosticId {
        let id = NetworkInterfaceDiagnosticId(self.next_diagnostic_id);
        self.next_diagnostic_id += 1;
        id
    }

    fn reset(&mut self) {
        self.initialized = false;
        self.previous.clear();
        self.diagnostics.clear();
    }
}

#[cfg(test)]
mod tests;
