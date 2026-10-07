## ADDED Requirements

### Requirement: Pinned offline core policy
The client MUST use an explicit compatibility policy with candidate managed targets mihomo `1.19.32` and sing-box `1.14.2`, compare versions offline, prefer compatible system binaries read-only, reuse only validated cached binaries offline, and MUST NOT silently downgrade. GitHub latest MUST be informational rather than unconditional launch selection; a higher version MUST still satisfy the tested allowed range.

#### Scenario: compatible cached binary selected offline
- **WHEN** network access is unavailable and a cached executable has a verified digest and an allowed version
- **THEN** the policy selects that executable without downloading or mutating a system binary

#### Scenario: unsupported or downgrade candidate rejected
- **WHEN** a candidate is below the pinned policy or cannot be compared unambiguously
- **THEN** selection fails with the core name, observed version, and required policy version

#### Scenario: release API unavailable with newer valid cache
- **WHEN** the release API is unavailable and the existing validated cache is newer than a compile-time fallback but still allowed by policy
- **THEN** the cache is reused without a fallback download or replacement

#### Scenario: latest exceeds supported range
- **WHEN** GitHub announces a stable version outside the tested allowed range
- **THEN** the client may display the update but MUST NOT install or launch it automatically

### Requirement: Real version-output parsing
The client MUST parse the actual `sing-box version` output, including supported official name/version forms, and MUST reject malformed, partial, or conflicting output.

#### Scenario: version output parsed
- **WHEN** an executable reports a complete semantic version such as `sing-box version 1.13.12`
- **THEN** the parser returns the core identity and semantic version for offline policy comparison

#### Scenario: malformed output rejected
- **WHEN** output omits a version, contains conflicting versions, or exits unsuccessfully
- **THEN** the parser returns a diagnostic and the executable is not eligible for selection

### Requirement: Atomic managed binary acquisition
Managed downloads MUST be staged under a cross-process lock, verified for integrity and executable permission, atomically renamed only after validation, and cleaned up on every failure.

#### Scenario: verified download committed
- **WHEN** a staged download matches its expected integrity metadata and executable checks pass
- **THEN** the client atomically installs it and records the validated version

#### Scenario: interrupted or invalid download rolled back
- **WHEN** transfer, integrity, permission, or final validation fails
- **THEN** temporary files are removed, the prior validated binary remains selected, and no downgrade fallback occurs

#### Scenario: verification metadata unavailable
- **WHEN** the installer cannot obtain trusted integrity metadata for a candidate
- **THEN** it preserves the existing executable and returns an actionable acquisition error instead of accepting an unchecked replacement

### Requirement: No live-core mutation during compatibility checks
Version comparison, executable validation, and fixture tests MUST NOT start, stop, signal, switch, or contact the user's running core. Developer verification MUST isolate temporary paths and mock endpoints, exclude real-core spawn tests, and avoid modifying installed managed binaries. Pure version subcommands used by normal product discovery are distinct from launching a core; developer tests use captured output fixtures.

#### Scenario: offline policy test
- **WHEN** a policy test evaluates fixture output and metadata
- **THEN** it uses mocks/files only and leaves processes, sockets, configs, and pidfiles untouched
