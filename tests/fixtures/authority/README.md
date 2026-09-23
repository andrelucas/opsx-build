# Authority regression cases

These cases capture two generated-plan mistakes: taking ownership of a consumed
dependency and inverting conditional idempotency. The contexts and contracts are
supplied inputs; the agenda text is deliberately wrong. `expected_finding` is
the reviewer rubric, not text to give the model as part of its assignment.

The unit tests exercise both cases through proposal and all verification prompt
builders. They check that the runner preserves the supplied inputs, keeps the
exact assignment, applies the common authority rule, and permits plan/spec
findings to request repair. They do not prove that a model will detect a semantic
contradiction. For a model evaluation, supply the context, contract, and agenda
with the stage prompt, then assess the response against `expected_status` and
`expected_finding`. In particular, success must retain the conditions in PHY-01;
unconditional duplicate success is also incorrect.
