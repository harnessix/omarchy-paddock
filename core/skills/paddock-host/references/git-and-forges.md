# Push and pull requests from a sandbox

The sandbox has no git credentials and no `gh` login. Do not look for a token, and do not ask the user for one. Plain `git push` and `gh` work anyway: the host does the work with the user's credentials.

1. Commit, and run the project's checks. Hooks never run on the host.
2. Push a new branch: `git push -u origin BRANCH`. Set `PADDOCK_PUSH_REASON="..."` to tell the user why.
3. Open the pull request: `gh pr create --title "..." --body "..."`. For a long body, write it to a file and use `--body-file FILE`.
4. Watch CI: `gh pr checks --watch` or `gh run watch`.

What to do differently in the sandbox:

- Push to a new branch. A push to the default branch, a force push or a branch delete asks the user every time.
- Do not add remotes. The user sees a remote that the sandbox added as new, and it asks every time.
- Do not use `--force-with-lease`, `--dry-run`, `--atomic` or push options. They do not work.
- Push the branch before `gh pr view`, `pr checks`, `pr diff` or `pr comment`: they use the sandbox's branch.
- Reads run at once. `pr create` and the comment commands wait for the user.
- Use only the commands that `paddock-host help forge` lists. paddock refuses the others, for example `gh api` and `gh auth`.
- `tea` (Gitea, Forgejo) and `glab` (GitLab) work the same way when the user has them on the host.

`paddock-host git push` does the same as plain `git push` and exits with the codes of `paddock-host --help` (3: denied). Plain `git push` exits with 1 when the host refuses. After a refusal, do not push again. Ask in the conversation what the user wants instead.

`paddock-host help git-push` and `paddock-host help forge` give the details.
