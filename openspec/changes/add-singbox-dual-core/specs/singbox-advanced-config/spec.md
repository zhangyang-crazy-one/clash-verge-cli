## ADDED Requirements

### Requirement: Structured DNS configuration
The TUI SHALL provide a structured editor for sing-box DNS configuration covering multiple DNS servers (plain, DoH, DoQ, DoT, local), DNS routing rules by domain/IP to specific servers, and fakeip settings.

#### Scenario: Configure split DNS
- **WHEN** the user adds a DNS rule routing a domain suffix to a specific server
- **THEN** the generated sing-box config SHALL contain the corresponding dns.servers and dns.rules entries

#### Scenario: Fakeip toggle
- **WHEN** the user enables fakeip with a CIDR range
- **THEN** the generated config SHALL contain the fakeip server and matching dns rules

### Requirement: TUN configuration options
The TUI SHALL expose sing-box TUN options including stack selection (gVisor/system/mixed), auto_route, strict_route, and MTU in the structured editor.

#### Scenario: Select TUN stack
- **WHEN** the user selects the gVisor stack and enables auto_route
- **THEN** the generated tun inbound SHALL carry those exact values

### Requirement: Inbound configuration
The TUI SHALL allow configuring the mixed inbound listen address/port and toggling the tun inbound on or off.

#### Scenario: Change mixed port
- **WHEN** the user changes the mixed inbound port
- **THEN** the regenerated config and system proxy settings SHALL use the new port

### Requirement: Raw JSON editing mode
Every configuration domain (routing, DNS, TUN, inbounds) SHALL offer a raw JSON editing mode backed by the existing profile editor; fields not covered by structured forms SHALL be preserved verbatim through form edits.

#### Scenario: Form edit preserves unknown fields
- **WHEN** a config contains fields outside the structured form and the user edits via the form
- **THEN** the unknown fields SHALL remain unchanged in the saved config

#### Scenario: Invalid JSON rejected
- **WHEN** the user saves raw JSON that fails schema validation
- **THEN** the save SHALL be blocked and the validation error displayed
