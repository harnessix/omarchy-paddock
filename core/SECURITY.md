# Security policy

paddock is a sandbox. A way for an agent to read, write or reach something outside its boundary is a security problem. Examples are a host file, a socket, a host address, or an environment variable that the sandbox must not see.

## Report a problem

Do not open a public issue. Use the private form instead:

1. Go to the Security tab of this repository.
2. Click "Report a vulnerability".
3. Describe the problem. Include the output of `paddock doctor` and the steps that show the escape.

The maintainers reply in the same private report. A fix and a public advisory follow after the problem is understood.

## Supported versions

Only the latest release gets security fixes.
