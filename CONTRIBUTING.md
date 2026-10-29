# Contributing to paddock

Thank you for your interest. This file says what a pull request must have before review. A pull request that does not meet these rules is closed without review.

## Scope

paddock supports Omarchy: Arch Linux with Hyprland. Changes for other distributions or compositors are welcome, but you maintain them: the maintainers do not test them on each release. See "Support scope" in the README.

paddock is a security tool. Some changes let a sandbox reach more of the host: a file, a socket, a network path, or an environment variable. Such a change needs a written reason and a check in `src/probe.sh` that proves the new limit.

## Before you open a pull request

1. Run the full test script, and make sure that it passes:

   ```sh
   bin/test
   ```

   It runs `cargo fmt --check`, `cargo clippy -D warnings`, all tests, the plugin manifest check, and the escape probe on each backend that your machine has.

2. Run the escape probe with network access, and make sure that every check passes:

   ```sh
   paddock doctor --escape --network egress --online
   ```

   If your change touches displays or apps, also run `paddock doctor --escape --gui` and `paddock doctor --escape --app`.

3. If you changed the bar plugin, run `omarchy plugin validate .`, and test the panel in a running Omarchy shell.

4. If you changed behavior that users see, update `README.md`. Write in plain English: short sentences, active voice.

## The pull request

Fill in every part of the pull request template:

- What changed, and why.
- The distribution, its version, the compositor and its version, where you ran the tests.
- The output of `paddock doctor`.
- The summary lines of `bin/test` and of `paddock doctor --escape`.

Keep one change per pull request. Do not commit build output, local configuration, or files with your user name, home path or host name: a test fails on them.

## Reports

To report a bug, use the issue form. It asks for the output of `paddock doctor`. To report a security problem, do not open a public issue. Follow [SECURITY.md](SECURITY.md).
