# Compatibility debt

Compatibility code we intend to remove while smelt is alpha is marked with
`COMPAT(<id>)` and documented here.

- `tool-attachment-path-metadata`: Provider request serialization can still
  load path-only attachment metadata written by older sessions. Remove after
  pre-capture tool attachments have aged out of supported session history.
