# Security policy

`graphy-rs` is experimental and has no supported stable release yet. Do not expose the HTTP service directly to untrusted networks without an authentication, authorization, rate-limiting, and resource-control boundary.

Please report suspected vulnerabilities privately through GitHub's security-advisory feature for the repository. Include the affected commit, reproduction steps, impact, and any suggested mitigation. Avoid opening a public issue until the report has been assessed.

Security-relevant defaults include deny-by-default outbound network access, request deadlines, a read-only service mode, parser depth and validation checks, and checksummed segment/WAL data. These controls do not replace deployment-level isolation.
