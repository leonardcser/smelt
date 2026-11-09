# Contributing

smelt is open source under the [MIT license](LICENSE). Issues and pull requests
are welcome. A person reads every one of them, so please keep them short and
make them useful.

## Issues

Search the open and closed issues first. If yours is new, open an issue.

- **Bug:** give the smelt version (`smelt --version`), your OS and terminal, the
  steps to reproduce it, what you expected and what happened. Say if you run
  smelt inside tmux, zellij, screen or another layer on top of the terminal,
  and try the bug without it. Many display bugs come from this layer. A minimal
  reproduction is better than a long description.
- **Feature:** describe the problem you want to solve, not only the solution.
  Wait for a maintainer to agree before you build anything large.
- **Security:** do not open a public issue. Follow [SECURITY.md](SECURITY.md).

## Pull requests

- Fix a bug or a small problem directly. For anything larger, open an issue
  first so we can agree on the approach before you spend time on it.
- Keep each pull request to one change.
- Write the pull request title as a
  [conventional commit](https://www.conventionalcommits.org), at most 60
  characters, such as `fix(tui): keep cursor in view`. Pull requests are
  squash merged, so the title becomes the commit, and release notes are built
  from it. CI checks the title.
- Add a test only where it protects real behavior from a regression. See
  [TESTING.md](TESTING.md).
- Update the docs when behavior changes. After a Lua API change, run
  `cargo xtask gen-lua-docs` and commit the result.
- Write `smelt` in lowercase everywhere.

Before you push, run the same checks as CI:

```bash
cargo fmt
cargo clippy --workspace --all-targets --features smelt-tui/harness -- -D warnings
cargo nextest run --workspace --features smelt-tui/harness
```

[AGENTS.md](AGENTS.md) lists the build commands and code conventions.

## AI use

AI tools are welcome. Much of smelt is written with them. The rules exist
because a pull request costs a maintainer time to review, and that cost must be
worth it.

- **Say if you used AI.** One line in the pull request description is enough.
- **Understand every line.** You must be able to explain what your change does
  and how it fits the rest of the code without asking an AI. If you cannot, do
  not submit it.
- **Keep it to the point.** This includes issues and comments. AI text is often
  long and full of noise. Check the facts and cut it down to what matters.
- **Test it yourself.** Run the change and the checks above. Do not submit code
  that you have not run.

Pull requests that are clearly unreviewed AI output will be closed without
review.
