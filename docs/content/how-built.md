# How this was built

postbor was written entirely by Claude, Anthropic's AI model, working through Claude Code: the Rust code, the SQL definitions, the tests, the fuzz targets, the specification and this documentation.

A human set the goals and made the design decisions: what the type should do, how it compares to `jsonb`, and the architecture. The choice of Rust with pgrx rather than C was theirs, made for memory safety on untrusted input. They directed the work and reviewed it at the level of design and tests, not line by line. They are not a Rust developer.

So no human has read most of the code. What stands behind it is mechanical:

- **No `unsafe` code in the core.** All parsing of user bytes happens in a crate that forbids `unsafe` and has no dependencies, so a parsing bug is a Rust error or panic (turned into an SQL error), not memory corruption. `unsafe` code exists only in the extension's calls to PostgreSQL's C interfaces.
- **Fuzzing.** Fuzz targets feed arbitrary bytes and generated values to the parser, the EDN reader, packing, editing and jsonpath, and check properties such as "reading a value back gives the same value" and "the sort order agrees with equality".
- **Miri** runs the core tests while checking for undefined behaviour.
- **SQL regression suites** on PostgreSQL 15, 16, 17 and 18, including PostgreSQL's own jsonpath tests run on both `jsonb` and `cbor`.
- **Cross-checks** against other implementations: Python's `cbor2` and JavaScript's cbor-x.

These are run locally during development. A CI configuration runs them on every change, but the repository is not yet published, so it has not run there yet.

Judge it accordingly. The tests cover what was thought of; review by someone who knows Rust and PostgreSQL internals would cover what was not. Reports of bugs, and especially of anything a crafted input can do to a server, are welcome.
