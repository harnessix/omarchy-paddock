# Devices in /dev/paddock

The user can turn on host devices. Each device is a file in `/dev/paddock` (on the srt backend: in `$PADDOCK_DEVICES`). `paddock-host device list` prints the catalog `/dev/paddock/.catalog`: each device, its request format and whether it is on. A device that shows as `off` does not work. Only the user can turn it on, on the host.

## Send a request

`paddock-host device NAME [REQUEST]` sends one request, prints the response and exits with a clear code. `-` as REQUEST reads it from standard input.

Without `paddock-host`, one open is one request: open the device read-write, write the request, then read the response.

```sh
exec 3<>/dev/paddock/echo; printf 'hello' >&3; cat <&3; exec 3<&-
```

The user can define two kinds of devices. The catalog shows the format of each. The first request of a sandbox asks the user.

- `http`: a loopback service. The request is `METHOD PATH`, header lines, an empty line, then the body: `paddock-host device ollama "GET /api/tags"`.
- `command`: the request is the standard input of a fixed host command.

A paid device has a budget. EDQUOT from a plain read means that the budget is used up: tell the user, who raises it on the host.

## Wait and ask

- `wait`: `paddock-host device wait 2026-10-30T15:00:00Z --timeout 100` returns at that time. Give an RFC 3339 time, not a duration, so that every read waits for the same moment. `paddock-host device wait 30m` names the time to use in its message.
- `ask`: `paddock-host device ask "QUESTION" --choice yes --choice no --timeout 100` shows the user a question and prints the answer. Exit code 3: the user dismissed it. Ask only when you need a decision. A plain question in the conversation is often better.

Exit code 75: the timeout passed first. Send the same command again. For a question, paddock finds the open question, so the user sees one card. A plain read of a waiting device fails with `ETIME` ("Timer expired") instead.

`/dev/paddock/gw` is the gateway that `paddock-host` uses. Do not use it directly. Credential files in `/dev/paddock/secret/` are described in `SKILL.md`, under "Secrets".

`paddock-host device --help` gives the exit codes.
