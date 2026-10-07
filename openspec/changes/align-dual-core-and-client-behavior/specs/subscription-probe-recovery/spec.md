## MODIFIED Requirements

### Requirement: Node-failure probing
The auto-update scheduler MUST probe the selected node and force a subscription refresh when it fails, using the selected core's capability and distinguishing controller errors from node failures.

#### Scenario: dead node triggers a forced refresh
- **WHEN** a selected core is running and the current node fails its delay test three consecutive times with a 5-second node-probe timeout and bounded HTTP response margin
- **THEN** the scheduler forces a refresh beyond interval/cooldown, reloads that core after readiness, and reapplies the selected node

#### Scenario: controller API errors are not node failures
- **WHEN** delay fails because the selected core controller API is unreachable
- **THEN** the failure counter is unchanged and no refresh is triggered

#### Scenario: forced refreshes are debounced
- **WHEN** a forced refresh completed less than five minutes ago
- **THEN** another forced refresh is not triggered

### Requirement: Fixed-exit rollback
A forced refresh MUST NOT silently change the selected exit node and MUST restore the previous valid state through the selected core lifecycle.

#### Scenario: selected node vanishes from refreshed config
- **WHEN** a forced refresh removes the selected node
- **THEN** prior profile content and `updated` timestamp are restored, the old config is reloaded and readiness confirmed, and the user is notified

#### Scenario: refresh succeeds but node still fails
- **WHEN** the selected node still fails after refresh and reload
- **THEN** the user is notified that the subscription may be down and no automatic node switching occurs

### Requirement: Probe toggle
The probe MUST be configurable by `verge.yaml` `probe_enabled`, defaulting to true when absent.

#### Scenario: probe disabled
- **WHEN** `probe_enabled` is false
- **THEN** the probe loop does not run and interval-driven updates continue

#### Scenario: probe enabled by default
- **WHEN** `probe_enabled` is absent
- **THEN** the probe loop runs for a running core
