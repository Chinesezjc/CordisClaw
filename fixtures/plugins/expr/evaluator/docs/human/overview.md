# expr_evaluator

Evaluation stage that computes a numeric result from the AST.

## Architecture

The evaluator plugin is a parent coordinator over operator-specific child plugins.
Existing binary operators are implemented as sibling child plugins under this subtree:

- `expr/evaluator/add`
- `expr/evaluator/sub`
- `expr/evaluator/mul`
- `expr/evaluator/div`

## Extension Pattern

When adding a new arithmetic operator, prefer creating a new sibling child plugin for the
operator and then wiring it into the evaluator parent, instead of embedding the operator's
full behavior directly into `expr/evaluator/src/core.rs`.

`expr/evaluator/src/core.rs` should usually stay focused on:

- dispatching between operator plugins
- shared orchestration and error mapping
- glue code that connects parser output to child plugin behavior

Use a direct parent-core implementation only when the change is genuinely shared across
multiple operators or is purely evaluator-level coordination.
