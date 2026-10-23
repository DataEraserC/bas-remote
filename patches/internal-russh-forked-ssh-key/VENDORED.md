# Why this crate is vendored

`russh` depends on `internal-russh-forked-ssh-key` for SSH encoding, and that
crate's `Mpint::try_from` rejects a redundant leading zero with
`Error::FormatEncoding`. SAP's dev-tunnel SSH server pads the
`ecdsa-sha2-nistp521` signature integers with such a zero, so every handshake
failed before the dev channel opened. `src/mpint.rs` now strips a zero that is
provably redundant instead of rejecting it. OpenSSH accepts the same input.

The copy is otherwise upstream 0.6.11+upstream-0.6.7 verbatim, with two
deliberate differences:

- `src/mpint.rs` — the change above, plus its unit test.
- `tests/` — removed, and the nine `[[test]]` targets with it. Upstream's test
  fixtures include plain SSH private keys, which a public repository cannot
  carry. The parser change does not need them: the regression tests that pair
  with it live in `crates/bas-core/tests/ssh_key_mpint.rs`.

Refresh by copying the registry source of a newer version over this directory
and re-applying the `mpint.rs` hunk.
