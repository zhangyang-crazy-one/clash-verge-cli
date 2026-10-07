## ADDED Requirements

### Requirement: Capability-gated operation deadlines
Health checks MUST retain a 5-second deadline, delay operations MUST use `timeout_ms` plus a bounded response margin, and provider refresh MUST use a configurable or 30-second default request deadline. A timeout MUST NOT alone classify a core as dead.

#### Scenario: delay timeout honored
- **WHEN** a delay request specifies `timeout_ms`
- **THEN** its deadline is derived from that value and the bounded margin, independently of the health deadline

#### Scenario: provider timeout diagnostic
- **WHEN** provider refresh exceeds its configured deadline
- **THEN** the client reports a provider-refresh timeout for the selected core without declaring the core dead solely from that timeout

#### Scenario: unsupported provider operation
- **WHEN** a CLI or TUI provider operation is requested for a core that lacks provider capability
- **THEN** the client reports that limitation before sending an API request and MUST NOT claim a successful empty update

### Requirement: Bounded data events and reliable lifecycle events
The TUI MUST coalesce traffic to a latest-value slot, bound logs with dropped-count reporting, and deliver control/lifecycle events without loss through backpressure or reserved capacity.

#### Scenario: traffic overload coalesced
- **WHEN** traffic updates arrive faster than the render budget
- **THEN** only the latest value is rendered and the control channel remains available

#### Scenario: lifecycle event retained
- **WHEN** a reload, ready, stop, or cancellation event is emitted during log overload
- **THEN** the event is delivered in generation order and is not dropped

#### Scenario: bounded logs report loss
- **WHEN** log production exceeds queue capacity
- **THEN** queued logs remain bounded and the user can observe a dropped-entry count while control events remain deliverable

#### Scenario: producer encounters a full control channel
- **WHEN** a control producer must wait for channel capacity
- **THEN** it MUST NOT hold a configuration or manager lock needed by the consumer while waiting

### Requirement: Dirty-state rendering
The TUI MUST avoid redrawing an unchanged static view on every timer tick while retaining input responsiveness and an explicit maximum redraw budget.

#### Scenario: unchanged static view
- **WHEN** no visible data, interaction or animation changes between timer ticks
- **THEN** the renderer skips redundant draws

#### Scenario: visible update arrives
- **WHEN** input or a visible data update arrives
- **THEN** the view is marked dirty and rendered within its configured budget

### Requirement: Cancellation and stale-generation safety
Background tasks MUST cancel on shutdown or replacement, and events from an unseen stale generation MUST NOT resurrect old state.

#### Scenario: cancelled generation ignored
- **WHEN** a delayed task emits after its generation was cancelled
- **THEN** the event is discarded and the current generation remains authoritative

### Requirement: Preserve existing lifecycle and authentication behavior
The implementation MUST retain graceful SIGTERM cleanup with a five-second kill fallback, watcher cancellation, render-error cleanup, gzip/basic empty-password authentication, TLS 1.2+, and fake-IP IPv6 behavior.

#### Scenario: shutdown remains graceful
- **WHEN** SIGTERM arrives during an operation
- **THEN** cancellation and cleanup run before the five-second fallback, without changing the established auth or network handling
