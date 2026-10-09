## ADDED Requirements

### Requirement: Guided core updates and transactional selection
The TUI MUST perform core inspection and consent-gated acquisition asynchronously after the shell exists, render actual check/download/verify/ready/preparation/switch/result phases, and preserve the old service until binary/configuration/preflight preparation succeeds. Manager selection MUST be shared across clones and API transport, committed only after readiness, and fully rolled back on later failure. GUI/foreign ownership and unsupported subscription conversion MUST yield a persistent dismissible diagnostic without lifecycle mutation.

#### Scenario: update declined or preparation cancelled
- **WHEN** the user presses n or Esc during consent, acquisition or verified-ready
- **THEN** no unconfirmed download or lifecycle mutation occurs, owned staging is cleaned and stale queued ready events cannot apply the candidate

#### Scenario: supported Clash subscription switched
- **WHEN** an owned core is switched using a supported current Clash subscription
- **THEN** existing node/group/rule/DNS conversion prepares the target automatically, target readiness precedes durable selection/shared kind publication, and no manual JSON is required

#### Scenario: target boot or commit failure
- **WHEN** stop, target boot/readiness, config save or marker commit fails
- **THEN** prior configuration/binary/kind/selector/ownership state is restored, a previously stopped owned predecessor is restarted, surviving old pid/version remain accurate, and rollback failure is reported separately

#### Scenario: GUI or foreign owner refuses switching
- **WHEN** a GUI instance, adopted supervisor, changed live CLI record or unowned controller socket blocks a requested lifecycle apply
- **THEN** the TUI shows a dismissible explanation and neither signals nor adopts nor sends controller requests to that foreign owner

#### Scenario: stale Unix socket without a PID record
- **WHEN** a CLI-private controller socket file remains after its owner has exited and passive kernel endpoint inspection confirms it is no longer bound
- **THEN** either core can proceed through guided apply without treating the file as a live foreign controller, and any stale-file cleanup is limited to the verified CLI-private path

#### Scenario: active or unverified Unix endpoint
- **WHEN** the endpoint is actively bound without a current owned record, including when an old record names a dead PID, or passive inspection cannot establish that the path is safe
- **THEN** guided apply refuses before lifecycle mutation or unlink, preserves the endpoint and shows its path and a specific ownership or inspection diagnostic

#### Scenario: prepare a binary while GUI supplies the network
- **WHEN** the GUI is running and the user requests inspection or confirms a core download
- **THEN** the TUI can inspect and verify a CLI-owned candidate and display its actual version without changing the GUI lifecycle, and ownership is checked before applying it

#### Scenario: cancellation during commit or shutdown
- **WHEN** cancellation arrives during an awaited commit or the application exits
- **THEN** the token is checked before final publication, owned rollback completes before shutdown, and ordinary stream cancellation cannot swallow the independent operation result

#### Scenario: verified target needs TUN permission
- **WHEN** TUN is enabled and the verified candidate lacks required capabilities
- **THEN** the TUI offers explicit permission setup naming its kind, version and exact path before stopping the old core, executes privileged setup only after user confirmation/password submission, rechecks permission, and resumes the same confirmed operation rather than resolving another core

#### Scenario: TUN setup declined, failed or stale
- **WHEN** permission setup is declined, cancelled, fails verification or completes for an obsolete operation
- **THEN** no target core is started, the previous running state and selected core/TUN configuration remain accurate, and a persistent result explains the outcome

#### Scenario: Settings permission setup targets selected core
- **WHEN** the user requests TUN setup in Settings
- **THEN** setup inspects the shared selected core and its verified candidate rather than an unconditional mihomo/system fallback, shows the exact authorization target, and neither downloads without confirmation nor implicitly starts or switches a core

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
