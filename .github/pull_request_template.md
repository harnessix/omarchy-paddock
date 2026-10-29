## What and why

<!-- What this pull request changes, and the reason. One change per pull request. -->

## Where it was tested

- Distribution and version:
- Compositor and version:
- Backends tested (bwrap, docker, podman, srt):

## `paddock doctor`

```text
paste the full output here
```

## Test results

```text
paste the last lines of bin/test here
```

```text
paste the summary of: paddock doctor --escape --network egress --online
```

## Checklist

- [ ] `bin/test` passes.
- [ ] `paddock doctor --escape` passes on every backend I changed.
- [ ] If a sandbox can now reach more of the host, the reason is above and `src/probe.sh` checks the new limit.
- [ ] `README.md` is up to date for behavior that users see.
- [ ] I read [CONTRIBUTING.md](../CONTRIBUTING.md).
