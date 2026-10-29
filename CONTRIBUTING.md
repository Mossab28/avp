# Contributing to AVP

Issues and pull requests are welcome. AVP is delivered through AVP: a pull request is fine, it
becomes a task on the line once it is accepted.

Before you open one:

- `cargo test --release` passes, and `tests/git-bridge-roundtrip.sh <scratch dir>` if you touch
  the Git bridge.
- A change a real run taught us goes in `docs/RUNS.md`: the symptom, the cause, the fix.

## Contributor license agreement

AVP is published under the Elastic License 2.0 by IntrudR, which may also offer it under other
terms (a commercial license, AVP Cloud). To keep that possible, every contribution
comes with this agreement. Opening a pull request means you accept it; say so once in the pull
request ("I accept the CLA in CONTRIBUTING.md").

1. You wrote the contribution, or you have the right to submit it, and your employer, if it has
   rights to it, agrees.
2. You keep the copyright of your contribution. You grant IntrudR, and those who receive AVP
   from it, a perpetual, worldwide, non-exclusive, royalty-free, irrevocable license to use,
   copy, modify, publish, sublicense and distribute it, under the Elastic License 2.0 or any other license,
   including a commercial one.
3. You grant the same people a patent license for any patent of yours that your contribution
   uses, on the same terms.
4. Your contribution comes as is, without warranty.
