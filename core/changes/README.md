A pull request adds one file `SLUG.md` in this folder. Use lower case and dashes.
Each line starts with `- ` and a prefix `Security:`, `New:`, `Changed:`, `Fix:` or `Removed:`.
A line with no prefix is a small note. `README.md` is not a fragment.
A change under `src/`, `profiles/` or `skills/` needs a fragment here, or the label `no-changelog`.
A release runs `.github/scripts/changelog-assemble.sh`, which moves these files into `CHANGELOG.md`.
