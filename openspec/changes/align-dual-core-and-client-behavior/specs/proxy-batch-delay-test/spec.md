## MODIFIED Requirements

### Requirement: One-key batch delay test
The Proxies TUI view SHALL expose a one-key operation that starts delay tests for all currently known testable leaf proxy nodes, resolving each target by core-supported provider identity.

#### Scenario: Start batch delay test
- **WHEN** the user presses the documented shortcut
- **THEN** the TUI schedules delay tests for every deduplicated, unambiguous testable leaf proxy and displays progress

#### Scenario: Empty testable set
- **WHEN** no testable leaf proxy nodes exist
- **THEN** the TUI displays an informative status and creates no delay requests

### Requirement: Batch target filtering
The operation SHALL exclude policy pseudo-nodes and nested proxy-group names while retaining each real leaf proxy once in stable order; duplicate display names SHALL remain separate only when provenance is representable.

#### Scenario: Mixed proxy groups
- **WHEN** groups contain duplicate leaves, policy pseudo-nodes, and nested group references
- **THEN** each real leaf/provenance identity appears once and `DIRECT`, `REJECT`, `REJECT-DROP`, `PASS`, `COMPATIBLE`, and group names are excluded

#### Scenario: Ambiguous provider identity
- **WHEN** duplicate display names cannot be represented by the available API
- **THEN** the ambiguous target is rejected with an informative status and healthy uniquely identified targets remain eligible

### Requirement: Bounded batch execution
The TUI SHALL run no more than four batch delay requests concurrently, prevent duplicate runs, and use the delay operation's capability-gated deadline.

#### Scenario: Batch in progress
- **WHEN** a batch is already running and the shortcut is invoked again
- **THEN** another batch does not start and current completed/total counts are shown

#### Scenario: Node result
- **WHEN** an individual request succeeds or fails
- **THEN** the node receives the same delay/failure state as an individual test and progress advances without blocking other nodes
