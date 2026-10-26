---
title: "Permission defaults"
description: "What each smelt mode (normal, plan, apply, yolo) allows for tools and bash commands before any of your own rules apply."
---

# Permission defaults

What each mode allows before any of your own rules apply. To change them, see
[Configuring Permissions](permissions.md#configuring-permissions).

## Tools

| Tool                  | Normal | Plan               | Apply | Yolo  |
| --------------------- | ------ | ------------------ | ----- | ----- |
| `read_file`           | Allow  | Allow              | Allow | Allow |
| `glob`                | Allow  | Allow              | Allow | Allow |
| `grep`                | Allow  | Allow              | Allow | Allow |
| `ask_user_question`   | Allow  | Allow              | Allow | Allow |
| `edit_file`           | Ask    | Deny               | Allow | Allow |
| `write_file`          | Ask    | Deny               | Allow | Allow |
| `edit_notebook`       | Ask    | Deny               | Ask   | Allow |
| `bash`                | Ask    | Allow / Ask / Deny | Ask   | Allow |
| `web_fetch`           | Ask    | Ask                | Ask   | Allow |
| `web_search`          | Ask    | Ask                | Ask   | Allow |
| `read_process_output` | Allow  | Allow              | Allow | Allow |
| `enter_worktree`      | Ask    | Ask                | Ask   | Allow |
| `switch_cwd`          | Ask    | Deny               | Ask   | Allow |
| `stop_process`        | Ask    | Deny               | Ask   | Allow |
| `load_skill`          | Ask    | Allow              | Ask   | Allow |
| `get_goal`            | Ask    | Ask                | Ask   | Allow |
| `create_goal`         | Ask    | Ask                | Ask   | Allow |
| `update_goal`         | Ask    | Ask                | Ask   | Allow |
| `clear_goal`          | Ask    | Ask                | Ask   | Allow |
| `present_plan`        | N/A    | Allow              | N/A   | N/A   |
| `smelt_reload`        | Ask    | Deny               | Ask   | Allow |

The optional LSP plugin registers read-effect semantic tools with Allow in
Normal, Plan, and Apply. `rename_symbol` has write effect, so its defaults are
Allow in Normal, Deny in Plan, Ask in Apply, and Allow in Yolo.

Plan mode is read-only: read tools stay allowed, networked and unknown tools ask,
and anything that writes, runs processes or changes config is denied without
prompting. It also adds the `present_plan` tool. Your config can extend these
defaults.

## Bash commands

Read-only commands with no side effects are allowed by default. Commands that
can modify files, install packages, or affect system state require approval.

| Pattern       | Normal | Apply | Yolo  |
| ------------- | ------ | ----- | ----- |
| `ls *`        | Allow  | Allow | Allow |
| `find *`      | Allow  | Allow | Allow |
| `tree *`      | Allow  | Allow | Allow |
| `cat *`       | Allow  | Allow | Allow |
| `head *`      | Allow  | Allow | Allow |
| `tail *`      | Allow  | Allow | Allow |
| `less *`      | Allow  | Allow | Allow |
| `grep *`      | Allow  | Allow | Allow |
| `sort *`      | Allow  | Allow | Allow |
| `uniq *`      | Allow  | Allow | Allow |
| `wc *`        | Allow  | Allow | Allow |
| `diff *`      | Allow  | Allow | Allow |
| `tr *`        | Allow  | Allow | Allow |
| `cut *`       | Allow  | Allow | Allow |
| `jq *`        | Allow  | Allow | Allow |
| `echo *`      | Allow  | Allow | Allow |
| `pwd *`       | Allow  | Allow | Allow |
| `which *`     | Allow  | Allow | Allow |
| `dirname *`   | Allow  | Allow | Allow |
| `basename *`  | Allow  | Allow | Allow |
| `realpath *`  | Allow  | Allow | Allow |
| `stat *`      | Allow  | Allow | Allow |
| `file *`      | Allow  | Allow | Allow |
| `test *`      | Allow  | Allow | Allow |
| `du *`        | Allow  | Allow | Allow |
| `df *`        | Allow  | Allow | Allow |
| `date *`      | Allow  | Allow | Allow |
| `whoami *`    | Allow  | Allow | Allow |
| `sha256sum *` | Allow  | Allow | Allow |
| `md5sum *`    | Allow  | Allow | Allow |
| `xxd *`       | Allow  | Allow | Allow |
| `hexdump *`   | Allow  | Allow | Allow |
| `strings *`   | Allow  | Allow | Allow |
| _other_       | Ask    | Ask   | Allow |

Compound commands split on shell operators (`&&`, `||`, `;`, `|`) are evaluated
per subcommand; the worst decision wins, and a single deny blocks the whole
command. `cd` is always allowed.

!!! note

    In modes whose metadata enables `ask_on_output_redirection` (Normal and Plan by default),
    otherwise-allowed bash commands that contain output redirection (`>`, `>>`, `&>`)
    are escalated to Ask. Plan mode's read-only policy then denies redirections that
    write to real files.
