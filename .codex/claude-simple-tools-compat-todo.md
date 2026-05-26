# Claude Simple Tools Compatibility TODO

This tracks known gaps between the Codex++ `claude_simple_tools` implementation
and `/Users/yuguorui/Code/opencode/claude-code`.

## High Priority Gaps

- `Glob` / `Grep` are implemented with recursive filesystem traversal plus
  `regex_lite` and a local glob matcher. Claude Code uses ripgrep semantics, so
  type filters, glob syntax, ignore behavior, max-column behavior, and
  performance can still differ.

## Completed Compatibility Items

- `Edit` / `Write` enforce read-before-write and stale-file guards for existing
  text files.
- `Bash.timeout` is wired as a real unified exec process timeout instead of only
  controlling the initial output yield.
- `Read` has first-version PDF support through poppler text extraction
  (`pdfinfo` / `pdftotext -layout`) with `pages` support.

## Release Checklist

- Validate each new tool with `codex++ -p dashscope-comp -m qwen3.7-max`:
  `Bash`, `Read`, `Edit`, `Write`, `Glob`, and `Grep`.
- Ensure unit test coverage for newly added code is at least 80%.
- Rebase onto the latest upstream before release.
- Force-push the branch, trigger fork-release, and monitor the release build.
