## ADDED Requirements

### Requirement: Unified route rule model
The TUI SHALL maintain a core-agnostic internal route rule model covering match fields (domain, domain_suffix, domain_keyword, ip_cidr, port, network, process, protocol, rule_set reference, logical AND/OR combination) and actions (outbound, direct, block, dns actions).

#### Scenario: Round-trip fidelity
- **WHEN** a rule set is serialized to clash YAML rules and back, or to sing-box route rules JSON and back
- **THEN** the resulting model SHALL equal the original for all expressible rules

### Requirement: Rule editing in TUI
The Rules view SHALL support creating, editing, deleting, and reordering routing rules in the active profile, with changes written to the profile's native format (clash YAML or sing-box JSON) and applied via the existing config reload path.

#### Scenario: Add a rule
- **WHEN** the user adds a rule specifying match conditions and a target outbound
- **THEN** the rule SHALL be persisted to the profile and visible after the core reloads

#### Scenario: Reorder rules
- **WHEN** the user moves a rule up or down
- **THEN** the persisted order SHALL match the displayed order, since rule order determines match priority

### Requirement: Logical rules
The rule editor SHALL support composing AND/OR logical rules from sub-rules to at least the nesting depth supported by both cores.

#### Scenario: Compose a logical rule
- **WHEN** the user builds an OR rule from two domain conditions targeting an outbound
- **THEN** it SHALL serialize correctly for both clash YAML and sing-box JSON formats

### Requirement: Raw fragment passthrough
Rules or rule fields that cannot be expressed in the unified model SHALL be preserved as raw fragments and round-tripped without loss; saving SHALL be blocked with a warning if a round-trip would lose data.

#### Scenario: Unexpressible field detected
- **WHEN** an existing profile rule contains a field outside the unified model
- **THEN** the editor SHALL show it as a raw fragment and preserve it verbatim on save

### Requirement: Rule-set management
The TUI SHALL allow adding, editing, and removing sing-box rule-set references (local .srs files and remote URLs) used by routing rules.

#### Scenario: Add remote rule-set
- **WHEN** the user adds a remote rule-set with a URL, tag, and type
- **THEN** it SHALL appear in the generated sing-box config and be selectable as a match condition in the rule editor
